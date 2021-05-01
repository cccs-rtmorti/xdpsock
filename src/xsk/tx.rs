//! Transmit end of AF_XDP socket

use crate::{socket::*, umem::*};

use std::error::Error;
use std::fmt;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;

#[derive(Debug)]
pub struct TxStats {
    pub pkts_tx: u64,
    pub start_time: Instant,
    pub end_time: Instant,
}

impl TxStats {
    pub fn new() -> Self {
        Self {
            pkts_tx: 0,
            start_time: Instant::now(),
            end_time: Instant::now(),
        }
    }

    pub fn duration(&self) -> Duration {
        self.end_time.duration_since(self.start_time)
    }

    pub fn pps(&self) -> f64 {
        self.pkts_tx as f64 / self.duration().as_secs_f64()
    }
}

#[derive(Debug)]
pub struct TxConfig {}

/// Send Error for Xsk
#[derive(Debug)]
pub enum XskSendError {
    NoFreeTxFrames,
}

impl fmt::Display for XskSendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoFreeTxFrames => write!(f, "there are no free tx frames, try again later"),
        }
    }
}

impl Error for XskSendError {}

#[derive(Debug)]
pub struct XskTx<'a> {
    pub tx_q: TxQueue<'a>,
    pub comp_q: CompQueue<'a>,
    pub tx_frames: Vec<Frame<'a>>,
    pub pkts_to_send: Receiver<Vec<u8>>,
    pub outstanding_tx_frames: u64,
    pub tx_poll_ms_timeout: i32,
    pub tx_cursor: usize,
    pub frame_size: u32,
    pub stats: TxStats,
    pub batch_size: usize,
    pub target_pps: u64,
    pub pps_threshold: u64,
}

impl<'a> XskTx<'a> {
    fn rate_limit(&self) {
        if self.target_pps == 0 {
            return;
        }

        if self.stats.pkts_tx % (self.target_pps / 10) == 0 {
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn send_loop(&mut self) {
        let pkt_iter = self.pkts_to_send.clone().into_iter();
        for data in pkt_iter {
            loop {
                self.rate_limit();
                match self.send(&data) {
                    Ok(_) => {
                        self.stats.pkts_tx += 1;
                        break;
                    }
                    Err(e) => match e {
                        XskSendError::NoFreeTxFrames => {
                            log::debug!("tx: No tx frames");
                            thread::sleep(Duration::from_millis(5));
                        }
                    },
                }
            }
        }
        log::debug!("tx: send loop complete");
        self.stats.end_time = Instant::now();
    }

    fn send(&mut self, data: &[u8]) -> Result<(), XskSendError> {
        log::debug!("tx: tx_cursor = {}", self.tx_cursor);
        let free_frames = self.comp_q.consume(self.outstanding_tx_frames);
        self.outstanding_tx_frames -= free_frames.len() as u64;

        if free_frames.len() == 0 {
            log::debug!("comp_q.consume() consumed 0 frames");
            if self.tx_q.needs_wakeup() {
                log::debug!("tx: waking up tx_q");
                self.tx_q.wakeup().expect("failed to wake up tx queue");
                log::debug!("tx: woke up tx_q");
            }
        }
        log::debug!(
            "dev2.comp_q.consume() consumed {} frames",
            free_frames.len()
        );

        self.update_tx_frames(&free_frames);

        if !self.tx_frames[self.tx_cursor].status.is_free() {
            return Err(XskSendError::NoFreeTxFrames);
        }

        unsafe {
            self.tx_frames[self.tx_cursor]
                .write_to_umem_checked(data)
                .expect("failed to write to umem");
        }

        // Add consumed frames back to the tx queue
        if ((self.tx_cursor + 1) % self.batch_size) == 0 {
            let start = self.tx_cursor + 1 - self.batch_size;
            let end = self.tx_cursor + 1;
            log::debug!("tx: adding tx_frames[{}..{}] to tx queue", start, end);

            while unsafe {
                self.tx_q
                    .produce_and_wakeup(&self.tx_frames[start..end])
                    .expect("failed to add frames to tx queue")
            } != self.batch_size
            {
                // Loop until frames added to the tx ring.
                log::debug!("tx_q.produce_and_wakeup() failed to allocate");
            }
            log::debug!("tx_q.produce_and_wakeup() submitted {} frames", 1);
        }

        self.outstanding_tx_frames += 1;
        self.tx_frames[self.tx_cursor].status = FrameStatus::OnTxQueue;
        self.tx_cursor = (self.tx_cursor + 1) % self.tx_frames.len();
        Ok(())
    }

    fn update_tx_frames(&mut self, free_frames: &[u64]) {
        for free_frame in free_frames {
            let tx_frame_index = *free_frame as u32 / self.frame_size;
            log::debug!(
                "update tx_frame, tx_frame_index = {}, free_frame = {}",
                tx_frame_index,
                free_frame
            );
            self.tx_frames[tx_frame_index as usize].status = FrameStatus::Free;
        }
    }
}
