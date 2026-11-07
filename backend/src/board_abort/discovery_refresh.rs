//! Restore expiring endpoint advertisements without probing command endpoints.
use super::{BOARDS, AppState, Duration, Instant};

const REFRESH_BEFORE_MS: u64 = 6_000;
const REQUEST_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Default)]
pub(super) struct DiscoveryRefresh {
    last: Option<Instant>,
}

impl DiscoveryRefresh {
    pub(super) fn poll(&mut self, state: &AppState) -> bool {
        self.poll_at(state, Instant::now())
    }

    fn poll_at(&mut self, state: &AppState, now: Instant) -> bool {
        if crate::rrls::is_rrls()
            || !crate::board_recovery::coordinator::readonly_query_idle(state)
            || self.last.is_some_and(|last| {
                now.checked_duration_since(last)
                    .is_none_or(|age| age < REQUEST_INTERVAL)
            })
        {
            return false;
        }
        let Some(router) = state.topology_router.get() else {
            return false;
        };
        if router.sender().as_ref() != "GS" {
            return false;
        }
        let topology = router.export_topology();
        let health = state.network_health.lock().unwrap()
            .snapshot(now, cfg!(feature = "test_fire_mode"));
        let needed = BOARDS.iter().any(|&board| {
            let connected = health.iter().any(|row| {
                row.sender_id == board.sender_id() && row.healthy
            });
            connected && advertisement_needs_refresh(
                crate::board_recovery::endpoint_advertisement_age(state, board, &topology),
            )
        });
        if !needed {
            return false;
        }
        // Bound failed attempts too. Only the SDK's built-in discovery request
        // is emitted: no QUERY to an unverified device endpoint, SET or motion.
        self.last = Some(now);
        router.request_topology().is_ok()
    }
}

fn advertisement_needs_refresh(age_ms: Option<u64>) -> bool {
    age_ms.is_none_or(|age| age >= REFRESH_BEFORE_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_is_requested_before_the_existing_identity_window_expires() {
        assert!(!advertisement_needs_refresh(Some(5_999)));
        assert!(advertisement_needs_refresh(Some(6_000)));
        assert!(advertisement_needs_refresh(Some(12_001)));
        assert!(advertisement_needs_refresh(None));
    }

    #[tokio::test]
    async fn actual_sdk_discovery_is_bounded_and_never_queues_an_operator_command() {
        let mut fixture = crate::actuator_plumbing::http_tests::Fixture::new().await;
        let mut refresh = DiscoveryRefresh::default();
        let start = Instant::now();
        assert!(!refresh.poll_at(&fixture.state, start), "unseen peers must not cause discovery traffic");
        // A seen peer with no current endpoint advertisement cannot be queried
        // through a made-up legacy endpoint. Ask the SDK for topology instead.
        fixture.state.network_health.lock().unwrap().observe("VB", start, 100);
        assert!(refresh.poll_at(&fixture.state, start));
        assert!(!refresh.poll_at(&fixture.state, start + Duration::from_millis(4_999)));
        fixture.pump();
        let stats = fixture.host.export_runtime_stats();
        assert!(stats.sides.iter().flat_map(|side| side.data_types.iter()).any(|ty| {
            ty.data_type == sedsnet::config::DataType::DiscoveryTopologyRequest && ty.tx_packets > 0
        }), "the actual SDK must emit its built-in topology request");
        assert!(fixture.cmd_rx.as_mut().unwrap().try_recv().is_err());
        assert!(!refresh.poll_at(&fixture.state, start + Duration::from_secs(30)), "disconnected peers do not trigger indefinite probes");
    }
}
