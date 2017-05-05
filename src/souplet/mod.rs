use std::collections::HashMap;
use std::collections::hash_map::Keys;
use std::net::SocketAddr;
use std::thread;
use std::sync::{Arc, Mutex};
use std::sync::mpsc;
use std::io;
use std::process;
use std::time;

use tarpc::future::server;
use tarpc::sync::client::ClientExt;
use tarpc::sync::client;
use tarpc::util::Never;
use tokio_core::reactor;

use slog;

use channel;
use checktable;
use flow::prelude::*;
use flow::domain;

service! {
    rpc start_domain(domain_index: domain::Index, nodes: DomainNodes);
    rpc recv_packet(domain_index: domain::Index, packet: Packet);
    rpc recv_input_packet(domain_index: domain::Index, packet: Packet);
    rpc recv_on_channel(tag: u64, data: Vec<u8>);
    rpc close_channel(tag: u64);
    rpc shutdown();
}

struct SoupletServerInner {
    pub domain_txs: HashMap<domain::Index, mpsc::SyncSender<Packet>>,
    pub domain_input_txs: HashMap<domain::Index, mpsc::SyncSender<Packet>>,
}

#[derive(Clone)]
struct SoupletServer {
    inner: Arc<Mutex<SoupletServerInner>>,
    demux_table: channel::DemuxTable,
    remote: reactor::Remote,
    local_addr: SocketAddr,
}

impl SoupletServer {
    pub fn new(local_addr: SocketAddr,
               demux_table: channel::DemuxTable,
               remote: reactor::Remote)
               -> Self {
        let inner = SoupletServerInner {
            domain_txs: HashMap::new(),
            domain_input_txs: HashMap::new(),
        };

        Self {
            inner: Arc::new(Mutex::new(inner)),
            demux_table,
            remote,
            local_addr,
        }
    }

    pub fn get_inner(&self) -> Arc<Mutex<SoupletServerInner>> {
        self.inner.clone()
    }
}

impl FutureService for SoupletServer {
    type StartDomainFut = Result<(), Never>;
    fn start_domain(&self,
                    domain_index: domain::Index,
                    nodes: DomainNodes)
                    -> Self::StartDomainFut {
        let mock_logger = slog::Logger::root(slog::Discard, None);
        let mock_checktable = Arc::new(Mutex::new(checktable::CheckTable::new()));

        let (in_tx, in_rx) = mpsc::sync_channel(256);
        let (tx, rx) = mpsc::sync_channel(1);

        let domain = domain::Domain::new(mock_logger, domain_index, nodes, mock_checktable, 0);
        domain.boot(rx, in_rx);

        let mut inner = self.inner.lock().unwrap();
        inner.domain_txs.insert(domain_index, tx);
        inner.domain_input_txs.insert(domain_index, in_tx);
        println!("Started domain #{}", domain_index.index());
        Ok(())
    }

    type RecvPacketFut = Result<(), Never>;
    fn recv_packet(&self, domain_index: domain::Index, mut packet: Packet) -> Self::RecvPacketFut {
        packet.complete_deserialize(self.local_addr.clone(), &self.demux_table);
        let inner = self.inner.lock().unwrap();
        inner.domain_txs[&domain_index].send(packet).unwrap();
        Ok(())
    }

    type RecvInputPacketFut = Result<(), Never>;
    fn recv_input_packet(&self,
                         domain_index: domain::Index,
                         mut packet: Packet)
                         -> Self::RecvInputPacketFut {
        packet.complete_deserialize(self.local_addr.clone(), &self.demux_table);
        let inner = self.inner.lock().unwrap();
        inner.domain_input_txs[&domain_index]
            .send(packet)
            .unwrap();
        Ok(())
    }

    type ShutdownFut = Result<(), Never>;
    fn shutdown(&self) -> Self::ShutdownFut {
        process::exit(0)
    }

    type RecvOnChannelFut = Result<(), Never>;
    fn recv_on_channel(&self, tag: u64, data: Vec<u8>) -> Self::RecvOnChannelFut {
        let demux_table = &self.demux_table.lock().unwrap().1;
        demux_table[&tag].recv_bytes(&data[..]).unwrap();
        Ok(())
    }

    type CloseChannelFut = Result<(), Never>;
    fn close_channel(&self, tag: u64) -> Self::CloseChannelFut {
        let mut demux_table = &mut self.demux_table.lock().unwrap().1;
        demux_table.remove(&tag);
        Ok(())
    }
}

pub struct Souplet {
    reactor_thread: thread::JoinHandle<()>,
    peers: HashMap<SocketAddr, SyncClient>,
    demux_table: channel::DemuxTable,
    local_addr: SocketAddr,
    server_inner: Arc<Mutex<SoupletServerInner>>,
    shutdown_tx: mpsc::Sender<()>,
}

impl Souplet {
    pub fn new(addr: SocketAddr) -> Self {
        let (tx, rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();

        let demux_table = Arc::new(Mutex::new((0, HashMap::new())));
        let demux_table2 = demux_table.clone();

        let reactor_thread = thread::spawn(move || {
            let mut reactor = reactor::Core::new().unwrap();

            let server = SoupletServer::new(addr, demux_table2, reactor.remote());
            let server_inner = server.get_inner();

            let listener = server
                .listen(addr, &reactor.handle(), server::Options::default())
                .unwrap();

            tx.send((server_inner, listener.0.addr())).unwrap();
            reactor.handle().spawn(listener.1);

            while shutdown_rx.try_recv().is_err() {
                reactor.turn(None)
            }
        });

        let (server_inner, addr) = rx.recv().unwrap();

        Self {
            reactor_thread,
            peers: HashMap::new(),
            demux_table,
            local_addr: addr,
            server_inner,
            shutdown_tx,
        }
    }

    pub fn connect_to_peer(&mut self, addr: SocketAddr) -> io::Result<()> {
        SyncClient::connect(addr, client::Options::default()).map(|c| {
                                                                      self.peers.insert(addr, c);
                                                                  })
    }

    pub fn start_domain(&self,
                        peer_addr: &SocketAddr,
                        domain: domain::Index,
                        nodes: DomainNodes)
                        -> channel::PacketSender {
        let peer = &self.peers[peer_addr];
        peer.start_domain(domain, nodes).unwrap();

        channel::PacketSender::make_remote(domain,
                                           peer.clone(),
                                           peer_addr.clone(),
                                           self.demux_table.clone(),
                                           self.local_addr)
    }

    pub fn add_local_domain(&self,
                            index: domain::Index,
                            tx: mpsc::SyncSender<Packet>,
                            input_tx: mpsc::SyncSender<Packet>) {
        let mut inner = self.server_inner.lock().unwrap();
        inner.domain_txs.insert(index.clone(), tx);
        inner.domain_input_txs.insert(index.clone(), input_tx);
    }

    pub fn listen(&mut self) {
        loop {
            thread::sleep(time::Duration::from_millis(1000));
        }
    }

    pub fn get_peers(&self) -> Keys<SocketAddr, SyncClient> {
        self.peers.keys()
    }

    pub fn get_local_addr(&self) -> SocketAddr {
        self.local_addr.clone()
    }
}

impl Drop for Souplet {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(());
    }
}

/// A `SoupletDaemon` listens for incoming connections, and starts up domains as requested.
pub struct SoupletDaemon {}
impl SoupletDaemon {
    /// Start a new WorkerDaemon instance.
    pub fn start(addr: SocketAddr) {
        Souplet::new(addr).listen();
    }
}
