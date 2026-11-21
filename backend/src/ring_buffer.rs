use std::collections::VecDeque;

// Status decisions are fixed at ingress. AppliedStatus carries either a live
// VALVE_STATE snapshot or an unconfirmed raw-status snapshot with no values.
// The bulk worker only persists it; discovery/recording cannot re-admit it.
pub enum ReceivedPacket {
    Telemetry {
        packet: sedsnet::packet::Packet,
        ingress_generation: u64,
    },
    AppliedStatus {
        packet: sedsnet::packet::Packet,
        row: crate::types::TelemetryRow,
    },
}

pub struct RingBuffer<T> {
    max: usize,
    buf: VecDeque<T>,
}

impl<T> RingBuffer<T> {
    pub fn new(max: usize) -> Self {
        Self {
            max,
            buf: VecDeque::with_capacity(max),
        }
    }

    pub fn push(&mut self, item: impl Into<T>) {
        let item = item.into();
        if self.buf.len() == self.max {
            self.buf.pop_front();
        }
        self.buf.push_back(item);
    }

    #[allow(dead_code)]
    pub fn recent(&self, n: usize) -> Vec<&T> {
        self.buf.iter().rev().take(n).collect()
    }

    pub fn pop_oldest(&mut self) -> Option<T> {
        self.buf.pop_front()
    }
    #[allow(dead_code)]
    pub fn pop_newest(&mut self) -> Option<T> {
        self.buf.pop_back()
    }
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }
}
