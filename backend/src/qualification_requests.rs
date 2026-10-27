//! Opt-in simulator evidence. No seed, UART or UTC timestamp substitutes for an API request.
use sedsnet::{packet::Packet, router::Router};
use std::sync::atomic::{AtomicU64, Ordering};

thread_local! { static CONTROLLED_ROUND: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) }; }
pub(crate) fn in_controlled_round<T>(round: usize, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<usize>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CONTROLLED_ROUND.with(|slot| slot.set(self.0));
        }
    }
    let _restore = Restore(CONTROLLED_ROUND.with(|slot| slot.replace(Some(round))));
    run()
}
pub(crate) struct Request {
    token: Option<u64>,
    submissions: std::cell::Cell<u64>,
}
impl Request {
    pub(crate) fn begin(publisher: &'static str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let token = (cfg!(any(feature = "hitl_mode", feature = "firmware_sim"))
            && std::env::var("GS_SIM_TRACE_API_REQUESTS").as_deref() == Ok("1"))
        .then(|| NEXT.fetch_add(1, Ordering::Relaxed));
        if let Some(token) = token {
            emit(
                serde_json::json!({"event":"begin","token":token,"publisher":publisher,"request_ms":crate::simulation_clock_ms(),"controlled_round":CONTROLLED_ROUND.with(|slot|slot.get())}),
            );
        }
        Self {
            token,
            submissions: std::cell::Cell::new(0),
        }
    }
    pub(crate) fn submit(&self, router: &Router, packet: Packet) -> anyhow::Result<()> {
        if self.token.is_none() {
            return router.set_network_variable(packet).map_err(Into::into);
        }
        let id = packet.packet_id();
        if let Some(token) = self.token {
            emit(
                serde_json::json!({"event":"bind","token":token,"packet_id":id,"submit_ms":crate::simulation_clock_ms(),"data_type":packet.data_type().as_u32()}),
            );
            self.submissions.set(self.submissions.get() + 1);
        }
        let result = router.set_network_variable(packet);
        if let Some(token) = self.token {
            emit(
                serde_json::json!({"event":"submit_result","token":token,"packet_id":id,"ok":result.is_ok(),"at_ms":crate::simulation_clock_ms()}),
            );
        }
        result.map_err(Into::into)
    }
}
impl Drop for Request {
    fn drop(&mut self) {
        if let Some(token) = self.token {
            emit(
                serde_json::json!({"event":"return","token":token,"submissions":self.submissions.get(),"at_ms":crate::simulation_clock_ms()}),
            );
        }
    }
}
fn emit(event: serde_json::Value) {
    eprintln!("[gs-qualification-request] {event}");
}

pub(crate) fn policy_states(hitl: bool, requested: &[u8]) -> anyhow::Result<Vec<u8>> {
    if hitl {
        return Ok(requested.to_vec());
    }
    anyhow::ensure!(
        requested.iter().all(|state| matches!(state, 0 | 1)),
        "normal qualification must remain within nonactuating prelaunch policy"
    );
    Ok(vec![1; requested.len()])
}
#[cfg(test)]
mod tests {
    #[test]
    fn normal_stable_idle_and_hitl_state_exercise_preserve_cosmetic_round_count() {
        assert_eq!(
            super::policy_states(false, &[1, 0, 1]).unwrap(),
            vec![1, 1, 1]
        );
        assert_eq!(
            super::policy_states(true, &[1, 0, 1]).unwrap(),
            vec![1, 0, 1]
        );
        assert!(super::policy_states(false, &[2]).is_err());
    }
}
