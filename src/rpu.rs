//! The `rpu` module implements the Request Processing Unit, a
//! 5-stage transaction processing pipeline in software.

use bank::Bank;
use crdt::{Crdt, ReplicatedData};
use hash::Hash;
use packet;
use record_stage::RecordStage;
use request_processor::RequestProcessor;
use request_stage::RequestStage;
use result::Result;
use sig_verify_stage::SigVerifyStage;
use std::io::Write;
use std::net::UdpSocket;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::Duration;
use streamer;
use write_stage::WriteStage;

pub struct Rpu {
    bank: Arc<Bank>,
    start_hash: Hash,
    tick_duration: Option<Duration>,
}

impl Rpu {
    /// Create a new Rpu that wraps the given Bank.
    pub fn new(bank: Bank, start_hash: Hash, tick_duration: Option<Duration>) -> Self {
        Rpu {
            bank: Arc::new(bank),
            start_hash,
            tick_duration,
        }
    }

    /// Create a UDP microservice that forwards messages the given Rpu.
    /// This service is the network leader
    /// Set `exit` to shutdown its threads.
    pub fn serve<W: Write + Send + 'static>(
        &self,
        me: ReplicatedData,
        requests_socket: UdpSocket,
        gossip: UdpSocket,
        exit: Arc<AtomicBool>,
        writer: W,
    ) -> Result<Vec<JoinHandle<()>>> {
        let crdt = Arc::new(RwLock::new(Crdt::new(me)));
        let t_gossip = Crdt::gossip(crdt.clone(), exit.clone());
        let window = streamer::default_window();
        let t_listen = Crdt::listen(crdt.clone(), window.clone(), gossip, exit.clone());

        // make sure we are on the same interface
        let mut local = requests_socket.local_addr()?;
        local.set_port(0);

        let packet_recycler = packet::PacketRecycler::default();
        let (packet_sender, packet_receiver) = channel();
        let t_receiver = streamer::receiver(
            requests_socket,
            exit.clone(),
            packet_recycler.clone(),
            packet_sender,
        )?;

        let sig_verify_stage = SigVerifyStage::new(exit.clone(), packet_receiver);

        let blob_recycler = packet::BlobRecycler::default();
        let request_processor = RequestProcessor::new(self.bank.clone());
        let request_stage = RequestStage::new(
            request_processor,
            exit.clone(),
            sig_verify_stage.verified_receiver,
            packet_recycler.clone(),
            blob_recycler.clone(),
        );

        let record_stage = RecordStage::new(
            request_stage.signal_receiver,
            &self.start_hash,
            self.tick_duration,
        );

        let write_stage = WriteStage::new(
            self.bank.clone(),
            exit.clone(),
            blob_recycler.clone(),
            Mutex::new(writer),
            record_stage.entry_receiver,
        );

        let broadcast_socket = UdpSocket::bind(local)?;
        let t_broadcast = streamer::broadcaster(
            broadcast_socket,
            exit.clone(),
            crdt.clone(),
            window,
            blob_recycler.clone(),
            write_stage.blob_receiver,
        );

        let respond_socket = UdpSocket::bind(local.clone())?;
        let t_responder = streamer::responder(
            respond_socket,
            exit.clone(),
            blob_recycler.clone(),
            request_stage.blob_receiver,
        );

        let mut threads = vec![
            t_receiver,
            t_responder,
            request_stage.thread_hdl,
            write_stage.thread_hdl,
            t_gossip,
            t_listen,
            t_broadcast,
        ];
        threads.extend(sig_verify_stage.thread_hdls.into_iter());
        Ok(threads)
    }
}
