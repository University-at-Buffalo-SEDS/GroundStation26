//! Primary side of the command proxy for the RRLS simulated primary.
//! It answers capability/observation queries with an all-unknown idle
//! observation and answers every forwarded command as Unavailable. Nothing is
//! ever executed, queued or dispatched.
// The simulated primary runs only where a pseudo-terminal radio exists (Linux).
#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]
use super::{Received, Runtime, Transport, wire::Outcome};
use crate::{rrls::Role, station_mode::Mode, station_pairing::Pairing};

pub(crate) struct SimulatedPrimaryProxy {
    runtime: Runtime,
    pub(crate) refused_commands: u64,
}
impl SimulatedPrimaryProxy {
    pub(crate) fn new() -> Self {
        let mut runtime = Runtime::new(Role::Primary);
        runtime.observation = crate::primary_mirror::simulated_idle_observation();
        Self {
            runtime,
            refused_commands: 0,
        }
    }
    /// One verified frame from the RRLS. Returns the signed reply, if any.
    pub(crate) fn receive(
        &mut self,
        pairing: &Pairing,
        transport: Option<Transport>,
        mode: Mode,
        frame: &[u8],
    ) -> Option<Vec<u8>> {
        self.runtime.connect(transport);
        match self.runtime.receive(pairing, transport?, mode, frame) {
            Ok(Received::Reply(reply)) => Some(reply),
            Ok(Received::Execute(request)) => {
                self.refused_commands = self.refused_commands.saturating_add(1);
                log::info!(
                    "Simulated primary refused forwarded {:?} from {}: no command is executed in RRLS simulation",
                    request.operation,
                    request.actor
                );
                self.runtime
                    .finish(pairing, request, Outcome::Unavailable)
                    .ok()
            }
            Ok(_) => None,
            Err(error) => {
                log::debug!("Simulated primary ignored a command-proxy frame: {error}");
                None
            }
        }
    }
    pub(crate) fn connect(&mut self, transport: Option<Transport>) {
        self.runtime.connect(transport);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    /// The RRLS half, driven exactly as the RRLS worker drives it: observe
    /// first, then forward one manual command once the capability is verified.
    pub(crate) struct RrlsProxy {
        runtime: Runtime,
        requested: bool,
    }
    impl RrlsProxy {
        pub(crate) fn new() -> Self {
            Self {
                runtime: Runtime::new(Role::Rrls),
                requested: false,
            }
        }
        pub(crate) fn next_frame(&mut self, pairing: &Pairing, transport: Transport) -> Option<Vec<u8>> {
            self.runtime.connect(Some(transport));
            if self.runtime.capability.is_none() {
                if self.runtime.query.is_some() {
                    return None;
                }
                return self.runtime.query_observe(pairing, transport).ok();
            }
            if self.requested {
                return None;
            }
            self.requested = true;
            self.runtime
                .request(
                    pairing,
                    transport,
                    crate::station_mode::local_mode(),
                    super::super::nonce(),
                    "sim-operator".into(),
                    super::super::Operation::Command("Abort".into()),
                )
                .ok()
        }
        /// `Ok(observation)` for a capability, `Err(outcome)` for a result.
        pub(crate) fn receive(
            &mut self,
            pairing: &Pairing,
            transport: Transport,
            frame: &[u8],
        ) -> Option<Result<Vec<u8>, String>> {
            self.runtime.connect(Some(transport));
            match self
                .runtime
                .receive(pairing, transport, crate::station_mode::local_mode(), frame)
                .ok()?
            {
                Received::Capability { observation, .. } => Some(Ok(observation)),
                Received::Result { outcome, .. } => Some(Err(format!("{outcome:?}"))),
                _ => None,
            }
        }
    }
    #[test]
    fn simulated_proxy_never_executes_and_refuses_with_unavailable() {
        let (_root, p, r, _primary, mut rr, pv, rv) = super::super::tests::connected();
        let mut simulated = SimulatedPrimaryProxy::new();
        let query = rr.query(&r, rv).unwrap();
        let reply = simulated.receive(&p, Some(pv), crate::station_mode::local_mode(), &query).unwrap();
        assert!(matches!(
            rr.receive(&r, rv, crate::station_mode::local_mode(), &reply).unwrap(),
            Received::Capability { .. }
        ));
        assert_eq!(simulated.refused_commands, 0);
        let request = rr
            .request(
                &r,
                rv,
                crate::station_mode::local_mode(),
                [5; 16],
                "operator".into(),
                super::super::Operation::Command("Abort".into()),
            )
            .unwrap();
        let reply = simulated
            .receive(&p, Some(pv), crate::station_mode::local_mode(), &request)
            .unwrap();
        let Received::Result { outcome, .. } = rr
            .receive(&r, rv, crate::station_mode::local_mode(), &reply)
            .unwrap()
        else {
            panic!("refusal result")
        };
        assert_eq!(outcome, Outcome::Unavailable);
        assert_eq!(simulated.refused_commands, 1);
        // A disconnected transport yields nothing at all.
        assert!(simulated.receive(&p, None, crate::station_mode::local_mode(), &query).is_none());
    }
}
