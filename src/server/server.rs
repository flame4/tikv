// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::fmt;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::boxed::Box;
use std::net::{SocketAddr, IpAddr};
use std::str::FromStr;
use futures::sync::mpsc;
use futures::{Stream, Future, Sink};
use tokio_core::reactor::{Handle as CoreHandle, Remote as RemoteCore};
use mio::{Handler, EventLoop, EventLoopConfig};
use grpc::{Server as GrpcServer, ServerBuilder, Environment, ChannelBuilder};
use kvproto::tikvpb_grpc::*;
use kvproto::raft_serverpb::*;
use util::worker::{Stopped, Worker};
use util::worker::{FutureWorker, FutureRunnable};
use util::transport::SendCh;
use storage::Storage;
use raftstore::store::{SnapshotStatusMsg, SnapManager};
use raft::SnapshotStatus;
use util::collections::{HashMap, HashSet};

use super::coprocessor::{EndPointTask, EndPointHost};
use super::{Msg, ConnData};
use super::{Result, Config, Error};
use super::grpc_service::Service;
use super::transport::RaftStoreRouter;
use super::resolve::StoreAddrResolver;
use super::snap::{Task as SnapTask, Runner as SnapHandler};
use super::metrics::*;

const DEFAULT_COPROCESSOR_BATCH: usize = 50;

pub fn create_event_loop<T, S>(config: &Config) -> Result<EventLoop<Server<T, S>>>
    where T: RaftStoreRouter,
          S: StoreAddrResolver
{
    let mut loop_config = EventLoopConfig::new();
    loop_config.notify_capacity(config.notify_capacity);
    loop_config.messages_per_tick(config.messages_per_tick);
    let el = try!(EventLoop::configured(loop_config));
    Ok(el)
}

// A helper structure to bundle all senders for messages to raftstore.
pub struct ServerChannel<T: RaftStoreRouter + 'static> {
    pub raft_router: T,
    pub snapshot_status_sender: Sender<SnapshotStatusMsg>,
}

pub struct Server<T: RaftStoreRouter + 'static, S: StoreAddrResolver> {
    // Channel for sending eventloop messages.
    sendch: SendCh<Msg>,
    // Grpc server.
    env: Arc<Environment>,
    grpc_server: GrpcServer,
    local_addr: SocketAddr,
    // Addrs map for communicating with other raft stores.
    store_addrs: HashMap<u64, SocketAddr>,
    store_resolving: HashSet<u64>,
    resolver: S,
    // For dispatch raft message.
    ch: ServerChannel<T>,
    // The kv storage.
    storage: Storage,
    // For handling coprocessor requests.
    end_point_worker: Worker<EndPointTask>,
    end_point_concurrency: usize,
    // For sending/receiving snapshots.
    snap_mgr: SnapManager,
    snap_worker: Worker<SnapTask>,
    // For sending raft messages to other stores.
    raft_msg_worker: FutureWorker<SendTask>,
}

impl<T: RaftStoreRouter, S: StoreAddrResolver> Server<T, S> {
    // Create a server with already initialized engines.
    pub fn new(event_loop: &mut EventLoop<Self>,
               core: RemoteCore,
               cfg: &Config,
               storage: Storage,
               ch: ServerChannel<T>,
               resolver: S,
               snap_mgr: SnapManager)
               -> Result<Server<T, S>> {
        let sendch = SendCh::new(event_loop.channel(), "raft-server");
        let end_point_worker = Worker::new("end-point-worker");
        let snap_worker = Worker::new("snap-handler");
        let raft_msg_worker = FutureWorker::new("raft-msg-worker");

        let h = Service::new(core,
                             storage.clone(),
                             end_point_worker.scheduler(),
                             ch.raft_router.clone(),
                             snap_worker.scheduler());
        let env = Arc::new(Environment::new(1));
        let addr = try!(SocketAddr::from_str(&cfg.addr));
        let ip = format!("{}", addr.ip());
        let mut grpc_server = ServerBuilder::new(env.clone())
            .register_service(create_tikv(h))
            .bind(ip, addr.port() as u32)
            .build();
        grpc_server.start();

        let addr = {
            let (ref host, port) = grpc_server.bind_addrs()[0];
            SocketAddr::new(try!(IpAddr::from_str(host)), port as u16)
        };

        let svr = Server {
            sendch: sendch,
            env: env,
            grpc_server: grpc_server,
            local_addr: addr,
            store_addrs: HashMap::default(),
            store_resolving: HashSet::default(),
            resolver: resolver,
            ch: ch,
            storage: storage,
            end_point_worker: end_point_worker,
            end_point_concurrency: cfg.end_point_concurrency,
            snap_mgr: snap_mgr,
            snap_worker: snap_worker,
            raft_msg_worker: raft_msg_worker,
        };

        Ok(svr)
    }

    pub fn run(&mut self, event_loop: &mut EventLoop<Self>) -> Result<()> {
        let ch = self.get_sendch();
        let snap_runner = SnapHandler::new(self.snap_mgr.clone(), self.ch.raft_router.clone(), ch);
        box_try!(self.snap_worker.start(snap_runner));
        box_try!(self.raft_msg_worker.start(SendRunner::new(self.env.clone())));
        let end_point = EndPointHost::new(self.storage.get_engine(),
                                          self.end_point_worker.scheduler(),
                                          self.end_point_concurrency);
        box_try!(self.end_point_worker.start_batch(end_point, DEFAULT_COPROCESSOR_BATCH));

        info!("TiKV is ready to serve");

        try!(event_loop.run(self));
        Ok(())
    }

    pub fn get_sendch(&self) -> SendCh<Msg> {
        self.sendch.clone()
    }

    // Return listening address, this may only be used for outer test
    // to get the real address because we may use "127.0.0.1:0"
    // in test to avoid port conflict.
    pub fn listening_addr(&self) -> Result<SocketAddr> {
        Ok(self.local_addr)
    }

    fn write_data(&mut self, addr: SocketAddr, data: ConnData) {
        if let Err(e) = self.raft_msg_worker.schedule(SendTask {
            addr: addr,
            msg: data.msg,
        }) {
            error!("send raft msg err {:?}", e);
        }
    }

    fn resolve_store(&mut self, store_id: u64, data: ConnData) {
        let ch = self.sendch.clone();
        let cb = box move |r| {
            if let Err(e) = ch.send(Msg::ResolveResult {
                store_id: store_id,
                sock_addr: r,
                data: data,
            }) {
                error!("send store sock msg err {:?}", e);
            }
        };
        if let Err(e) = self.resolver.resolve(store_id, cb) {
            error!("try to resolve err {:?}", e);
        }
    }

    fn report_unreachable(&self, data: ConnData) {
        let region_id = data.msg.get_region_id();
        let to_peer_id = data.msg.get_to_peer().get_id();
        let to_store_id = data.msg.get_to_peer().get_store_id();

        if let Err(e) = self.ch.raft_router.report_unreachable(region_id, to_peer_id, to_store_id) {
            error!("report peer {} unreachable for region {} failed {:?}",
                   to_peer_id,
                   region_id,
                   e);
        }
    }

    fn send_store(&mut self, store_id: u64, data: ConnData) {
        if data.is_snapshot() {
            RESOLVE_STORE_COUNTER.with_label_values(&["snap"]).inc();
            return self.resolve_store(store_id, data);
        }

        // check the corresponding token for store.
        if let Some(addr) = self.store_addrs.get(&store_id).cloned() {
            return self.write_data(addr, data);
        }

        // No connection, try to resolve it.
        if self.store_resolving.contains(&store_id) {
            RESOLVE_STORE_COUNTER.with_label_values(&["resolving"]).inc();
            // If we are resolving the address, drop the message here.
            debug!("store {} address is being resolved, drop msg {}",
                   store_id,
                   data);
            self.report_unreachable(data);
            return;
        }

        debug!("begin to resolve store {} address", store_id);
        RESOLVE_STORE_COUNTER.with_label_values(&["store"]).inc();
        self.store_resolving.insert(store_id);
        self.resolve_store(store_id, data);
    }

    fn on_resolve_result(&mut self, store_id: u64, sock_addr: Result<SocketAddr>, data: ConnData) {
        if !data.is_snapshot() {
            // clear resolving.
            self.store_resolving.remove(&store_id);
        }

        if let Err(e) = sock_addr {
            RESOLVE_STORE_COUNTER.with_label_values(&["failed"]).inc();
            debug!("resolve store {} address failed {:?}", store_id, e);
            return self.report_unreachable(data);
        }

        RESOLVE_STORE_COUNTER.with_label_values(&["success"]).inc();
        let sock_addr = sock_addr.unwrap();
        info!("resolve store {} address ok, addr {}", store_id, sock_addr);
        self.store_addrs.insert(store_id, sock_addr);

        if data.is_snapshot() {
            return self.send_snapshot_sock(sock_addr, data);
        }

        self.write_data(sock_addr, data)
    }

    fn new_snapshot_reporter(&self, data: &ConnData) -> SnapshotReporter {
        let region_id = data.msg.get_region_id();
        let to_peer_id = data.msg.get_to_peer().get_id();
        let to_store_id = data.msg.get_to_peer().get_store_id();

        SnapshotReporter {
            snapshot_status_sender: self.ch.snapshot_status_sender.clone(),
            region_id: region_id,
            to_peer_id: to_peer_id,
            to_store_id: to_store_id,
        }
    }

    fn send_snapshot_sock(&mut self, sock_addr: SocketAddr, data: ConnData) {
        let rep = self.new_snapshot_reporter(&data);
        let cb = box move |res: Result<()>| {
            if res.is_err() {
                rep.report(SnapshotStatus::Failure);
            } else {
                rep.report(SnapshotStatus::Finish);
            }
        };
        if let Err(Stopped(SnapTask::SendTo { cb, .. })) = self.snap_worker
            .schedule(SnapTask::SendTo {
                addr: sock_addr,
                data: data,
                cb: cb,
            }) {
            error!("channel is closed, failed to schedule snapshot to {}",
                   sock_addr);
            cb(Err(box_err!("failed to schedule snapshot")));
        }
    }
}

impl<T: RaftStoreRouter, S: StoreAddrResolver> Handler for Server<T, S> {
    type Timeout = Msg;
    type Message = Msg;

    fn notify(&mut self, event_loop: &mut EventLoop<Self>, msg: Msg) {
        match msg {
            Msg::Quit => event_loop.shutdown(),
            Msg::SendStore { store_id, data } => self.send_store(store_id, data),
            Msg::ResolveResult { store_id, sock_addr, data } => {
                self.on_resolve_result(store_id, sock_addr, data)
            }
            Msg::CloseConn { .. } => {}
        }
    }

    fn interrupted(&mut self, _: &mut EventLoop<Self>) {
        // To be able to be attached by gdb, we should not shutdown.
        // TODO: find a grace way to shutdown.
        // event_loop.shutdown();
    }

    fn tick(&mut self, el: &mut EventLoop<Self>) {
        // tick is called in the end of the loop, so if we notify to quit,
        // we will quit the server here.
        // TODO: handle quit server if event_loop is_running() returns false.
        if !el.is_running() {
            self.snap_worker.stop();
            self.raft_msg_worker.stop();
            self.grpc_server.shutdown();
        }
    }
}

struct SnapshotReporter {
    snapshot_status_sender: Sender<SnapshotStatusMsg>,
    region_id: u64,
    to_peer_id: u64,
    to_store_id: u64,
}

impl SnapshotReporter {
    pub fn report(&self, status: SnapshotStatus) {
        debug!("send snapshot to {} for {} {:?}",
               self.to_peer_id,
               self.region_id,
               status);

        if status == SnapshotStatus::Failure {
            let store = self.to_store_id.to_string();
            REPORT_FAILURE_MSG_COUNTER.with_label_values(&["snapshot", &*store]).inc();
        };

        if let Err(e) = self.snapshot_status_sender.send(SnapshotStatusMsg {
            region_id: self.region_id,
            to_peer_id: self.to_peer_id,
            status: status,
        }) {
            error!("report snapshot to peer {} in store {} with region {} err {:?}",
                   self.to_peer_id,
                   self.to_store_id,
                   self.region_id,
                   e);
        }
    }
}

// SendTask delivers a raft message to other store.
pub struct SendTask {
    pub addr: SocketAddr,
    pub msg: RaftMessage,
}

impl fmt::Display for SendTask {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "send raft message to {:?}", self.addr)
    }
}

struct Conn {
    _client: TikvClient,
    stream: mpsc::UnboundedSender<RaftMessage>,
}

impl Conn {
    fn new(env: Arc<Environment>, addr: SocketAddr, handle: &CoreHandle) -> Result<Conn> {
        let channel = ChannelBuilder::new(env).connect(&format!("{}", addr));
        let client = TikvClient::new(channel);
        let (tx, rx) = mpsc::unbounded();
        handle.spawn(client.raft().sink_map_err(Error::from)
            .send_all(rx.map_err(|_| Error::Sink))
            .map(|_| ())
            .map_err(|e| error!("send raftmessage failed: {:?}", e)));
        Ok(Conn {
            _client: client,
            stream: tx,
        })
    }
}

// SendRunner is used for sending raft messages to other stores.
pub struct SendRunner {
    env: Arc<Environment>,
    conns: HashMap<SocketAddr, Conn>,
}

impl SendRunner {
    pub fn new(env: Arc<Environment>) -> SendRunner {
        SendRunner {
            env: env,
            conns: HashMap::default(),
        }
    }

    fn get_conn(&mut self, addr: SocketAddr, handle: &CoreHandle) -> Result<&Conn> {
        // TDOO: handle Conn::new() error.
        let env = self.env.clone();
        let conn = self.conns
            .entry(addr)
            .or_insert_with(|| Conn::new(env.clone(), addr, handle).unwrap());
        Ok(conn)
    }

    fn send(&mut self, t: SendTask, handle: &CoreHandle) -> Result<()> {
        let conn = try!(self.get_conn(t.addr, handle));
        box_try!(mpsc::UnboundedSender::send(&conn.stream, t.msg));
        Ok(())
    }
}

impl FutureRunnable<SendTask> for SendRunner {
    fn run(&mut self, t: SendTask, handle: &CoreHandle) {
        let addr = t.addr;
        if let Err(e) = self.send(t, handle) {
            error!("send raft message error: {:?}", e);
            self.conns.remove(&addr);
        }
    }
}


#[cfg(test)]
mod tests {
    use std::thread;
    use std::sync::Arc;
    use std::sync::mpsc::{self, Sender};
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use mio::tcp::TcpListener;

    use super::*;
    use super::super::{Msg, ConnData, Result, Config};
    use super::super::transport::RaftStoreRouter;
    use super::super::resolve::{StoreAddrResolver, Callback as ResolveCallback};
    use storage::Storage;
    use kvproto::raft_serverpb::RaftMessage;
    use raftstore::Result as RaftStoreResult;
    use raftstore::store::Msg as StoreMsg;

    struct MockResolver {
        addr: SocketAddr,
    }

    impl StoreAddrResolver for MockResolver {
        fn resolve(&self, _: u64, cb: ResolveCallback) -> Result<()> {
            cb.call_box((Ok(self.addr),));
            Ok(())
        }
    }

    #[derive(Clone)]
    struct TestRaftStoreRouter {
        tx: Sender<usize>,
        report_unreachable_count: Arc<AtomicUsize>,
    }

    impl TestRaftStoreRouter {
        fn new(tx: Sender<usize>) -> TestRaftStoreRouter {
            TestRaftStoreRouter {
                tx: tx,
                report_unreachable_count: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl RaftStoreRouter for TestRaftStoreRouter {
        fn send(&self, _: StoreMsg) -> RaftStoreResult<()> {
            self.tx.send(1).unwrap();
            Ok(())
        }

        fn try_send(&self, _: StoreMsg) -> RaftStoreResult<()> {
            self.tx.send(1).unwrap();
            Ok(())
        }

        fn report_unreachable(&self, _: u64, _: u64, _: u64) -> RaftStoreResult<()> {
            let count = self.report_unreachable_count.clone();
            count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn test_peer_resolve() {
        let addr = "127.0.0.1:0".parse().unwrap();
        let listener = TcpListener::bind(&addr).unwrap();

        let resolver = MockResolver { addr: listener.local_addr().unwrap() };

        let cfg = Config::new();
        let mut event_loop = create_event_loop(&cfg).unwrap();
        let mut storage = Storage::new(&cfg.storage).unwrap();
        storage.start(&cfg.storage).unwrap();

        let (tx, rx) = mpsc::channel();
        let router = TestRaftStoreRouter::new(tx);
        let report_unreachable_count = router.report_unreachable_count.clone();

        let (snapshot_status_sender, _) = mpsc::channel();

        let ch = ServerChannel {
            raft_router: router,
            snapshot_status_sender: snapshot_status_sender,
        };
        let mut server =
            Server::new(&mut event_loop,
                        &cfg,
                        listener,
                        storage,
                        ch,
                        resolver,
                        SnapManager::new("", None, cfg.raft_store.use_sst_file_snapshot))
                .unwrap();

        for i in 0..10 {
            if i % 2 == 1 {
                server.report_unreachable(ConnData::new(0, RaftMessage::new()));
            }
            assert_eq!(report_unreachable_count.load(Ordering::SeqCst), (i + 1) / 2);
        }

        let ch = server.get_sendch();
        let h = thread::spawn(move || {
            event_loop.run(&mut server).unwrap();
        });

        ch.try_send(Msg::SendStore {
                store_id: 1,
                data: ConnData::new(0, RaftMessage::new()),
            })
            .unwrap();

        rx.recv().unwrap();

        ch.try_send(Msg::Quit).unwrap();
        h.join().unwrap();
    }
}
