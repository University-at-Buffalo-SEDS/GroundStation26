use std::collections::VecDeque;

// Live control state is applied at ingress. The bulk worker only persists
// its arrival-time snapshot, so a delayed recording cannot roll it back.
pub enum ReceivedPacket {
    Telemetry(sedsnet::packet::Packet),
    AppliedStatus { packet: sedsnet::packet::Packet, row: crate::types::TelemetryRow },
}
impl From<sedsnet::packet::Packet> for ReceivedPacket {
    fn from(packet: sedsnet::packet::Packet) -> Self { Self::Telemetry(packet) }
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
