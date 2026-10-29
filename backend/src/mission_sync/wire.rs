//! Both paired-document profiles share one signed-envelope byte allowance.
use std::sync::{Mutex, OnceLock};
#[derive(Clone, Copy)]
pub(crate) enum Class {
    Legacy,
    ModeQuery,
    ModeReply,
    ClockQuery,
    ClockReply,
    #[cfg(not(feature = "hitl_mode"))]
    Checklist,
}
#[derive(Default)]
struct Outbox {
    pending: [Option<(Class, Vec<u8>)>; 4],
    next: u64,
    exhausted: bool,
    cursor: usize,
}
impl Outbox {
    fn offer(&mut self, class: Class, wire: &[u8]) -> bool {
        if wire.is_empty() || wire.len() > paired_settings::auth::MAX_WIRE {
            return false;
        }
        let index = match class {
            Class::Legacy => 0,
            Class::ModeQuery | Class::ModeReply => 2,
            Class::ClockQuery | Class::ClockReply => 3,
            #[cfg(not(feature = "hitl_mode"))]
            Class::Checklist => 1,
        };
        let slot = &mut self.pending[index];
        if slot.is_some() {
            return false;
        }
        *slot = Some((class, wire.to_vec()));
        true
    }
    fn flush(&mut self, now: u64, mut send: impl FnMut(Class, &[u8]) -> bool) {
        if self.exhausted || now < self.next {
            return;
        }
        for _ in 0..self.pending.len() {
            let index = self.cursor;
            self.cursor = (index + 1) % self.pending.len();
            let Some((class, wire)) = self.pending[index].as_ref() else {
                continue;
            };
            let accepted = send(*class, wire);
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
    outbox().lock().unwrap().flush(now, |class, wire| {
        let mut mesh = mesh.lock().unwrap();
        if matches!(class, Class::ModeReply | Class::ClockReply) {
            mesh.queue_operating_mode_reply(wire, now)
        } else {
            mesh.queue_settings(wire, now)
        }
    });
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
        outbox.flush(0, |_, wire| {
            sends.push(wire.to_vec());
            false
        });
        outbox.flush(499, |_, _| {
            panic!("aggregate allowance must delay next class")
        });
        outbox.flush(500, |_, wire| {
            sends.push(wire.to_vec());
            true
        });
        outbox.flush(1000, |_, wire| {
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
    fn display_mode_and_signed_documents_share_one_existing_byte_allowance() {
        let mut outbox = Outbox::default();
        assert!(outbox.offer(Class::ModeQuery, &[2; 25]));
        assert!(
            !outbox.offer(Class::ModeQuery, &[3; 25]),
            "only one pending mode frame"
        );
        assert!(outbox.offer(Class::Legacy, &[1; 256]));
        outbox.cursor = 2;
        let mut sends = Vec::new();
        outbox.flush(0, |_, wire| {
            sends.push(wire.len());
            true
        });
        outbox.flush(97, |_, _| {
            panic!("25 mode bytes require98ms at existing256B/s")
        });
        outbox.flush(98, |_, wire| {
            sends.push(wire.len());
            true
        });
        assert_eq!(sends, vec![25, 256]);
        assert_eq!(outbox.next, 1098);
    }
    #[test]
    fn wire_bounds_and_terminal_clock_do_not_burst() {
        let mut outbox = Outbox::default();
        assert!(!outbox.offer(Class::Legacy, &[]));
        assert!(!outbox.offer(Class::Legacy, &vec![0; paired_settings::auth::MAX_WIRE + 1]));
        assert!(outbox.offer(Class::Legacy, &[1]));
        outbox.flush(u64::MAX, |_, _| false);
        outbox.flush(u64::MAX, |_, _| panic!("overflow cannot refill allowance"));
    }
}
