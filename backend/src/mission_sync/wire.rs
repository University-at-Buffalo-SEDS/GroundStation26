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
    fn flush_mesh(&mut self, mesh: &crate::rrls::Shared, _source_work_at: u64) {
        // The caller's source tick can precede awaited SQL/fsync/signature work.
        // Stamp the Mesh deadline and shared byte allowance only when the owned
        // frame is actually offered, using the Mesh's current monotonic clock.
        let mut mesh = mesh.lock().unwrap();
        let admitted_at = mesh.now();
        self.flush(admitted_at, |class, wire| {
            if matches!(class, Class::ModeReply | Class::ClockReply) {
                mesh.queue_operating_mode_reply(wire, admitted_at)
            } else {
                mesh.queue_settings(wire, admitted_at)
            }
        });
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
    outbox().lock().unwrap().flush_mesh(mesh, now);
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn slow_work_before_signed_journal_flush_uses_actual_admission_clock() {
        use paired_settings::{
            journal::{Journal, Limits},
            journal_replication::JournalRadio,
        };
        use ring::signature::Ed25519KeyPair;
        use std::sync::{Arc, Mutex};
        let path = std::env::temp_dir().join(format!(
            "gs-journal-admission-{}-{}",
            std::process::id(),
            rand::random::<u128>()
        ));
        let journal = Journal::open(
            &path.join("journal.sqlite3"),
            "test".into(),
            vec!["gs".into(), "rrls".into()],
            Limits::default(),
        )
        .await
        .unwrap();
        journal
            .edit(
                "checklists/progress/test",
                serde_json::json!({"checked":true}),
                "gs",
                |_, _| Ok(()),
            )
            .await
            .unwrap();
        let revision = journal.metadata().await.unwrap().revision;
        assert!(
            revision > 0,
            "the local update is already durable before its first wire attempt"
        );
        let signer = Ed25519KeyPair::from_seed_unchecked(&[17; 32]).unwrap();
        let mut radio = JournalRadio::new("test".into(), "gs".into(), vec!["rrls".into()]).unwrap();
        let mesh = Arc::new(Mutex::new(crate::rrls::Mesh::new(
            crate::rrls::Role::Primary,
            7,
        )));
        let scheduled = mesh.lock().unwrap().now();
        mesh.lock().unwrap().authenticated_settings_peer(scheduled);
        let mut owned = Outbox::default();
        let staged = radio
            .poll(&journal, scheduled, &signer, |_, wire| {
                owned.offer(Class::Legacy, wire)
            })
            .await
            .unwrap()
            .unwrap();
        assert!(staged.accepted);
        let signed_wire = owned.pending[0].as_ref().unwrap().1.clone();
        assert!(JournalRadio::recognizes(&signed_wire));
        // Model awaited SQL/fsync/signature work or producer scheduling after
        // capturing the source tick but before the actual Mesh queue admission.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let before_admission = mesh.lock().unwrap().now();
        assert!(before_admission.saturating_sub(scheduled) > 250);
        owned.flush_mesh(&mesh, scheduled);
        assert!(
            owned.pending[0].is_none(),
            "accepted Mesh ownership releases the outbox once"
        );
        let frame = {
            let mut link = mesh.lock().unwrap();
            let now = link.now();
            link.pop(now)
        };
        eprintln!(
            "journal admission scheduled_ms={scheduled} before_admission_ms={before_admission} frame_present={} durable_revision={revision}",
            frame.is_some()
        );
        let frame =
            frame.expect("signed metadata admitted after slow work must not be born expired");
        assert_eq!(
            &frame.bytes[4..frame.bytes.len() - 4],
            signed_wire.as_slice()
        );
        let interval =
            (signed_wire.len() as u64 * 1000).div_ceil(paired_settings::radio::BYTES_PER_SECOND);
        assert!(
            owned.next >= before_admission + interval,
            "the shared byte allowance starts at actual admission, not before awaited work"
        );
        // Explicit old-path control: queue admission can succeed, then expiry
        // discards that frame. The durable journal is NOT permanently lost:
        // its unchanged HELLO protocol retries at5000ms, not before that.
        let old_mesh = Arc::new(Mutex::new(crate::rrls::Mesh::new(
            crate::rrls::Role::Primary,
            8,
        )));
        let old_tick = old_mesh.lock().unwrap().now();
        old_mesh
            .lock()
            .unwrap()
            .authenticated_settings_peer(old_tick);
        let mut old_radio =
            JournalRadio::new("test".into(), "gs".into(), vec!["rrls".into()]).unwrap();
        let mut old_owned = Outbox::default();
        assert!(
            old_radio
                .poll(&journal, old_tick, &signer, |_, wire| old_owned
                    .offer(Class::Legacy, wire))
                .await
                .unwrap()
                .unwrap()
                .accepted
        );
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        old_owned.flush(old_tick, |_, wire| {
            old_mesh.lock().unwrap().queue_settings(wire, old_tick)
        });
        assert!(old_owned.pending[0].is_none());
        assert!(
            {
                let mut link = old_mesh.lock().unwrap();
                let now = link.now();
                link.pop(now)
            }
            .is_none(),
            "the old backdated-admission control must reproduce expiry"
        );
        assert!(
            old_radio
                .poll(&journal, old_tick + 4999, &signer, |_, _| panic!(
                    "HELLO retry is still5000ms"
                ))
                .await
                .unwrap()
                .is_none()
        );
        let mut retried = Vec::new();
        assert!(
            old_radio
                .poll(&journal, old_tick + 5000, &signer, |_, wire| {
                    retried.push(wire.to_vec());
                    true
                })
                .await
                .unwrap()
                .unwrap()
                .accepted
        );
        assert_eq!(
            retried,
            vec![signed_wire],
            "durable state remains advertised by the existing retry, not event_saved suppression"
        );
        assert_eq!(journal.metadata().await.unwrap().revision, revision);
        journal.close().await;
        std::fs::remove_dir_all(path).unwrap();
    }
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

#[cfg(all(test,target_os="linux",any(feature="firmware_sim",feature="hitl_mode")))]
#[derive(Default)]
pub(crate) struct NativeOutbox(Outbox);
#[cfg(all(test,target_os="linux",any(feature="firmware_sim",feature="hitl_mode")))]
impl NativeOutbox {
    pub(crate) fn offer(&mut self,class:Class,wire:&[u8])->bool{self.0.offer(class,wire)}
    pub(crate) fn flush(&mut self,mesh:&crate::rrls::Shared,now:u64){self.0.flush_mesh(mesh,now)}
}
