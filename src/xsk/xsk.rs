//! AF_XDP socket
use super::rx::*;
use super::tx::*;
use crate::xsk::rx::*;
use crate::{socket::*, umem::*};

use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crossbeam_channel::{bounded, Receiver, Sender};
use etherparse::ReadError;

pub const MAX_PACKET_SIZE: usize = 4096;

/// AF_XDP socket
pub struct Xsk<'a> {
    pub if_name: &'a str,
    pub fill_q: FillQueue<'a>,
    pub comp_q: CompQueue<'a>,
    pub tx_q: TxQueue<'a>,
    pub rx_q: RxQueue<'a>,
    pub tx_frames: Vec<Frame<'a>>,
    pub rx_frames: Vec<Frame<'a>>,
    pub umem: Umem<'a>,
    pub umem_config: UmemConfig,
    pub socket_config: SocketConfig,
}

impl<'a> Xsk<'a> {
    pub fn new(
        if_name: &'a str,
        queue_id: u32,
        umem_config: UmemConfig,
        socket_config: SocketConfig,
        n_tx_frames: usize,
    ) -> Self {
        let (mut umem, fill_q, comp_q, frames) = Umem::builder(umem_config.clone())
            .create_mmap()
            .expect("failed to create mmap area")
            .create_umem()
            .expect("failed to create umem");

        let (tx_q, rx_q) = Socket::new(socket_config.clone(), &mut umem, if_name, queue_id)
            .expect("failed to build socket");

        let tx_frames = frames[..n_tx_frames].into();
        let rx_frames = frames[n_tx_frames..].into();

        Self {
            if_name,
            fill_q,
            comp_q,
            tx_q,
            rx_q,
            tx_frames,
            rx_frames,
            umem,
            umem_config,
            socket_config,
        }
    }
}

/// AF_XDP socket new implementation
pub struct Xsk2<'a> {
    pub ifname: &'a str,
    pub umem: Umem<'a>,
    pub umem_config: UmemConfig,
    pub socket_config: SocketConfig,
    tx_handle: Option<JoinHandle<TxStats>>,
    tx_channel: Option<Sender<([u8; MAX_PACKET_SIZE], usize)>>,
    rx_handle: Option<JoinHandle<RxStats>>,
    rx_channel: Option<Receiver<([u8; MAX_PACKET_SIZE], usize)>>,
    shutdown: Arc<AtomicBool>,
}

impl<'a> Xsk2<'a> {
    pub fn new(
        if_name: &'a str,
        queue_id: u32,
        umem_config: UmemConfig,
        socket_config: SocketConfig,
        n_tx_frames: usize,
    ) -> Self {
        let (mut umem, fill_q, comp_q, frames) = Umem::builder(umem_config.clone())
            .create_mmap()
            .expect("failed to create mmap area")
            .create_umem()
            .expect("failed to create umem");

        let (tx_q, rx_q) = Socket::new(socket_config.clone(), &mut umem, if_name, queue_id)
            .expect("failed to build socket");

        let tx_frames = frames[..n_tx_frames].into();
        let rx_frames = frames[n_tx_frames..].into();

        let tx_channel_capacity = 100_000;
        let rx_channel_capacity = 100_000;

        let (tx_pkt_send, tx_pkt_recv) = bounded(tx_channel_capacity);
        let (rx_pkt_send, rx_pkt_recv) = bounded(rx_channel_capacity);

        let shutdown = Arc::new(AtomicBool::new(false));

        // TODO: Create new function that enforces tx_frames.len() is multiple of batch size
        let mut xsk_tx = XskTx {
            tx_q,
            comp_q,
            tx_frames,
            pkts_to_send: tx_pkt_recv,
            outstanding_tx_frames: 0,
            tx_poll_ms_timeout: 1,
            tx_cursor: 0,
            frame_size: umem_config.frame_size(),
            stats: TxStats::new(),
            //target_pps: 100_000,
            target_pps: 0,
            pps_threshold: 5_000,
            batch_size: 1,
        };

        let mut xsk_rx = XskRx {
            rx_q,
            fill_q,
            rx_frames,
            pkts_recvd: rx_pkt_send,
            outstanding_rx_frames: 0,
            rx_cursor: 0,
            poll_ms_timeout: 1,
            shutdown: shutdown.clone(),
            include_payload: true,
            stats: RxStats::new(),
        };

        let core_ids = core_affinity::get_core_ids().expect("failed to get cpu core ids");
        let core_num = queue_id as usize % core_ids.len();
        let core_tx = core_ids[core_num].clone();
        let core_rx = core_ids[core_num].clone();

        let tx_handle = thread::spawn(move || {
            log::debug!("tx: pinning thread to core {:?}", core_tx);
            core_affinity::set_for_current(core_tx);
            xsk_tx.send_loop();
            xsk_tx.stats
        });

        let rx_handle = thread::spawn(move || {
            log::debug!("rx: pinning thread to core {:?}", core_rx);
            core_affinity::set_for_current(core_rx);
            xsk_rx.start_recv();
            xsk_rx.stats
        });

        Self {
            ifname: if_name,
            umem,
            umem_config,
            socket_config,
            tx_handle: Some(tx_handle),
            tx_channel: Some(tx_pkt_send),
            rx_handle: Some(rx_handle),
            rx_channel: Some(rx_pkt_recv),
            shutdown,
        }
    }

    pub fn shutdown_rx(&mut self) -> Option<RxStats> {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(rx_channel) = self.rx_channel.take() {
            drop(rx_channel);
        }
        if let Some(rx_handle) = self.rx_handle.take() {
            let stats = rx_handle.join().expect("failed to join rx_handle");
            return Some(stats);
        }
        None
    }

    pub fn shutdown_tx(&mut self) -> Option<TxStats> {
        if let Some(tx_channel) = self.tx_channel.take() {
            drop(tx_channel);
        }
        if let Some(tx_handle) = self.tx_handle.take() {
            let stats = tx_handle.join().expect("failed to join tx_handle");
            return Some(stats);
        }
        None
    }

    pub fn send(&mut self, data: &[u8]) {
        if let Some(ref mut tx_channel) = self.tx_channel {
            let mut packet: [u8; MAX_PACKET_SIZE] = [0; MAX_PACKET_SIZE];
            let l = std::cmp::min(MAX_PACKET_SIZE, data.len());
            let packet_slice = &mut packet[..l];
            packet_slice.copy_from_slice(&data[..l]);

            tx_channel.send((packet, l)).expect("failed to send");
        }
    }

    pub fn tx_sender(&self) -> Option<Sender<([u8; MAX_PACKET_SIZE], usize)>> {
        if let Some(ref tx_channel) = self.tx_channel {
            Some(tx_channel.clone())
        } else {
            None
        }
    }

    pub fn rx_receiver(&self) -> Option<Receiver<([u8; MAX_PACKET_SIZE], usize)>> {
        if let Some(ref rx_channel) = self.rx_channel {
            Some(rx_channel.clone())
        } else {
            None
        }
    }

    pub fn recv(&mut self) -> Option<([u8; MAX_PACKET_SIZE], usize)> {
        if let Some(ref rx_channel) = self.rx_channel {
            let recvd = rx_channel.recv().expect("failed to recv");
            Some(recvd)
        } else {
            None
        }
    }
}
