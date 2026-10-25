//! Both paired-document profiles share one signed-envelope byte allowance.
use std::sync::{Mutex, OnceLock};
#[derive(Clone, Copy)]
pub(crate) enum Class {
    Legacy = 0,
    #[cfg(not(feature = "hitl_mode"))]
    Checklist = 1,
}
#[derive(Default)]
struct Outbox {
    pending: [Option<Vec<u8>>; 2],
    next: u64,
    exhausted: bool,
    cursor: usize,
}
impl Outbox {
    fn offer(&mut self, class: Class, wire: &[u8]) -> bool {
        if wire.is_empty() || wire.len() > paired_settings::auth::MAX_WIRE {
            return false;
        }
        let slot = &mut self.pending[class as usize];
        if slot.is_some() {
            return false;
        }
        *slot = Some(wire.to_vec());
        true
    }
    fn flush(&mut self, now: u64, mut send: impl FnMut(&[u8]) -> bool) {
        if self.exhausted || now < self.next {
            return;
        }
        for _ in 0..2 {
            let index = self.cursor;
            self.cursor = (index + 1) % 2;
            let Some(wire) = self.pending[index].as_ref() else {
                continue;
            };
            let accepted = send(wire);
            let interval =
                (wire.len() as u64 * 1000).div_ceil(paired_settings::radio::BYTES_PER_SECOND);
            match now.checked_add(interval) {
                Some(next) => self.next = next,
                None => self.exhausted = true,
            }
            if accepted {
                self.pending[index] = None;
            }
            break;
        }
    }
}
static OUTBOX: OnceLock<Mutex<Outbox>> = OnceLock::new();
fn outbox() -> &'static Mutex<Outbox> {
    OUTBOX.get_or_init(Default::default)
}
pub(crate) fn offer(class: Class, wire: &[u8]) -> bool {
    outbox().lock().unwrap().offer(class, wire)
}
pub(crate) fn flush(mesh: &crate::rrls::Shared, now: u64) {
    outbox()
        .lock()
        .unwrap()
        .flush(now, |wire| mesh.lock().unwrap().queue_settings(wire, now));
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classes_share_allowance_retain_exact_backpressure_and_take_turns() {
        let mut outbox = Outbox::default();
        assert!(outbox.offer(Class::Legacy, &[1; 128]));
        #[cfg(not(feature = "hitl_mode"))]
        assert!(outbox.offer(Class::Checklist, &[2; 128]));
        assert!(!outbox.offer(Class::Legacy, &[3]));
        let mut sends = Vec::new();
        outbox.flush(0, |wire| {
            sends.push(wire.to_vec());
            false
        });
        outbox.flush(499, |_| panic!("aggregate allowance must delay next class"));
        outbox.flush(500, |wire| {
            sends.push(wire.to_vec());
            true
        });
        outbox.flush(1000, |wire| {
            sends.push(wire.to_vec());
            true
        });
        assert_eq!(sends.first(), Some(&vec![1; 128]));
        #[cfg(not(feature = "hitl_mode"))]
        assert_eq!(sends, vec![vec![1; 128], vec![2; 128], vec![1; 128]]);
        #[cfg(feature = "hitl_mode")]
        assert_eq!(sends, vec![vec![1; 128], vec![1; 128]]);
        assert!(outbox.pending.iter().all(Option::is_none));
    }
    #[test]
    fn wire_bounds_and_terminal_clock_do_not_burst() {
        let mut outbox = Outbox::default();
        assert!(!outbox.offer(Class::Legacy, &[]));
        assert!(!outbox.offer(Class::Legacy, &vec![0; paired_settings::auth::MAX_WIRE + 1]));
        assert!(outbox.offer(Class::Legacy, &[1]));
        outbox.flush(u64::MAX, |_| false);
        outbox.flush(u64::MAX, |_| panic!("overflow cannot refill allowance"));
    }
}
