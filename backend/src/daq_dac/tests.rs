use super::wire::ChannelStatus;
use super::*;

fn status(gate: u8, active: [bool; 2]) -> Status {
    let mut s = Status {
        flight_state: 1,
        gate,
        flags: wire::FLAG_HARDWARE_READY | wire::FLAG_READBACK,
        uptime_ms: 1000,
        channels: [ChannelStatus::default(); 2],
    };
    for (c, a) in active.into_iter().enumerate() {
        s.channels[c].active = a;
    }
    s
}

fn dc_request(channel: u8, offset_v: f64) -> ConfigRequest {
    ConfigRequest {
        channel,
        waveform: Waveform::Dc,
        amplitude_v: 0.0,
        offset_v,
        frequency_hz: None,
        steps: None,
        dwell_ms: None,
    }
}

#[test]
fn enable_gate_names_every_refusal_and_allows_startup_and_idle() {
    let ok = status(0, [false; 2]);
    for state in [FlightState::Startup, FlightState::Idle] {
        assert_eq!(enable_refusal(state, Some((&ok, 100))), None, "{state:?}");
    }
    for state in [
        FlightState::PreFill,
        FlightState::NitrousFill,
        FlightState::Armed,
        FlightState::Launch,
        FlightState::Aborted,
    ] {
        let r = enable_refusal(state, Some((&ok, 100))).unwrap();
        assert_eq!(r.status, 409);
        assert!(r.message.contains(&format!("{state:?}")), "{}", r.message);
        assert!(r.message.contains("only in Startup or Idle"));
    }
    let r = enable_refusal(FlightState::NitrousFill, None).unwrap();
    assert_eq!(r.status, 409, "flight state wins over missing status");

    let r = enable_refusal(FlightState::Idle, None).unwrap();
    assert_eq!(r.status, 503);
    assert!(r.message.contains("No DAQ DAC status"));
    let r = enable_refusal(FlightState::Idle, Some((&ok, STATUS_FRESH_MS + 1))).unwrap();
    assert_eq!(r.status, 503);
    assert!(r.message.contains("stale (3.0 s old)"), "{}", r.message);
    assert!(enable_refusal(FlightState::Idle, Some((&ok, STATUS_FRESH_MS))).is_none());

    let mut refused = status(1, [false; 2]);
    refused.flight_state = 2; // Fill
    let r = enable_refusal(FlightState::Idle, Some((&refused, 0))).unwrap();
    assert_eq!(r.status, 409);
    assert!(
        r.message.contains("DAQ reports flight state Fill"),
        "{}",
        r.message
    );
    let unknown = status(2, [false; 2]);
    let r = enable_refusal(FlightState::Startup, Some((&unknown, 0))).unwrap();
    assert!(r.message.contains("does not know the flight state"));
    let odd = status(7, [false; 2]);
    assert!(
        enable_refusal(FlightState::Startup, Some((&odd, 0)))
            .unwrap()
            .message
            .contains("gate code 7")
    );
    let mut not_ready = status(0, [false; 2]);
    not_ready.flags = 0;
    let r = enable_refusal(FlightState::Startup, Some((&not_ready, 0))).unwrap();
    assert_eq!(r.status, 503);
    assert!(r.message.contains("not ready"));
}

#[test]
fn plan_command_converts_connector_volts_and_reports_clamps() {
    let (c, clamped) = plan_command(&dc_request(1, 2.0), 5).unwrap();
    assert_eq!(
        (c.channel, c.enable, c.sequence, c.lease_ms),
        (1, true, 5, 3000)
    );
    assert!((c.offset_v - 1.0).abs() < 1e-6 && c.amplitude_v == 0.0);
    assert!(clamped.is_empty());

    let (c, clamped) = plan_command(&dc_request(0, 0.1), 5).unwrap();
    assert!((c.offset_v - 0.2).abs() < 1e-6);
    assert_eq!(clamped, vec!["offset"]);

    let sine = ConfigRequest {
        channel: 0,
        waveform: Waveform::Sine,
        amplitude_v: 2.0,
        offset_v: 1.0,
        frequency_hz: Some(5000.0),
        steps: None,
        dwell_ms: None,
    };
    let (c, clamped) = plan_command(&sine, 5).unwrap();
    // Pin 0.5 V offset leaves 0.3 V of room below.
    assert!((c.amplitude_v - 0.3).abs() < 1e-6);
    assert_eq!(c.frequency_hz, 1000.0);
    assert_eq!(clamped, vec!["amplitude", "frequency"]);

    let stair = ConfigRequest {
        channel: 0,
        waveform: Waveform::Staircase,
        amplitude_v: 1.0,
        offset_v: 3.0,
        frequency_hz: None,
        steps: Some(64),
        dwell_ms: Some(5),
    };
    let (c, clamped) = plan_command(&stair, 5).unwrap();
    assert_eq!((c.steps, c.dwell_ms), (32, 10));
    assert_eq!(clamped, vec!["steps", "dwell"]);

    assert!(plan_command(&dc_request(2, 1.0), 1).is_err());
    assert!(plan_command(&dc_request(0, f64::NAN), 1).is_err());
    let mut negative = sine.clone();
    negative.amplitude_v = -1.0;
    assert!(plan_command(&negative, 1).is_err());
    let mut no_freq = sine.clone();
    no_freq.frequency_hz = None;
    assert!(plan_command(&no_freq, 1).is_err());
    let json: ConfigRequest = serde_json::from_str(
        r#"{"channel":0,"waveform":"sine","amplitude_v":1.0,"offset_v":2.0,"frequency_hz":0.5,"steps":8,"dwell_ms":1000}"#,
    )
    .unwrap();
    assert_eq!(json.waveform, Waveform::Sine);
}

struct Rig {
    rt: Runtime,
    t0: Instant,
}
impl Rig {
    fn new() -> Self {
        Self {
            rt: Runtime::default(),
            t0: Instant::now(),
        }
    }
    fn at(&self, ms: u64) -> Instant {
        self.t0 + Duration::from_millis(ms)
    }
    fn tick(&self, ms: u64, state: FlightState, status: Option<(&Status, u64)>) -> TickPlan {
        self.tick_with(ms, state, false, true, status)
    }
    fn tick_with(
        &self,
        ms: u64,
        flight_state: FlightState,
        abort_latched: bool,
        admission_open: bool,
        status: Option<(&Status, u64)>,
    ) -> TickPlan {
        self.rt.plan_tick(TickInput {
            now: self.at(ms),
            flight_state,
            abort_latched,
            admission_open,
            status,
        })
    }
    fn enable(&self, ms: u64, channel: u8) -> Command {
        let (mut command, _) = plan_command(&dc_request(channel, 2.0), 1).unwrap();
        command.sequence = self.rt.next_sequence();
        self.rt
            .set_desired(dc_request(channel, 2.0), command, self.at(ms));
        command
    }
}

#[test]
fn keepalive_resends_the_same_sequence_about_every_second() {
    let rig = Rig::new();
    let s = status(0, [true, false]);
    let command = rig.enable(0, 0);
    assert!(
        rig.tick(200, FlightState::Idle, Some((&s, 100)))
            .send
            .is_empty()
    );
    let plan = rig.tick(1000, FlightState::Idle, Some((&s, 100)));
    assert_eq!(plan.send, vec![command]);
    assert!(
        rig.tick(1600, FlightState::Idle, Some((&s, 100)))
            .send
            .is_empty()
    );
    let plan = rig.tick(2000, FlightState::Idle, Some((&s, 100)));
    assert_eq!(plan.send.len(), 1);
    assert_eq!(plan.send[0].sequence, command.sequence);
    assert!(plan.warnings.is_empty());
}

fn assert_drops(rig: &Rig, plan: TickPlan, text: &str) {
    assert_eq!(plan.warnings.len(), 1, "{plan:?}");
    assert!(plan.warnings[0].contains(text), "{:?}", plan.warnings);
    assert!(plan.send.iter().all(|c| !c.enable));
    assert!(!plan.send.is_empty());
    assert!(rig.rt.desired_request(0).is_none());
    assert!(rig.rt.last_drop().unwrap().contains(text));
}

#[test]
fn keepalive_drops_output_on_state_change_abort_stale_status_and_daq_gate() {
    let s = status(0, [true, false]);
    let rig = Rig::new();
    rig.enable(0, 0);
    let plan = rig.tick(200, FlightState::PreFill, Some((&s, 0)));
    assert_drops(&rig, plan, "Flight state is PreFill");

    let rig = Rig::new();
    rig.enable(0, 0);
    let plan = rig.tick_with(200, FlightState::Idle, true, true, Some((&s, 0)));
    assert_drops(&rig, plan, "abort");

    // An abort the GS latches is also delivered directly.
    let rig = Rig::new();
    rig.enable(0, 0);
    rig.rt.note_abort();
    let plan = rig.tick(200, FlightState::Idle, Some((&s, 0)));
    assert_drops(&rig, plan, "abort");

    let rig = Rig::new();
    rig.enable(0, 0);
    let plan = rig.tick(200, FlightState::Idle, Some((&s, 3_500)));
    assert_drops(&rig, plan, "stale");
    let rig = Rig::new();
    rig.enable(0, 0);
    let plan = rig.tick(200, FlightState::Idle, None);
    assert_drops(&rig, plan, "lost");

    let rig = Rig::new();
    rig.enable(0, 0);
    let mut gated = s;
    gated.gate = 1;
    gated.flight_state = 2;
    let plan = rig.tick(200, FlightState::Idle, Some((&gated, 0)));
    assert_drops(&rig, plan, "DAQ flight state Fill");

    let rig = Rig::new();
    rig.enable(0, 0);
    let plan = rig.tick_with(200, FlightState::Idle, false, false, Some((&s, 0)));
    assert_drops(&rig, plan, "closed");

    // A latch that was already set when the output started is not a new abort.
    let rig = Rig::new();
    rig.tick_with(
        0,
        FlightState::Idle,
        true,
        true,
        Some((&status(0, [false; 2]), 0)),
    );
    rig.enable(100, 0);
    let plan = rig.tick_with(200, FlightState::Idle, true, true, Some((&s, 0)));
    assert!(plan.warnings.is_empty());
}

#[test]
fn disable_repeats_until_status_confirms_or_times_out() {
    let rig = Rig::new();
    rig.enable(0, 0);
    let command = rig.rt.request_disable(Some(0), rig.at(100));
    assert!(!command.enable && command.channel == 0);
    let on = status(0, [true, false]);
    // Status still shows it on (sent before the disable): keep repeating.
    assert!(
        rig.tick(200, FlightState::Idle, Some((&on, 50)))
            .send
            .is_empty()
    );
    let plan = rig.tick(400, FlightState::Idle, Some((&on, 50)));
    assert_eq!(plan.send, vec![Command::disable(0, command.sequence)]);
    // A status received after the request shows it off: done.
    let off = status(0, [false; 2]);
    assert!(
        rig.tick(800, FlightState::Idle, Some((&off, 100)))
            .send
            .is_empty()
    );
    assert!(
        rig.tick(1200, FlightState::Idle, Some((&off, 100)))
            .send
            .is_empty()
    );

    // With no confirming status it gives up after a few seconds.
    let rig = Rig::new();
    let command = rig.rt.request_disable(None, rig.at(0));
    assert_eq!(command.channel, wire::CHANNEL_ALL);
    let plan = rig.tick(300, FlightState::Launch, None);
    assert_eq!(
        plan.send,
        vec![Command::disable(wire::CHANNEL_ALL, command.sequence)]
    );
    let mut sent = 1;
    for ms in (600..6000).step_by(300) {
        sent += rig.tick(ms, FlightState::Launch, None).send.len();
    }
    assert!((15..=25).contains(&sent), "{sent}");
    assert!(rig.tick(7000, FlightState::Launch, None).send.is_empty());
}

#[test]
fn startup_turns_off_outputs_the_gs_is_not_driving() {
    let rig = Rig::new();
    let on = status(0, [true, true]);
    let plan = rig.tick(0, FlightState::Idle, Some((&on, 10)));
    assert_eq!(plan.send.len(), 1);
    assert_eq!(plan.send[0].channel, wire::CHANNEL_ALL);
    assert!(!plan.send[0].enable);
    // Stale status is not acted on (it may be old), but the keepalive does not
    // re-enable anything either.
    let rig = Rig::new();
    assert!(
        rig.tick(0, FlightState::Idle, Some((&on, 10_000)))
            .send
            .is_empty()
    );
    // A channel the GS just stopped wanting gets the grace period first.
    let rig = Rig::new();
    rig.enable(0, 0);
    let one = status(0, [true, false]);
    rig.tick(100, FlightState::Idle, Some((&one, 10)));
    {
        let mut inner = rig.rt.inner.lock().unwrap();
        inner.channels[0].desired = None;
    }
    assert!(
        rig.tick(1000, FlightState::Idle, Some((&one, 10)))
            .send
            .is_empty()
    );
    let plan = rig.tick(3500, FlightState::Idle, Some((&one, 10)));
    assert_eq!(plan.send, vec![Command::disable(0, plan.send[0].sequence)]);
}

#[test]
fn validation_collects_only_settled_samples_of_the_confirmed_configuration() {
    let rt = Runtime::default();
    let t0 = Instant::now();
    {
        let mut inner = rt.inner.lock().unwrap();
        inner.validation = Some(Validation {
            target: model::target("mcp_ch2").unwrap(),
            channel: 0,
            attenuation: 1.0,
            polarity: Polarity::SMinus,
            samples: VecDeque::new(),
            wave: None,
            sequence: None,
            last_primary_ms: None,
        });
        rt.refresh_collecting(&inner);
    }
    assert!(rt.collecting.load(Ordering::Acquire));
    let mut s = status(0, [true, false]);
    s.channels[0].sequence = 7;
    s.channels[0].offset_v = 1.0;
    rt.on_status(&s, t0, 1_000);
    let sdadc = [Some(2.0f32), Some(0.0), Some(0.0), Some(0.0), None, None];
    // Within the settle time: ignored.
    rt.on_values(
        "DAQ_SDADC_VOLTAGES",
        &sdadc,
        1_100,
        t0 + Duration::from_millis(100),
    );
    // Other streams: ignored.
    rt.on_values(
        "DAQ_SAR_VOLTAGES",
        &[Some(1.0); 8],
        1_700,
        t0 + Duration::from_millis(700),
    );
    rt.on_values(
        "DAQ_SDADC_VOLTAGES",
        &sdadc,
        1_700,
        t0 + Duration::from_millis(700),
    );
    rt.on_values(
        "DAQ_SDADC_VOLTAGES",
        &sdadc,
        1_950,
        t0 + Duration::from_millis(950),
    );
    {
        let inner = rt.inner.lock().unwrap();
        let v = inner.validation.as_ref().unwrap();
        assert_eq!(v.samples.len(), 2);
        assert_eq!(v.wave.unwrap().offset_v, 1.0);
    }
    // A new configuration resets the series.
    s.channels[0].sequence = 8;
    rt.on_status(&s, t0 + Duration::from_secs(1), 2_000);
    assert!(
        rt.inner
            .lock()
            .unwrap()
            .validation
            .as_ref()
            .unwrap()
            .samples
            .is_empty()
    );
    // Output off: nothing more is collected.
    s.channels[0].active = false;
    rt.on_status(&s, t0 + Duration::from_secs(2), 3_000);
    rt.on_values(
        "DAQ_SDADC_VOLTAGES",
        &sdadc,
        3_900,
        t0 + Duration::from_secs(3),
    );
    assert!(
        rt.inner
            .lock()
            .unwrap()
            .validation
            .as_ref()
            .unwrap()
            .samples
            .is_empty()
    );
}

#[test]
fn fallback_source_counts_only_while_the_primary_is_silent() {
    let mut last = None;
    assert!(accept_source(&mut last, true, 0));
    assert!(accept_source(&mut last, false, 100));
    assert!(!accept_source(&mut last, true, 1_000));
    assert!(accept_source(&mut last, true, 2_200));
}

#[test]
fn validation_view_aligns_expected_and_warns_about_rate_and_attenuation() {
    let target = model::target("mcp_ch2").unwrap();
    let wave = Wave {
        waveform: Waveform::Sine,
        offset_v: 0.6,
        amplitude_v: 0.2,
        frequency_hz: 2.0,
        steps: 0,
        dwell_ms: 0,
    };
    let samples: Vec<(i64, f64)> = (0..120)
        .map(|i| {
            let t = 1_000_000 + i * 250;
            let frac = t as f64 / 1000.0 * 2.0 + 0.1;
            (t, 2.0 * wave.pin_at(frac))
        })
        .collect();
    let snap = api::Snapshot {
        target,
        channel: 0,
        attenuation: 1.0,
        polarity: Polarity::SMinus,
        samples: samples.clone(),
        wave: Some(wave),
        active: true,
    };
    let now_ms = samples.last().unwrap().0 + 100;
    let (series, analysis, warnings) = api::evaluate(&snap, true, now_ms);
    assert_eq!(series.t_ms.len(), 120);
    assert_eq!(series.expected.len(), 120);
    assert_eq!(analysis.unwrap().samples, 120);
    assert_eq!(
        warnings,
        vec!["Frequency 2 Hz is above a quarter of the 4 Hz stream rate.".to_string()]
    );

    let p6 = api::Snapshot {
        target: model::target("p6_diff").unwrap(),
        channel: 0,
        attenuation: 1.0,
        polarity: Polarity::SMinus,
        samples: Vec::new(),
        wave: Some(Wave::dc(0.5)),
        active: true,
    };
    let (series, analysis, warnings) = api::evaluate(&p6, true, now_ms);
    assert!(series.t_ms.is_empty() && analysis.is_none());
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("needs an external attenuator"))
    );
    assert!(warnings.iter().any(|w| w.contains("valid range")));
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("No samples") && w.contains("P6 differential"))
    );
}

#[test]
fn sweep_report_passes_or_fails_against_path_tolerances() {
    use analysis::SweepPoint;
    let target = model::target("mcp_ch2").unwrap();
    let points = |gain: f64| {
        [0.2, 0.4, 0.6, 0.8, 1.0, 1.14]
            .iter()
            .map(|pin| SweepPoint {
                dac_pin_v: *pin,
                expected: 2.0 * pin,
                measured_mean: 2.0 * pin * gain + 0.001,
                measured_std: 0.0005,
                readback_v: Some(*pin),
                samples: 12,
            })
            .collect::<Vec<_>>()
    };
    let ok = sweep::build_report(
        target,
        0,
        1.0,
        Polarity::SMinus,
        points(1.002),
        1,
        2,
        sweep::State::Done,
        None,
    );
    assert!(ok.pass, "{ok:?}");
    assert!((ok.gain_error_pct.unwrap() - 0.2).abs() < 1e-6);
    assert_eq!(ok.checks.len(), 4);
    let bad = sweep::build_report(
        target,
        0,
        1.0,
        Polarity::SMinus,
        points(1.05),
        1,
        2,
        sweep::State::Done,
        None,
    );
    assert!(!bad.pass);
    assert!(bad.checks.iter().any(|c| c.name == "gain" && !c.pass));
    // A cancelled sweep never passes, and too few points leave the fit empty.
    let cancelled = sweep::build_report(
        target,
        0,
        1.0,
        Polarity::SMinus,
        points(1.0),
        1,
        2,
        sweep::State::Cancelled,
        None,
    );
    assert!(!cancelled.pass);
    let short = sweep::build_report(
        target,
        0,
        1.0,
        Polarity::SMinus,
        points(1.0)[..1].to_vec(),
        1,
        2,
        sweep::State::Failed,
        Some("x".into()),
    );
    assert!(short.offset_error.is_none() && short.checks.is_empty() && !short.pass);
    let json = serde_json::to_value(&ok).unwrap();
    for key in [
        "target",
        "channel",
        "attenuation",
        "started_utc_ms",
        "finished_utc_ms",
        "points",
        "offset_error",
        "gain_error_pct",
        "max_nonlinearity_pct_fs",
        "noise_rms",
        "tolerances",
        "checks",
        "pass",
        "saved_path",
    ] {
        assert!(json.get(key).is_some(), "{key}");
    }
    assert_eq!(json["tolerances"]["gain_pct"], 1.0);
    assert!(json["points"][0].get("readback_v").is_some());
}

#[test]
fn disable_channel_parsing_and_write_authorization() {
    let p = |s: &str| api::parse_disable_channel(&serde_json::from_str(s).unwrap());
    assert_eq!(p(r#"{"channel":0}"#), Ok(Some(0)));
    assert_eq!(p(r#"{"channel":1}"#), Ok(Some(1)));
    assert_eq!(p(r#"{"channel":"all"}"#), Ok(None));
    assert!(p(r#"{"channel":2}"#).is_err());
    assert!(p(r#"{}"#).is_err());
    use axum::http::StatusCode;
    assert_eq!(
        api::local_authorization(true, true, false).unwrap_err().0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        api::local_authorization(false, false, false).unwrap_err().0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        api::local_authorization(false, true, true).unwrap_err().0,
        StatusCode::FORBIDDEN
    );
    assert!(api::local_authorization(false, true, false).is_ok());
    assert!(api::check_selection("mcp_ch2", 0, 1.0).is_ok());
    assert!(api::check_selection("nope", 0, 1.0).is_err());
    assert!(api::check_selection("mcp_ch2", 2, 1.0).is_err());
    assert!(api::check_selection("mcp_ch2", 0, 0.0).is_err());
    assert!(api::check_selection("mcp_ch2", 0, 1.5).is_err());
}

#[test]
fn command_packet_is_32_bytes_to_sd_card_and_rejects_sequence_zero() {
    crate::telemetry_schema::initialize().unwrap();
    let (mut command, _) = plan_command(&dc_request(0, 2.0), 1).unwrap();
    command.sequence = 0;
    assert!(command_packet(&command).is_err());
    command.sequence = 42;
    let packet = command_packet(&command).unwrap();
    assert_eq!(packet.data_type().as_u32(), 206667655);
    assert_eq!(
        packet.endpoints(),
        &[crate::telemetry_schema::endpoint("SD_CARD")]
    );
    assert_eq!(packet.payload(), &command.encode());
    let router =
        sedsnet::router::Router::new(sedsnet::router::RouterConfig::new([]).with_sender("GS"));
    send(&router, &command).unwrap();
    assert_eq!(router.export_runtime_stats().queues.tx_len, 1);
}

#[test]
fn status_overlay_marks_dac_activity_in_board_diagnostics() {
    let mut report = BoardDiagnostics::default();
    status(0, [false, true]).overlay(&mut report, 250);
    assert_eq!(report.dac_active, Some(true));
    assert_eq!(report.dac_status_received_age_ms, Some(250));
    let mut tracker = crate::network_health::Tracker::default();
    let now = Instant::now();
    let bytes = status(0, [true, false]).encode();
    assert!(!tracker.observe_dac_status("FC", &bytes, now, 1));
    assert!(!tracker.observe_dac_status("DAQ", &bytes[..79], now, 1));
    assert!(tracker.observe_dac_status("DAQ", &bytes, now, 1));
    let (s, age) = tracker
        .daq_dac_status(now + Duration::from_millis(40))
        .unwrap();
    assert!(s.channels[0].active);
    assert_eq!(age, 40);
    let rows = tracker.snapshot(now, false);
    let daq = rows.iter().find(|r| r.sender_id == "DAQ").unwrap();
    assert_eq!(daq.diagnostics.as_ref().unwrap().dac_active, Some(true));
}

#[tokio::test]
async fn api_gates_enable_never_gates_disable_and_answers_json() {
    use crate::recording_export::tests::{TestDirectory, export_state};
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    let root = TestDirectory::new();
    let state = export_state(&root.0, true).await;
    std::fs::write(
        root.0.join("auth.json"),
        serde_json::json!({
            "version":1,"anonymous":{"view_data":true,"send_commands":false},
            "users":[{"username":"dac-operator","password":{"iterations":120000,"salt_b64":"","hash_b64":""},
                "permissions":{"view_data":true,"send_commands":true},
                "command_access":{"allowed_commands":[COMMAND_NAME]}}]
        })
        .to_string(),
    )
    .unwrap();
    let now = crate::telemetry_task::get_current_timestamp_ms() as i64;
    sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES('dac-test','dac-operator','session',1,1,?,?)")
        .bind(now).bind(now + 60000).execute(&state.auth_db).await.unwrap();
    let router = Arc::new(sedsnet::router::Router::new(
        sedsnet::router::RouterConfig::new([]).with_sender("GS"),
    ));
    assert!(state.topology_router.set(router.clone()).is_ok());
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        "Bearer dac-test".parse().unwrap(),
    );
    let body = |s: &str| axum::body::Bytes::from(s.to_string());
    let json = |r: axum::response::Response| async move {
        let status = r.status();
        let bytes = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (
            status,
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
        )
    };
    let config = r#"{"channel":0,"waveform":"dc","amplitude_v":0,"offset_v":2.0,"frequency_hz":0,"steps":2,"dwell_ms":1000}"#;

    // Anonymous writes are refused with a JSON error.
    let (code, value) =
        json(api::post_config(State(state.clone()), HeaderMap::new(), body(config)).await).await;
    assert!(matches!(
        code,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ));
    assert!(value["error"].is_string());
    // No DAQ status: 503.
    let (code, value) =
        json(api::post_config(State(state.clone()), headers.clone(), body(config)).await).await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "{value}");
    assert!(
        value["error"]
            .as_str()
            .unwrap()
            .contains("No DAQ DAC status")
    );
    state.network_health.lock().unwrap().observe_dac_status(
        "DAQ",
        &status(0, [false; 2]).encode(),
        Instant::now(),
        now as u64,
    );
    // Wrong flight state: 409 naming it.
    *state.state.lock().unwrap() = FlightState::NitrousFill;
    let (code, value) =
        json(api::post_config(State(state.clone()), headers.clone(), body(config)).await).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert!(value["error"].as_str().unwrap().contains("NitrousFill"));
    // Bad values: 400.
    *state.state.lock().unwrap() = FlightState::Idle;
    let (code, _) = json(
        api::post_config(
            State(state.clone()),
            headers.clone(),
            body(r#"{"channel":5,"waveform":"dc","offset_v":1}"#),
        )
        .await,
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    // Allowed in Idle: queued once, kept as desired.
    let (code, value) =
        json(api::post_config(State(state.clone()), headers.clone(), body(config)).await).await;
    assert_eq!(code, StatusCode::ACCEPTED, "{value}");
    assert_eq!(value["accepted"], true);
    assert!(value["sequence"].as_u64().unwrap() > 0);
    assert!((value["dac_pin"]["offset_v"].as_f64().unwrap() - 1.0).abs() < 1e-6);
    assert_eq!(router.export_runtime_stats().queues.tx_len, 1);
    let (code, view) = json(api::get_status(State(state.clone()), headers.clone()).await).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(view["allowed"], true);
    assert_eq!(view["flight_state"], "Idle");
    assert_eq!(view["daq_flight_state"], 1);
    assert_eq!(view["any_active"], true);
    assert_eq!(view["channels"][0]["desired"]["offset_v"], 2.0);
    assert_eq!(view["connector_range_v"][1], 6.2);
    assert!(
        view["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["id"] == "loadcell_kg1000" && i["needs_attenuator"] == true)
    );
    // Disable works in any flight state.
    *state.state.lock().unwrap() = FlightState::Launch;
    let (code, value) = json(
        api::disable(
            State(state.clone()),
            headers.clone(),
            body(r#"{"channel":"all"}"#),
        )
        .await,
    )
    .await;
    assert_eq!(code, StatusCode::ACCEPTED, "{value}");
    assert!(state.daq_dac.desired_request(0).is_none());
    // Validation selection is not gated by flight state.
    let (code, value) = json(api::post_validation(State(state.clone()), headers.clone(), body(r#"{"target":"loadcell_kg50","channel":1,"attenuation":0.005,"load_cell_polarity":"s_plus"}"#)).await).await;
    assert_eq!(code, StatusCode::OK, "{value}");
    assert_eq!(value["target"], "loadcell_kg50");
    assert_eq!(value["load_cell_polarity"], "s_plus");
    assert!(value["series"]["t_ms"].as_array().unwrap().is_empty());
    let (code, value) = json(
        api::post_validation(
            State(state.clone()),
            headers.clone(),
            body(r#"{"target":null}"#),
        )
        .await,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert!(value["target"].is_null());
    // Sweep refused outside Startup/Idle; cancel answers JSON.
    let (code, value) = json(api::post_sweep(State(state.clone()), headers.clone(), body(r#"{"target":"mcp_ch2","channel":0,"attenuation":1.0,"load_cell_polarity":"s_minus"}"#)).await).await;
    assert_eq!(code, StatusCode::CONFLICT, "{value}");
    let (code, value) = json(api::sweep_cancel(State(state.clone()), headers.clone()).await).await;
    assert_eq!(code, StatusCode::ACCEPTED);
    assert_eq!(value["state"], "idle");
}
