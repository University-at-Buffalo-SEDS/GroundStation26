//! fullnet B-fix: 7 of 12 RRLS commands sent through the primary were refused
//! as "Stale primary connection contract". The RRLS seals each request with
//! its estimate of the primary's HEALTH sequence and the primary accepts it
//! only within SEQUENCE_WINDOW sequences. This replays the real mechanism:
//! the primary's mesh sends HEALTH every 500 ms, the RRLS's mesh hears it with
//! loss, the RRLS seals requests at arbitrary times and repeats them under its
//! own rule, each copy is lost or delayed on the air, and the primary's own
//! freshness check judges every copy that arrives.
use super::*;
use crate::rrls::{Mesh, Role as MeshRole};

fn envelope(session: u32, sequence: u32) -> wire::Envelope {
    wire::Envelope { issuer: "rrls".into(), session, sequence, request: [0; 16], boot: [0; 16], message: wire::Message::Query }
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn chance(&mut self, ppm: u64) -> bool {
        self.next() % 1_000_000 < ppm
    }
    fn between(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo + 1)
    }
}

#[derive(Default, Debug)]
struct Tally {
    sealed: u32,
    delivered: u32,
    accepted: u32,
    stale: u32,
}

/// `loss_ppm` applies to every HEALTH frame and every request copy.
/// `delay_*`: a request copy's time to the primary (queueing and air).
fn simulate_with(loss_ppm: u64, seed: u64, delay_lo: u64, delay_hi: u64) -> Tally {
    let mut rng = Lcg(seed);
    let mut primary = Mesh::new(MeshRole::Primary, 7);
    let mut rrls = Mesh::new(MeshRole::Rrls, 8);
    let mut tally = Tally::default();
    // (arrival ms, request id, sealed sequence)
    let mut in_air: Vec<(u64, u32, u32)> = Vec::new();
    // (request id, sealed sequence, sent copies, next copy ms, delivered, accepted)
    let mut pending: Option<(u32, u32, u8, u64, bool, bool)> = None;
    let mut next_command = 3_000;
    let mut id = 0u32;
    let end = 600_000;
    let mut now = 1_000;
    let finish = |p: (u32, u32, u8, u64, bool, bool), tally: &mut Tally| {
        if p.4 {
            tally.delivered += 1;
            if p.5 { tally.accepted += 1 } else { tally.stale += 1 }
        }
    };
    while now < end {
        primary.tick(now);
        while let Some(frame) = primary.pop(now) {
            if frame.bytes.get(3) == Some(&0) && !rng.chance(loss_ppm) {
                rrls.incoming(true, &frame.bytes, now);
            }
        }
        // Copies arriving at the primary now.
        let primary_view = primary.command_proxy_transport(now).unwrap();
        in_air.retain(|&(at, req, seq)| {
            if at > now { return true; }
            let accepted = primary_view.accepts(
                &envelope(primary_view.session, seq),
                true,
            );
            if let Some(p) = pending.as_mut().filter(|p| p.0 == req) {
                p.4 = true;
                p.5 |= accepted;
            }
            false
        });
        // A new operator press when idle.
        if pending.is_none() && now >= next_command {
            if let Some(view) = rrls.command_proxy_transport(now) {
                id += 1;
                tally.sealed += 1;
                pending = Some((id, view.sequence, 0, now, false, false));
            }
            next_command = now + rng.between(2_000, 6_000);
        }
        // The RRLS's copy rule (command_proxy submit loop).
        if let Some(mut p) = pending {
            let view = rrls.command_proxy_transport(now);
            let age = view.map(|v| v.sequence.wrapping_sub(p.1));
            let accepted_or_expired = p.5 || age.is_none_or(|age| age > LATE_REPLY_WINDOW);
            if accepted_or_expired {
                finish(p, &mut tally);
                pending = None;
            } else {
                let copy = view.and_then(|v| request_copy_sequence(p.1, v.sequence, p.2));
                if let Some(sealed) = copy.filter(|_| now >= p.3) {
                    if !rng.chance(loss_ppm) {
                        in_air.push((now + rng.between(delay_lo, delay_hi), p.0, sealed));
                    }
                    p.2 += 1;
                    p.3 = now + REQUEST_RESEND_MS + rng.between(0, 150);
                }
                pending = Some(p);
            }
        }
        now += 10;
    }
    tally
}

/// At 30% loss no request that reached the primary is refused as stale.
#[test]
fn requests_that_reach_the_primary_are_not_refused_as_stale_at_30_percent_health_loss() {
    for loss in [100_000, 300_000, 400_000] {
        let mut total = Tally::default();
        for seed in 1..=6 {
            // Copies queue and cross a busy channel for up to 1.2 s.
            let t = simulate_with(loss, seed, 60, 1_200);
            total.sealed += t.sealed;
            total.delivered += t.delivered;
            total.accepted += t.accepted;
            total.stale += t.stale;
        }
        println!("loss {}%: {total:?}", loss / 10_000);
        assert!(total.delivered > 300, "{total:?}");
        if loss <= 300_000 {
            assert_eq!(total.stale, 0, "loss {}%: {total:?}", loss / 10_000);
        }
    }
}

/// Replay protection is unchanged: a copy sealed more than SEQUENCE_WINDOW
/// HEALTH sequences before the primary's current one is refused.
#[test]
fn an_old_request_is_still_refused() {
    let mut primary = Mesh::new(MeshRole::Primary, 7);
    let mut now = 1_000;
    primary.tick(now);
    let sealed = primary.command_proxy_transport(now).unwrap().sequence.wrapping_sub(1);
    let fresh = |primary: &Mesh, now| primary.command_proxy_transport(now).unwrap()
        .accepts(&envelope(7, sealed), true);
    assert!(fresh(&primary, now), "sealed with the last HEALTH heard");
    while primary.command_proxy_transport(now).unwrap().sequence.wrapping_sub(sealed) <= SEQUENCE_WINDOW {
        now += 10;
        primary.tick(now);
        while primary.pop(now).is_some() {}
    }
    assert!(!fresh(&primary, now), "older than the replay window");
}

#[test]
#[ignore]
fn sweep() {
    for (lo, hi) in [(60, 450), (200, 1_200), (500, 2_000)] {
        for loss in [0, 100_000, 300_000, 400_000] {
            let mut t = Tally::default();
            for seed in 1..=6 { let r = simulate_with(loss, seed, lo, hi); t.sealed += r.sealed; t.delivered += r.delivered; t.accepted += r.accepted; t.stale += r.stale; }
            println!("delay {lo}-{hi} ms loss {}%: delivered {} stale {} ({:.1}%)", loss / 10_000, t.delivered, t.stale, 100.0 * t.stale as f64 / t.delivered.max(1) as f64);
        }
    }
}
