//! Actual Engine and Turbocharge controllers, with explicit fresh valve readbacks.
use super::*;
use crate::fill_resume::FillStart;
use crate::turbocharge::{Context, Controller, Evidence};
fn config() -> Config {
    Config { pressure_ceiling_psi: Some(900.0), maximum_zero_offset_psi: Some(5.0),
        turbocharge_max_added_mass_kg: Some(0.3), fill_settle_ms: 1000, ..Config::default() }
}
fn input(now: u64, pressure: f32) -> Inputs {
    Inputs { now_ms: now, pressure: Some(Sample { value: pressure, at_ms: now }),
        valves: [Some(ValveState { open: false, at_ms: now }); 5], interlock: true, prelaunch: true }
}
fn engine(paused_inventory: bool) -> Engine {
    let mut e=Engine { config: config(), nitrous_target_psi: 750., ..Engine::default() };
    if paused_inventory { e.prepare_startup(9000,true); e.status.phase=Phase::Paused; }
    else {e.status.phase=Phase::Passed;}
    e.status.nitrogen_passed=true;
    e.status.baseline=Some(Baseline {average_psi:0.,min_psi:0.,max_psi:0.,noise_psi:0.1,samples:50}); e
}
fn apply(effects: Effects, states: &mut [Option<ValveState>;5], now:u64, events:&mut Vec<(usize,bool)>) {
    for (i,open) in effects.valves { events.push((i,open));states[i]=Some(ValveState{open,at_ms:now+1}); }
}
fn drive_setup(e:&mut Engine, first: Effects, pressure:f32)->Vec<(usize,bool)> {
    let mut states=input(10_000,pressure).valves;let mut events=vec![];
    apply(first,&mut states,10_000,&mut events);
    for n in 1..=10 {
        let now=10_000+n;for v in states.iter_mut().flatten(){v.at_ms=now;}
        let effect=e.tick(Inputs{valves:states,..input(now,pressure)});
        apply(effect,&mut states,now,&mut events);
        if events.contains(&(4,true))||e.status.phase==Phase::Fault {break;}
    } events
}
#[test]
fn paused_inventory_resumes_under_pressure_through_real_five_valve_ack_setup() {
    let mut e=engine(true);
    let FillStart::Started(effects)=e.request_fill(input(10_000,500.),Some(Sample{value:7.7,at_ms:10_000}),8.).unwrap() else {panic!("resume was noop")};
    assert!(e.resume_existing_inventory&&e.nitrous_session());
    let events=drive_setup(&mut e,effects,500.);
    assert_eq!(e.status.phase,Phase::Filling);assert!(!e.resume_existing_inventory);
    assert_eq!(events.last(),Some(&(4,true)));assert_eq!(events.iter().filter(|&&(i,open)|i==4&&open).count(),1);
}
#[test]
fn initial_fill_does_not_borrow_its_new_session_to_bypass_delayed_zero_gate() {
    let mut e=engine(false);
    let FillStart::Started(effects)=e.request_fill(input(10_000,0.),Some(Sample{value:0.,at_ms:10_000}),8.).unwrap() else {panic!()};
    assert!(e.nitrous_session());assert!(!e.resume_existing_inventory);
    let events=drive_setup(&mut e,effects,500.);
    assert_eq!(e.status.phase,Phase::Fault);assert!(!events.contains(&(4,true)));
}
#[test]
fn pressure_freshness_ceiling_and_operating_guards_remain_at_resume_admission() {
    for bad in [f32::NAN,f32::INFINITY,-0.001,800.,900.] {
        assert!(engine(true).request_fill(input(10_000,bad),Some(Sample{value:7.7,at_ms:10_000}),8.).is_err(),"{bad}");
    }
    for bad in [Inputs{pressure:None,..input(10_000,500.)},Inputs{pressure:Some(Sample{value:500.,at_ms:7999}),..input(10_000,500.)},Inputs{pressure:Some(Sample{value:500.,at_ms:10_001}),..input(10_000,500.)},Inputs{interlock:false,..input(10_000,500.)},Inputs{prelaunch:false,..input(10_000,500.)}] {
        assert!(engine(true).request_fill(bad,Some(Sample{value:7.7,at_ms:10_000}),8.).is_err());
    }
    assert!(engine(false).request_fill(input(10_000,500.),Some(Sample{value:0.,at_ms:10_000}),8.).is_err());
}
#[test]
fn pressure_loss_or_ceiling_after_admission_never_opens_nitrous() {
    for pressure in [-1.,800.,f32::NAN] {
        let mut e=engine(true);
        let FillStart::Started(first)=e.request_fill(input(10_000,500.),Some(Sample{value:7.7,at_ms:10_000}),8.).unwrap() else {panic!()};
        let events=drive_setup(&mut e,first,pressure);
        assert_eq!(e.status.phase,Phase::Fault);assert!(!events.contains(&(4,true)));assert!(!e.resume_existing_inventory);
    }
}
#[test]
fn captured_resume_is_not_authority_after_cancel_reset_or_manual_override() {
    for action in 0..4 {
        let mut e=engine(true);e.request_fill(input(10_000,500.),Some(Sample{value:7.7,at_ms:10_000}),8.).unwrap();assert!(e.resume_existing_inventory);
        match action {0=>{e.request(Action::CancelFill,input(10_001,500.)).unwrap();},1=>{e.prepare_startup(10_001,false);},2=>e.manual_override(),_=>{e.fault(10_001,"test");}}
        assert!(!e.resume_existing_inventory);
    }
}
#[test]
fn full_inventory_band_is_asymmetric_signed_and_exact_resume_boundary_is_not_full() {
    for sign in [1.,-1.] {
        assert!(fill_resume::inventory_ready(7.9*sign,8.*sign,0.07));
        assert!(!fill_resume::inventory_ready(7.8*sign,8.*sign,0.07));
        assert!(fill_resume::resume_required(100,Some(Sample{value:7.8*sign,at_ms:100}),8.*sign).unwrap());
        assert!(!fill_resume::inventory_ready(8.1*sign,8.*sign,0.07));
        for bad in [f32::NAN,f32::INFINITY] {assert!(!fill_resume::inventory_ready(bad,8.*sign,0.07));}
    }
    assert!(!fill_resume::inventory_ready(7.9,8.,0.2));
    assert_eq!(config().fill_tolerance_kg,0.07);assert_eq!(config().fill_overfill_kg,0.05);
}
struct TurboRig { c:Controller, now:u64, mass:f32, pressure:f32, valves:[Option<ValveState>;5], events:Vec<(usize,bool)>, evidence:Option<Evidence> }
impl TurboRig {
 fn new()->Self {
  let (c,first)=Controller::start(&config(),8.,750.,input(10_000,100.),Some(Sample{value:7.9,at_ms:10_000}),ctx()).unwrap();
  let mut r=Self{c,now:10_000,mass:7.9,pressure:100.,valves:input(10_000,100.).valves,events:vec![],evidence:None};
  apply(first,&mut r.valves,r.now,&mut r.events);r
 }
 fn step(&mut self) -> crate::turbocharge::Update {
  self.now+=100;for v in self.valves.iter_mut().flatten(){v.at_ms=self.now;}
  let u=self.c.tick(Inputs{valves:self.valves,..input(self.now,self.pressure)},Some(Sample{value:self.mass,at_ms:self.now}),ctx());
  apply(Effects{valves:u.effects.valves.clone(),notification:None},&mut self.valves,self.now,&mut self.events);
  if let Some(e)=&u.completed {self.evidence=Some(e.clone());} u
 }
 fn raise(&mut self){for _ in 0..10 {self.step();if self.c.phase()==crate::turbocharge::Phase::Raising{return;}}panic!("N2 raising not reached");}
}
fn ctx()->Context{Context{reset_generation:1,qualification_generation:2,configuration:[3;32]}}
#[test]
fn near_full_first_turbo_completes_then_repeat_keeps_first_anchor_and_cumulative_cap() {
 let mut r=TurboRig::new();r.raise();assert_eq!(&r.events[..6],&[(3,false),(4,false),(0,false),(1,false),(2,false),(3,true)]);
 assert!(r.events.iter().all(|&(i,open)|!open||i==3));r.mass=8.1;r.pressure=750.;
 for _ in 0..25 {r.step();if r.evidence.is_some(){break;}}
 let evidence=r.evidence.as_ref().unwrap();assert!(evidence.valid());assert!((evidence.before_mass.value-7.9).abs()<0.0001);assert!((evidence.added_mass_kg-0.2).abs()<0.0001);
 assert!(evidence.accepts_mass(ctx(),Sample{value:r.mass,at_ms:r.now},r.now,0.07));assert!(r.valves.iter().all(|s|s.is_some_and(|v|!v.open)));
 let input=Inputs{valves:r.valves,..input(r.now,700.)};
 let (again,first)=Controller::start_with_inventory(&config(),8.,750.,input,Some(Sample{value:r.mass,at_ms:r.now}),ctx(),Some(evidence)).unwrap();
 assert!((again.inventory_anchor().value-7.9).abs()<0.0001);r.c=again;r.pressure=700.;r.evidence=None;apply(first,&mut r.valves,r.now,&mut r.events);r.raise();
 r.mass=8.21;let bad=r.step();assert!(bad.cancelled.is_some());assert!(bad.completed.is_none());assert!(bad.effects.valves.iter().all(|(_,open)|!*open));
}
