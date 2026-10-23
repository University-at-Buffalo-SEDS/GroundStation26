# Active reliability and logging checklist

Earlier completed entries in `antenna-and-comms.json` describe their recorded
baseline. They do not qualify the current source or replace the gates below.
No hardware is available; use the approved Jupiter simulator workspace.

- [ ] **valve-pressure-daq-sd** (in_progress): Record250 valve pressure samples/second on DAQ SD, retaining source uptime/frequency/UTC/sequence, raw ADC and converted pressure; existing50Hz GS reports unchanged. Four samples per dedicated DAQ-only VALVE_PRESSURE_LOG batch; bounded DMA capture and SD queues.
  Evidence / remaining validation: DAQ190 and Valve138 unit tests pass; factory builds pass. Late-IRQ rejection and the actual Renode500Hz TIM3/ADC/DMA half/full/circular contract pass. Updated SD inspector and pressure-rate gates pass the62-test simulator Python suite. C19 amplifier startup passed, but its pressure/P6 short gate failed: zero high-rate capture and startup gateway drops. C20 read-only registers prove a simulator ADC read-set bug (ADSTART set, ADEN incorrectly cleared), with correct TIM3 trigger and no pressure initialization failures. Exact old-model negative control fails; repaired HAL enable/start plus actual Renode500Hz DMA/stop contracts pass. C21 confirmed pressure capture/publication/DAQ acceptance with zero pressure drops but failed amplifier startup with19 auxiliary rejections. C22 moves pressure formatting/SD locks off network RX, copies a bounded batch/session and tracks written/flushed rows.190 DAQ tests, ASan/UBSan handoff/retry and macOS production factory pass;63 simulator Python tests and ADC/UART DMA contracts pass. C22 still failed five CAN ring drops, eight boot auxiliary refusals and four PT publish refusals. C23 unchanged-firmware probes reproduced fragmented final schema assembly headroom refusals; no long run is accepted. A descriptor-allocation optimization is being tested in an isolated SDK prototype, not applied/released.

- [ ] **fc-raw-calibrated-sd** (in_progress): Retain raw sensor codes and corresponding converted values together on FC SD, alongside existing logs, at configured SD cadence with no extra radio traffic.
  Evidence / remaining validation: Eight SD regression tests pass; production factory fits474356/474624 bytes. Full172-test FC suite passes. Shared formatter/append code passes172 FC tests and production factory fits473932/474624 bytes (692 bytes headroom); simulator FC factory now also fits. Recovered-card/throughput validation remains pending; very small flash headroom must remain explicit.

- [ ] **paired-mission-settings** (in_progress): Synchronize mission calibration and settings between explicitly paired GS and RRLS. Warning confirmed on RRLS. Application signing trust independent of HTTPS; per-channel revisions, persistence/reconnect/offline merge; no hardware config, passwords or device identity sync.
  Evidence / remaining validation: Causal revision, atomic persistence, replay, concurrent conflict, stale-resolution and bounded loss/reorder transport tests pass. Application signature tests are added; backend typed adapters, radio budget/integration, conflict UI, fresh-data warning and paired-system qualification remain required. A signed two-station integration regression passes loss/duplication/reordering, independent offline channel edits, restart with retained conflicts and explicit reviewed-head resolution;19 tests and strict Clippy pass. No synchronization is claimed from core tests alone.

- [x] **compact-pt-editor** (complete): Cap PT limit editor width and input width; About secondary/smaller than Save, preserve other settings on save.
  Evidence / remaining validation: UI108 passed/7 existing ignored; Wasm build; isolated real-component browser checks at1440/390 widths and screenshots, help/save flows pass. Pushed to UI dev in1e6fc81, followed by89763ec (Tools-only clear action).

- [x] **tools-clear-graphs** (complete): Add existing Clear all graphs/no-history-reseed action to Tools too.
  Evidence / remaining validation: Same browser run clicks Tools action and verifies persisted history cutoff. Standalone toolbar action removed; Tools is the sole graph-clear action; recording files unaffected. Pushed to UI dev in1e6fc81, followed by89763ec (Tools-only clear action).

- [ ] **final-software-audit** (pending): Reconcile earlier completed plan entries against latest changes, verify remaining actual UI controls, sequence/calibration flows, RRLS identity/config/native handover, Ethernet discovery and safety behavior.
  Evidence / remaining validation: Reuse recorded historical acceptance, rerun tests impacted by new settings/logging/network changes. Keep physical device checks and simulator checks distinct.

- [ ] **current-platform-release** (pending): Build current sources on macOS and RaspberryPi/Linux; strict SDK checks for heap/compact/embedded; all firmware factory builds; final paired GS/RRLS tests and long soak; manual multiarch simulator build.
  Evidence / remaining validation: No relaxed drop/latency/freshness gates. Firmware main and GS version4 selectors retained. Hardware unavailable.

- [ ] **publish-qualified-changes** (pending): Commit/push owned files, update SDK README/changelog/docs, release SDK4.2.5 locally on crates/PyPI and GitHub, manually publish simulator0.4.15 then push dependent board image refs and GS/UI.
  Evidence / remaining validation: No public release before qualification. Exclude credentials/runtime/user spreadsheets. No physical flashing. SDK main pushed before dependent resolution; final registry/package checks required.

- [ ] **Gateway/RF root repair:** C13 P6 short/long passes are retained, C13
  amplifier restart and C14 enlarged-queue failures remain failed evidence.
  C15 original-cap amplifier boot passed but restart failed with four auxiliary
  refusals. C16 fair TX plus maintenance-free DAQ drain passed startup and two
  GS restarts with the original16KiB cap. Current all-seven-board and backend/image builds pass in C18 after preserving
  the C17 simulator-FC flash-overflow failure. Shared FC append/format code
  fixes that overflow. C18 amplifier startup failed two auxiliary refusals. C19 with24KiB passed amplifier startup but failed pressure/P6 short with zero modeled pressure capture and startup gateway drops. C20 proved the ADC enable latch simulator error. Gateway now reclaims completed UART/queued work before new CAN admission under low memory; both gateway checkouts pass142 tests and the production gateway factory fits. C21 amplifier startup failed19 auxiliary refusals. C22 pressure writer is progressing with no SD drops, but startup CAN/source refusals remain. C23 localizes additional memory refusals to final chunked schema assembly; an isolated SDK prototype is under test. No long/current-source pass is claimed.

Completed code is not a field guarantee. Keep source-rate preservation, source
and SD loss counters, valve freshness, RF receipt deadlines, exactly-once FC
execution, memory bounds and reconnect behavior as separate acceptance checks.

Offline edits on either station are required (operator confirmation). Preserve concurrent same-setting versions and require explicit resolution; merge independent channels. C16 startup and two-restart isolation passed; C15 failed with four boot auxiliary refusals. No full current-source release qualification is claimed.

## Latest qualification checkpoint (C25–C27)

- C25 corrected bounded chunk-descriptor/scratch candidate: heap/compact full native
  matrices and strict Clippy pass; all seven simulator firmware builds pass.
  Amplifier startup passes. P6 short fails four initial pressure admission losses
  and additional missing batches, despite 100% observed loadcell publications,
  20 ms maximum LC gaps, zero gateway CAN ring drops and 50–110 ms RF receipts.
  Read-only recovered-card inspection independently rejects a pressure sequence gap.
- C26 switches only the dedicated VB-to-DAQ pressure type to Unordered reliable,
  with bounded 200 ms retry of refused enqueues. Both images build, but amplifier
  startup fails continued auxiliary publication refusals (41 at the final probe).
  No long gate is accepted. Queue capacity, source rates and reserves are unchanged.
- A new 44-byte lost-data/lost-ACK regression passes for heap and compact storage.
  An additional refused-SD-handoff negative control exposes a real SDK defect:
  local handler failures were recorded but final success receipts still emitted.
  Isolated Router Packet/Packed paths now return the error and withhold receipt,
  while attempting forwarding to other destinations. The regression covers both
  ingress representations and both handler representations, without duplicates.
  Full heap/compact native SDK matrices and strict Clippy pass after this fix.
- DAQ acknowledges pressure only after its atomic whole-batch SD queue accepts it.
  Executable receipt/backpressure regression and full 191 DAQ tests pass; Valve
  bounded retry/rollover regression and full 139 tests pass. SD inspection accepts
  bounded out-of-order retries while continuing to reject holes and duplicates.
- C27 is rebuilding all seven images and running the original strict startup,
  P6/pressure short, and conditional 600-second gates. It adds first/latest queue
  refusal snapshots using existing simulator-only storage. Results are pending.
  No public push/release or hardware flashing has occurred in this checkpoint.

## C28 diagnosis and C29 bounded-drain candidate

C27 startup failed35 auxiliary refusals. C28 adds allocation-free diagnostic queue
counts and independent live-entry byte sums to the existing simulator-only buffer.
It proves real retained loadcell traffic (109–113 entries,22345–23165 bytes,
priority16), with no queued schema, ACK or end-to-end pending records at failure.
Accounting agrees with the live sum; this is not evidence of a counter leak.
C28 also retains five gateway startup ring drops; it is failed evidence.

DAQ's previous loop always slept1ms after its1ms cooperative network budget,
capping a busy worker near500 transmit opportunities/s before overhead. The amp
profile produces up to two250Hz streams, in addition to pressure receipts. C29
replaces that unconditional idle wait with at most four immediate work passes,
each relinquishing to equal-priority SD/report peers, followed by a mandatory1ms
sleep. Acquisition retains higher priority; queue limits, source rates, clocks and
all other firmware remain unchanged. The executable wait-policy test covers1kHz
and50kHz scheduler ticks, idle/error/missing-router paths and bounded fairness.
Full192 DAQ tests and strict SDK Clippy pass; C29 simulator results are pending.

The recovered C25 FC SD card validates13-column records and counter reconciliation,
including5840 paired raw/converted vectors:2192 accelerometer,2191 gyroscope and
1457 barometer samples. This advances FC-format validation but does not turn the
failed network cohort into a qualification pass. Current-source long validation
and release remain gated. SDK changes remain isolated in the prototype until
combined firmware validation passes.


## C30–C33 checkpoint

C29 failed startup with three gateway ring drops. C30's lower-allocation SDK
publication path builds all seven factories, but startup fails26 gateway CAN-ring
drops at tick1014 in CAN processing; the drop total remains flat afterward.
DAQ loadcell/50kg publication failures stay zero in the observed samples, with
an earlier auxiliary refusal snapshot retained. No P6/long pass is inferred.
Host five-round profiling of10000 C-API16-byte publications shows median14%
lower CPU time,53% fewer allocations and43% fewer allocated bytes; MCU gates remain.

Exact further tests found clock quota spent during both route planning and
forwarding, and a chunk expiry extension after refused descriptor allocation.
Isolated router/relay fixes commit clock quota after accepted handoff, keep
reachability queries non-mutating, and expire assemblies from accepted progress
only. Busy-side forwarding, duplicate/refused fragment expiry, complete heap and
compact native matrices (511/517 library tests plus integrations/C-system tests)
and strict Clippy pass. C31/C32 FC simulator links fail by124 FLASH bytes; all
failures remain recorded. No SDK source promotion, publication or flashing.

An extra LLVM machine-outliner pass comparison recovers496 native FC FLASH bytes
(474164 to473668/474624) without changing RAM or functionality. CSV field-writer
and forced startup-outline experiments were rejected because they grew flash;
those experimental source rewrites are not applied. FC build flags now request
machine outlining plus two reruns, preserving explicit option overrides. All-seven
simulator rebuild/strict startup and conditional P6/600-second gates remain next.

Still open: pressure250Hz end-to-end/card completeness; gateway startup/steady and
restart loss; current-source RF latency/exactly-once delivery; typed GS/RRLS setting
adapters/runtime/radio/conflict UI; final audit/platform/paired long qualification;
owned commits/pushes and conditional SDK/simulator releases. Existing PT-editor and
Tools clear-graphs UI acceptance remains complete but uncommitted.


## Added scope confirmed2026-10-22

- [x] **gitlab-ci-campaign**: Repair the actual failing SEDSnet GitLab main job,
  without disabling warnings or the release campaign. Pipeline7610/job28764 on
  main13c74ab fails embedded Clippy explicit-auto-deref at config.rs3089. Isolated
  current-main negative control reproduces it; removing only the redundant
  dereference passes the exact strict thumbv7em-none-eabihf embedded/timesync
  command. Separate CI commit3574af3 is pushed to main; GitLab pipeline7616
  passes Rust tests, secret detection and Semgrep. The larger network candidate remains isolated/unreleased.
- [x] **idle-fill-armed-fire**: Both flight-launch and test-fire use the requested
  Idle → Fill → Armed → Fire operational flow. Preserve HITL; preserve existing
  flight-only GPS, FC routes/notification and QD ACK+5s gates. Preserve the rule
  against automatic GroundStation abort after entering launch/countdown/Fire;
  board abort latches and autonomous FC protections remain independent.
- [x] **sequence-checklist-evidence**: Successful sequences automatically complete
  their linked shared checklist items, including valve self-test and the existing
  stepped nitrogen test. The nitrogen result is the requested cold-flow evidence
  (operator explicitly confirmed existing test), persists across service/board
  restarts until manually cleared or the Fire sequence runs, and supports an
  explicit UI override. Distinguish successful test evidence from overrides;
  persist/audit both, sync devices, and reject delayed completion after reset.
  Do not invent a new nitrogen valve pattern or automate commands from a checkbox.
- [x] **saved-data-3d-replay**: Rocket3D is for replay of saved telemetry, not live
  viewing. Add recording selection, timeline/speed/seek and saved-sample binding;
  live dashboards/programs use cameras/2D. Do not silently bind missing saved
  attitude or phases to live telemetry. Test saved-data isolation and time bounds.
- [ ] **frontend-source-ownership**: Move browser UI HTML/JS and rendering assets
  out of backend/src/backend assets into the frontend repository; keep approved
  microservices separate. Move frontend source-folder JS helpers into frontend
  assets. Provide a frontend submodule and reproducible source/asset build path;
  frontend-dev must select matching current dev UI/assets, backend-only and
  direct Cargo/Docker/RRLS/native builds must remain supported. Preserve auth,
  voice/worklets, camera/program behavior and offline assets during relocation.
- [ ] **repository-layout**: Organize GroundStation runtime/API/config, tooling,
  docs and frontend submodule coherently, update build scripts/CI/container copy
  paths and docs together. Preserve user files/runtime state and git history.

C33 all-seven simulator factories now pass with the FC outlining options. Strict
startup/pressure diagnostics are still running; no full network pass is claimed.
An isolated GS runtime staging workspace has three passing typed calibration
adapter tests: channel-scoped merge/reset, atomic physical-input/profile transfer,
and stale/unknown/malformed rejection. A smaller128-byte chunk plus binary signed
settings-envelope experiment passes19 core tests and the paired loss/restart/
conflict integration test; it also rejects duplicate-only expiry refresh. These
staged adapters/envelopes are not yet applied to the runtime or claimed as a fix
for RRLS missing live fill telemetry. Integration, budgets, typed idle application,
conflict UI and paired backend tests remain open.

### Automated checklist completion — 2026-10-22

Implemented administrator-selectable sequence bindings (valve self-test, nitrogen test, fill target). Only a successful engine transition after durable qualification persistence emits a completion; requests, failures and interrupted sequences do not. Completion is stored in the existing transactional checklist store and polled by clients. Manual checks of linked items are recorded as overrides; a subsequent real pass replaces an override with actual test evidence. Changing a binding invalidates completion. Explicit qualification reset clears linked completions while preserving unrelated manual checks. Refresh/restart does not replay old events into manually cleared items. HITL sequence behavior is unchanged.

Validation: full isolated macOS backend suite 369 passed, one existing ignored (local loopback enabled); focused checklist tests 9 passed; GSE routed/procedure tests 18 passed; frontend wasm check passed. Frontend rendered editor verification, cross-station checklist synchronization, nitrogen completion consumption after Fire, full Idle/Fill/Armed/Fire workflow and crash-window retry/outbox remain pending; this does not claim those larger items complete.

CI: minimal embedded Clippy fix pushed to GitHub main as `3574af3`; GitLab mirror still reported the previous commit at the last check. Initial local full campaign: 604/605 native tests passed, with the C harness failing to locate its static library because it ignores an external CARGO_TARGET_DIR. A source-tree target symlink to the same isolated build directory is used for the rerun; no assertion or lint was relaxed.

Simulator C33: all seven firmware factories build, including FC with LLVM machine-outliner reruns. Startup run timed out after 1500 wall-clock seconds while progressing through samples, not an observed firmware halt. Original failure retained; identical startup gate restarted detached with a 6000-second wall-clock allowance. No virtual duration, source rate, freshness or loss criterion changed. No long-soak pass claimed.

Automated checklist handoff now has a bounded durable outbox (maximum three pending events) and transactional receipt IDs (128 retained). Startup replays pending events; an applied-but-unacknowledged event cannot undo a later manual clear. Qualification/abort reset invalidates pending events. Full macOS backend: 372 passed, one existing ignored; includes three staging-only calibration-adapter tests not yet promoted. GSE tests20 passed; checklist tests10 passed; HITL checklist tests10 passed and HITL cargo check has no warnings. UI rendered testing found/fixed an accessible selector label; stale-template rejection remains intact and the browser fixture now waits for its second-client update before editing. Final browser rerun pending. Local SEDSnet full and compact-packet-compression/fair-admission campaigns both passed. GitLab main mirrored3574af3; pipeline7616 running at last check, both security jobs passed.

Final automated-checklist browser acceptance passed against the compiled Wasm bundle: administrator sequence binding, automatic completion observed by two clients, explicit manual-override label, manual clear retained across reload, stale-template protection, and editor/view layouts320/390/844/1440px.390px screenshot visually reviewed. Frontend connection/footer labels use the connected station's GroundStation/RRLS name. Final UI sources and browser regression applied to the frontend repo. Cross-station checklist merging, explicit cold-flow qualification override/Fire consumption, and full new mission-state flow remain open in the larger plan; this feature does not claim those integrations.

Additional lint audit: strict backend all-target Clippy is not a passing gate yet. It first stops at three pre-existing shared GSE style diagnostics; an isolated style-only experiment exposed a larger backend lint backlog (including large Response errors and old nested conditions). That experiment was reverted in staging and no unrelated engine changes were promoted. Regular backend/HITL checks and the tested checklist feature remain passing; the lint backlog stays part of final software audit.

## Persisted operational flow checkpoint — 2026-10-22

Implemented Idle/Fill/Armed/Fire for operational flight-launch and test-fire, separately from unchanged FC wire ascent/recovery values. Version-1 qualification files migrate to version2. Existing stepped nitrogen-test success remains cold-flow evidence; a dedicated authenticated reasoned override is separately audited and cannot claim a passed test. It requires prior valve self-test/baseline, fresh zero-band PT, closed supply confirmations, stable routes and interlock. Fire durably consumes cold-flow evidence before dispatch, clears linked checklist items through the receipt/outbox mechanism, refuses duplicate Fire and cannot replay after restart. Failed Fire persistence leaves armed evidence intact in memory, latches storage failure and refuses dispatch. Paused Fill/Armed retain connectivity protection with FC Idle. The automatic mass cutoff and arming/live launch mass checks now share one threshold, preventing an accepted99.5%-style cutoff from requiring an unreachable exact mass afterward; underfilled plateau stops do not arm. Flight GPS/FC/QD ACK+5s remain; test-fire needs no AV bay; HITL sequence behavior unchanged.

Validation: isolated full macOS backend378 passed/1 ignored(default),379/1(test-fire),364/1(HITL), including three unpromoted typed-calibration staging tests. Core37 tests; final focused GSE25 tests; HITL check warning-free. Actual compiled browser test passes reason dialog, override-vs-pass display, all four stages, reload/Fire lock and320/390/844/1440px layouts with standard44px controls;390px screenshot visually reviewed. No hardware actuation. New flows applied to repos; combined firmware/network/platform qualification remains open.

SEDSnet GitLab pipeline7616 at main3574af3 is green: rust-tests28784, secret_detection28783 and semgrep-sast28782 all succeeded. Both local full and compact/fair-admission campaigns passed. This closes the narrow CI campaign repair, without promoting the larger SDK candidate. Jupiter SSH is reachable again; C33 rerun failed and its result is under investigation. No long-soak or release pass is claimed.


## C34 retained diagnostic capture (2026-10-22)

Jupiter is reachable again. The interrupted C33 startup container exited137
(SIGKILL), after only three checkpoints, without final artifacts. No retained
assertion proves a firmware crash or host OOM; this remains failed/incomplete
evidence, never a soak pass.

Simulator console capture now bounds individual lines and retained head/tail
memory, rotates disk logs, and writes memory probes directly into a persistent
live directory. Read-only R0–R15 and SCB fault registers are emitted at each
checkpoint. Regression tests cover oversized lines, sustained noisy output,
rotation, failure cleanup and reading partial probes before finalization. Full
Rust/native,64 Python tests and strict Clippy pass on macOS. The Jupiter manual
C34 image build also passes its Rust suite. It inherits the exact C22 Renode/GS
runtime and uses unchanged C33 firmware. A fresh original startup gate is being
run with Docker lifecycle events and resource monitoring; packet-loss, virtual
time, source-rate and freshness criteria are unchanged. No qualification or
release is claimed before that run completes.

Current UI is committed locally:1e6fc81 (checklist results/compact tools) and
0534b7f (mission flow/reviewed cold-flow override). It is not yet pushed.


## C34–C37: captured FC code-generation failure and current gateway loss

The earlier496-byte FC outlining experiment is **withdrawn as unsafe**. C34's
retained console exposed FC copying through unmapped SRAM. A read-only hook on
the exact C33 ELF captured memcpy R2=0x20032b60 and LR=0x0806439f. Disassembly
shows OUTLINED_FUNCTION_86 calling a second-pass helper without preserving LR,
then tail-calling memcpy. Its first168-byte copy returns into the same helper
with the advanced source pointer as the next length. This is a real candidate
image defect, not an accepted network soak or evidence of hardware repair.

FC now explicitly disables additional machine-outliner iterations and migrates
existing CMake caches to reruns=0. A real CMake migration regression and all173
FC tests pass. Concise sensor diagnostics preserve every failure condition,
actual/expected chip ID and diagnostic register value while recovering148
FLASH bytes. The macOS factory uses474016/474624 bytes; C36 simulator factory
uses474596/474624. Features and flash partitions are preserved.

C36 completes startup execution in about176 wall seconds but **fails** the
network gates. At sample6, DAQ publication/acquisition failures remain zero;
gateway CAN ring drops=767 (670 bulk, sender6=4, sender7=763), hardware FIFO
overflow=162, ingress queue failures=513 and admission failures=5. Allocation
and mutex failure counters remain zero. The first CAN ring drop at tick1006
occurs during CAN work, with PSP PC=0x080570e8 in varint::read_u64_32_slow.
No pressure/P6 short or long gate is accepted from this failed startup.

A bounded20ms trace of this exact firmware records1803134 guest instructions:
compiler_builtins memcpy9.659%,32-bit slow varint9.474%, ThreadX mutex/hook
functions roughly10%. About5.6% of PCs lack sized symbol attribution; that
analysis count is **not** an unmapped-memory access count. C37 tests cached
priority-band rejection before router/relay control scans decode bulk headers,
and a board-owned gateway recursive-lock fast path. Real ThreadX mutex entry
and priority inheritance remain for competing tasks; ISR/startup and owner
handoff are tested. Gateway143 tests and macOS factory pass. Full SDK heap
and compact/fair-admission matrices plus strict Clippy pass before C37.
C37 manually rebuilds all seven images and runs unchanged startup, short and
conditional long gates. Its startup includes the same bounded CPU profile.

Simulator diagnostic capture additionally starts before UART readiness, retains
launcher stderr, and treats CPU access to unmapped SRAM as a failed run rather
than silently continuing a zero-filled copy. It does not fabricate a physical
HardFault register state. Full native simulator regression and strict Clippy
pass, including partial evidence readable before finalization and a failed
launcher before PTY creation. Current firmware selectors remain main and GS4;
SDK runtime changes remain isolated pending complete qualification.


## C37–C39: bounded boot evidence and current backend build

C37 rebuilt all seven firmware factories. Its startup capture records zero
CAN receive-ring drops, compared with767 in C36, but still records11 network
admission failures and4 ingress queue failures. The unchanged strict startup
and120-second pressure/P6 runs both fail the missing FC boot-marker check;
healthy FC telemetry counters do not waive that check. No long run is accepted.
The20ms instruction profile still attributes9.619% to slow varint decoding and
9.881% to compiler-builtins memcpy; these are instruction shares, not measured
CPU utilization or a physical-radio latency guarantee.

Simulator capture now retains only actually observed boot-marker lines outside
its bounded head/tail console. Each CPU boot-success hook also writes a separate,
closed per-node marker file. Missing, corrupt and oversized records fail;
telemetry readiness never synthesizes boot evidence. Full114 Rust and64 Python
regressions and strict all-target Clippy pass, including missing/corrupt marker
negative controls and Ethernet socket/discovery/backpressure tests.

C38 manually builds the current GroundStation HITL source with the exact frozen
C37 SDK candidate4.2.5 and unchanged seven-board images. Its startup fails before
firmware qualification because the new backend's compiled build-only path is not
writable by the simulator user. C39 corrects the image build path and copied
configuration ownership to match the existing runtime installation. C38 remains
failed evidence; C39 must independently pass startup, pressure/P6, GroundStation
restart, and then the conditional600-second gate. Hardware remains unavailable.


## C39–C42: managed-update transmit starvation reproduction

C39 passes all seven actual CPU boot markers and records zero gateway CAN-ring
loss, but fails the500ms managed-variable deadline. This is failed qualification,
not a long-soak pass. The sequence stops after its first deadline failure;
lower than expected final update counts do not prove that three operator
changes were actually dispatched. Three RF destination ACKs remain pending.

C40 repeats the same image/firmware with existing transport/pending tracing.
A buzzer update queued at virtual2180ms is first handed to transport at2750ms:
570ms of delay already occurs before the radio transport. This identifies a
scheduler investigation independent of antenna/modem tuning. The SDK's shared
fair TX pop chooses only the highest band and, after8 high items, the lowest.
With continuously ready255 receipts and16 telemetry,254 managed values can
starve indefinitely. A native negative control reproduces service[16,16] with
no managed turn in18 pops.

The isolated C42 candidate rotates the bounded lower-priority opportunity
through waiting bands, descending then wrapping. FIFO within a band and retry
eligibility remain intact. Both routers and relays use this shared implementation.
The regression now observes[254,16]; deferred/FIFO and queue-layout checks pass.
Full513 heap and519 compact library tests, all integration/C-system tests and
strict all-target compact/fair-admission Clippy pass. The cursor fits existing
queue padding on the checked native layout; no queue budgets/rates/reserves,
wire formats or latency deadlines are changed. C42 manually rebuilds the current
GS HITL image against the exact frozen candidate, then all seven firmware images
before unchanged startup/pressure/P6/conditional600-second gates. SDK promotion
and release remain gated on this combined validation.

## C43–C45: fair scheduling, independent host transports and bounded analog retry

C42's fair-queue repair exceeded FC flash by52 bytes. The equivalent C43
priority-distance implementation fits the simulator FC at474620/474624 bytes
(4 bytes free); the macOS factory uses474040 bytes (584 bytes free). This very
small flash margin remains a release risk. Full514 heap/520 compact SDK tests,
integration/C-system tests and strict Clippy pass, but C43 startup and its
independent120-second diagnostic fail unchanged network gates. DAQ retains four
auxiliary publication refusals; RF managed updates miss500ms. Actual DAQ queue
snapshots show107 priority16 entries and24368/24576 logical bytes, with zero
allocator/mutex failures. This is real queued traffic, not evidence of a leak.

Recovered C43 DAQ-card inspection independently rejects a pressure sequence/time
gap at record1231. FC-card inspection accepts9662 recorded raw/converted rows;
that individual check does not qualify the failed network campaign.

Native Router and Relay negative controls reproduce a second host bottleneck:
one blocked interface callback causes another independent interface to return
`side tx busy`. C45 uses one persistent callback gate per side on std hosts,
keeping same-side serialization and aggregate callback reentry protection.
Embedded builds retain the original layout/gate. Full516 heap/522 compact SDK
tests, all integration/C-system checks, native and embedded strict Clippy pass.
The frozen SDK SHA256 is
`2d9d0815f2eb490e1963d885b2982e7d738af94bd1f90928ccbba99851fe41b1`.

DAQ C45 also retries transient auxiliary enqueue refusals within one shared100ms
snapshot deadline, yielding between attempts. Persistent/nontransient failures
remain explicit losses; source rates, queue budgets and pressure/SD acceptance
are unchanged. ASan/UBSan tests cover transient recovery, a persistent full
queue, tick wrap and nontransient errors at1kHz/50kHz. Full193 DAQ tests and the
macOS TLSF/compact factory pass (910808/1900032 FLASH bytes). All seven C45
simulator factories and its current-source GS image build. Startup, independent
pressure/P6 short and conditional600-second soak results must pass unchanged
before SDK promotion/release; no current-source qualification is claimed here.

C45 startup confirms zero DAQ analog final failures and zero gateway CAN-ring,
UART-ring, allocation or mutex failures. It still fails seven RF managed-value
assertions at510ms against the unchanged500ms deadline. Only the first operator
round is dispatched before validation stops. Three RF owner receipts remain
pending, and buzzer transmission counts do not advance before the deadline.
Gateway ingress admission still refuses25 CAN and2 UART entries, with one
network RX enqueue refusal. These remain unresolved; no long gate is allowed.
C46 is a bridge-isolated diagnostic using exactly C45's image and hash-verified
firmware, with existing reliable/transport admission tracing enabled. Concurrent
host scheduling means it is diagnostic evidence, never release qualification.

## Browser map image decoding

Frontend dev `3aa6dfd` now routes browser HTTP raster requests through the same
validating protocol as cached/native paths. HTML/SVG fallback responses with
HTTP200 previously bypassed the checks. A real headless MapLibre negative
control records12 image decode failures; the corrected path records zero for
12 requests. Seven targeted Node checks and actual transparent-PNG browser
decoding pass. UI dev commits through this fix are pushed. The initial missing
204-response hypothesis was ruled out by the actual browser test; no backend
204-response change is retained. The running station has not been rebuilt here.

## Current station/UI corrections and remaining C45 failures

The Pi service was on GS dev34a1fe1 while the frontend already included checklist
`complete_on` fields. Matching tested GS91b3c50 is now pushed to dev; rebuilding
and restarting the backend is required to replace that old binary. This mismatch
was introduced by publishing the frontend before its backend, not a reason to
remove the new sequence-checklist contract.

Direct HTTPS/HTTP probes and the service log establish that the station had no
map database in its expected directory. Map files are ignored runtime data;
no deletion is established by that absence. The operator started the downloader;
no second download was started here. New backend tests cover a database installed
after startup and its initially unfinished schema, plus legacy inline bundles.
Browser tests preserve zoom-out during missing tiles and prove visible nontransparent
pixels arrive when tiles become available. The standalone Clear all graphs
button is removed; the same persisted cutoff action remains in Tools. Settings
administration uses the shared theme and44px touch target, verified at1440/390px.

The current station's clock/status endpoint returns T−00:10.00, and its actual
audience page renders the clock after the delayed buffer warms. Program history
was collected only by viewer polls. The new fixed-cadence service sampler retains
at most364 snapshots/90seconds, respects shutdown, skips missed ticks, and clears
future samples after a backward UTC step. Delayed/replay time is preserved;
there is no live-clock fallback. Startup now explicitly says Buffering. Default
backend382 tests pass with one existing ignored; HITL368 pass with one ignored.
These counts include the isolated workspace's additional test-only settings adapter.

C45's sustained120-second diagnostic failed with2953 gateway CAN-ring drops
(2895DAQ,58Valve), beginning at tick34602ms in CAN processing. No allocation,
mutex or UART-ring failures were recorded. Gateway CAN admission refusals279 and
network RX queue failures849 remain unresolved. C46's independent trace also
fails RF's500ms deadline: its first buzzer update is queued at2250ms but not
admitted to transport until5030ms, then selected for physical transmission at
6380ms. This is both SDK-side and transport-side queueing, not proof of an antenna
fault or a successful current-source soak. No long gate or SDK release is allowed.

A separate actual-schema FC regression exposes an obsolete v3 numeric-ID limiter:
current GPS/hash-based sensor IDs can bypass the configured cap, and unrelated
current IDs can collide with old sensor IDs. A fixed13-slot board-owned mapping
uses SEDS_DT constants, preserving the configured cadence and full-rate SD logging.
The old implementation fails the real-ID negative control; ASan/UBSan coverage
checks all13 streams, independent slots, command/message exemptions, wraparound,
sustained rates and1kHz. Full174 FC tests and its macOS factory pass (474092 of
474624 flash bytes); simulator flash headroom and a combined campaign remain
required. This repair is not claimed to explain all RF queueing.

Current UI source is pushed as89763ec;108 Rust tests,8 focused Node checks,
real visible-tile recovery, and responsive settings/Tools browser checks pass.


## Map downloader first-use repair

The reported SQLite14 failure is reproduced by an absent output database:
the downloader created directories but never enabled SQLite file creation.
Both direct-download and legacy-bundling paths now use typed filename options
with create-if-missing and contextual errors. WAL/normal locking allows the
station to read committed tiles without waiting for the entire download.
Three downloader regressions pass: first creation/resume/reset with special
characters in the path, concurrent read visibility during an active writer,
and legacy bundling into an absent output directory. The original first-use
case fails with SQLite14; a URL-special path independently reproduces the old
string-URL parsing failure. No second full tile download was started here.


## Protected radio admission and FC real-schema rate mapping

C49's time-sliced trace confirms repeated radio-side backpressure for discovery
and ReliableAck traffic. A canonical-frame negative control fills the shared
radio control allowance with actual ACKs and refuses a newly requested underglow
value. GroundStation now leaves up to256 bytes inside the unchanged allowance
for new commands and services receipts separately from the application FIFO.
Application bursts remain bounded at256 bytes; total control bursts still yield
to ordered schema/routing metadata. Accepted frames are retained until successful
physical writes. The new regression checks command admission/first dispatch,
all accepted receipts, bounded reverse fairness, metadata order and exact budget
release.17 focused radio tests pass. Default backend383 tests and HITL369 pass,
with one pre-existing ignored in each; these include the isolated workspace's
three test-only calibration-adapter cases. Registry SEDSnet4.2.4 remains the
native-test dependency and the production requirement remains `4`.

C51, using the same C49 SDK diagnostic source and all seven unchanged hash-verified
C45 firmware images, passes the original amplifier/startup gate including500ms
RF managed-value receipt deadlines. C49 failed that gate. This is a startup
repair, not a claim that sustained gateway drops/reconnects are solved. C53 adds
the independently built FC rate-map candidate and runs startup followed by the
unchanged120-second pressure/P6 gate; long qualification is conditional.

FC's obsolete numeric rate map is replaced by13 bounded slots keyed by the
actual SEDS_DT constants. The original implementation fails the actual-ID test;
175 tests now pass, including byte-preserving nonallocating simulator publication
observations. Sharing repeated barometer initialization diagnostic formatting
recovers flash without removing errors, observations, features or partitions.
The macOS production factory uses474012/474624 bytes (612 free); Linux simulator
factory uses474596/474624 (28 free). The unsuccessful shared numeric-publisher
experiment was reverted. Combined current-source network and SD-card checks
are still required before release.

Map fixes are pushed separately:37420cb creates missing databases and permits
concurrent reads;1a314a0 keeps transient transfer retries quiet, emits the final
underlying cause only on exhaustion, and keeps default concurrency1024. Seven
map regressions and strict all-target Clippy pass, including deliberately
truncated response bodies for both storage modes. One reported public tile
was independently returned as a valid11371-byte JPEG; that does not establish
which side caused the operator's intermittent transfers.

## Latest checkpoint: descriptor exhaustion and C53–C56

- Map downloader93c9dd5 is pushed to GS dev. Read-only Pi capture confirms its
  actual1024 soft/1048576 hard file limit and a1024-open-descriptor snapshot;
  another snapshot contains939 network sockets. Independent system lookups
  succeed between failures. The downloader now raises only its own soft limit
  within the existing hard limit (2304 for unchanged concurrency1024), shares
  bounded per-host DNS results/single-flight refresh, and retains quiet retries.
  Thirteen exact-lockfile macOS tests and strict all-target Clippy pass, including
  child-process soft/hard-limit and1024-open-file headroom tests. No interfaces,
  DNS servers, Pi service or running download were modified. Operator must pull
  and restart the resumable download to use it.
- C53 completes120 virtual seconds. The original report selection fails because
  retained live and completed capture directories both match. Explicit read-only
  analysis of the single completed capture passes the original per-source99.5%
  preservation and500ms/3s freshness thresholds. Gateway CAN ring drops remain
  zero in all12 observations. Initial admission6 / network queue12 counters
  remain flat, so this does not establish zero refusals of all packet classes.
- Independent C53 SD inspection rejects pressure record1362: consecutive source
  sequences139/140 have ticks1598/1603, a5ms step instead of4ms. This is proven
  timestamp lattice jitter, not evidence of a lost pressure sample. The Valve
  candidate derives subsequent ticks from source sample progression while
  preserving whole-period sequence gaps and late/overwritten DMA rejection.
  The old code fails the expanded actual-ISR regression; candidate139 tests and
  macOS TLSF/compact factory pass (452696/474624 FLASH). The inspector is unchanged.
- Simulator's completed-capture selector correction passes both retained-live
  and ambiguous/live-only regressions. Full current Python suite69 tests passes.
  C54 remains a diagnostic-only run and retains its original selection failure;
  it is not used to qualify latency or release. Its bounded20ms gateway profile
  at virtual60s attributes1572600 instructions, with96856 unmapped. Varint slow
  decoding10.394% and Rust memcpy10.314% are measured optimization targets, not
  claims of cycle-accurate CPU utilization. Exact ELF/trace hashes are retained
  with the profile; unsized assembly labels are included.
- C55 fails before configuration because its helper assumes Release rather than
  this board's Release_Script directory. Preserve that evidence. Corrected C56
  isolated Linux factory passes; startup then unchanged120-second network/SD
  rerun is in progress using FC52 + Valve56 + five hash-verified C45 boards and
  the untraced C51 engine. Card inspection follows completion. No long or release
  qualification is claimed; remaining paired settings/runtime, replay, audits,
  platforms, source promotion and long-soak checklist items remain open.

## Current checkpoint: C57–C59 and saved recording replay

- C56's rounded individual sample clock can overlap adjacent four-sample DMA
  batches. Valve now advances whole16ms DMA half periods, preserves genuinely
  missing halves, rejects repeated/backward jitter and keeps source sequence/tick
  wrapping. An actual-ISR negative control fails on the old code. All139 Valve
  tests and macOS/Linux factory builds pass (macOS452712/474624 FLASH).
- C57 startup passes, but the unchanged120-second network/SD campaign fails.
  The prior duplicate/overlap is gone; the pressure card now exposes a real
  retired-prefix sequence hole. Valve battery freshness also fails. Neither
  network nor SD is qualified, and no long gate is accepted.
- Gateway profile analyzer f251c10 is pushed separately to simulator main after
  explicit approval. Its three regressions cover unsized assembly labels and
  aliases. The published diagnostic tool is not a firmware/image release.
- A new isolated SDK candidate filters only known, live, nonlocal target sets
  proven to be entirely behind the ingress side, before owning/queuing payloads.
  Dynamic schema, unknown/local/mixed/alternate routes, managed variables,
  explicit policies, encrypted data and sequenced hop receipts retain normal
  processing. Runtime-added schema and actual old-code negative tests pass.
  Final521heap/527compact library tests and strict embedded/all-target Clippy
  pass. The native instrumented10000-frame comparison changes median elapsed
  time16.713ms to3.004ms and70688 counted allocation calls to zero for that
  specific filtered stream; this is not MCU cycle utilization or end-to-end
  qualification.
- C58 fails before firmware build because its archive omits Cargo-declared
  benchmark paths; it also exposes a generic build.py feature forwarding defect.
  Both packaging and forwarding are corrected and executable CLI tests added.
  C59's fresh archive SHA256 is
  1ec645c97b406668fe3849eaca09bc06d4773c46afd806e73014debe84bf8adb.
  Its gateway factory and startup pass. The exact executed cargo feature list
  includes ingress-target-filter. The unchanged 120-second run fails its nominal-rate count check. Separate
  source-preservation analysis passes the original 99.5% threshold and freshness
  bounds, with zero CAN/UART ring drops in all 12 observations. Gateway CAN
  admission refusals still grow from 2 to 64; allocation failures remain zero.
  Pressure capture/publication reaches 29,624 samples, DAQ enqueues/writes 29,556
  and flushes 29,536. Both DAQ inspector modes reject the same pressure sequence
  hole at CSV record 16,659; this does not independently establish raw-ADC loss.
  FC card inspection passes. The original failed run is retained; no long gate
  is accepted. Memory caps/source rates/thresholds are unchanged. No production SDK promotion or public release has occurred.
- Saved3D replay is staged independently: explicit recording selection, frozen
  record-ID snapshot, bounded/paged read-only windows and saved-only state.
  Current live telemetry/calibration/phase cannot become replay data. Application
  station origin, recording ID and snapshot ID are isolated across native GS/RRLS
  changes. Seeking commits after drag rather than fetching every intermediate
  position. Playback supports0.25–4x, pause/rewind and missing-data indication.
  Live audience program uses2D artwork/cameras. Seven replay API tests pass,
  including HTTP authorization and active-file snapshot exclusion. Native UI
  tests, Wasm build and real-component desktop/mobile control tests pass; real-model rendering including mobile pivot/framing and saved-clock
  pause/slow-motion checks passes. Full staged backend suite passes 390 tests
  with one existing ignored; the staged workspace also includes three earlier
  typed-settings adapter tests. Exact repository checks follow promotion.
  A strict frontend Clippy audit identifies existing warnings plus four new
  replay-clock nesting warnings; the new warnings are fixed. A clean full
  frontend warning audit remains required before release.
- Paired settings production runtime/conflict UI, final audits, all-platform
  builds, pressure/network long soak and release remain open. Core anti-entropy
  and adapter tests alone do not establish cross-station settings synchronization.


### C60–C64 continuation checkpoint (2026-10-22)

- C60 fails its original 120-second source-preservation gate: one ten-second
  second-loadcell window preserves 99.3918% (99.5% required). Freshness passes
  and the observed CAN rings remain drop-free. Both DAQ inspector modes reject
  the same pressure-part sequence gap at CSV record 17,173; FC card inspection
  passes. No long gate is accepted.
- Valve62 separates observed HAL capture time from the ADC sample grid, so UTC
  age never subtracts projected future sample time. Its 140 tests, including
  drift/gap/wrap checks and old-code negative reproductions, pass. macOS and
  Linux factory builds pass. Source rates and the 44-byte CAN payload remain
  unchanged; network/SD qualification still requires a fresh cohort.
- SDK61 preserves accepted queued reliable packets under exhausted receipt
  credit and prioritizes/drains queued hop ACKs in Router and Relay. Twelve
  Packet/Packed ordered/unordered regressions exercise actual wire ACKs. Native
  524 heap/530 compact, C-system integration and strict Clippy checks pass. Its
  Linux FC image overflows flash by 348 bytes, blocking the simulator campaign.
- The SDK63 cold error-classifier experiment passes native and strict Clippy
  checks but increases Linux FC overflow to 540 bytes. It is rejected; the failed
  immutable build remains available. C64 compares compiler inlining budgets
  against the original frozen SDK61 without changing source, features, rates,
  memory limits, diagnostics, partitions or acceptance thresholds.
- Saved replay is pushed as backend 5a8335a and frontend 182cdef. Exact checkout
  tests pass 387 backend tests (one existing ignored) and 114 frontend tests
  (seven existing ignored). Frontend dfea867 fixes warning causes without
  suppression; native/Wasm all-target Clippy passes. Backend 4014880 renders
  paused replay only on view/state changes and preserves centered animation
  transforms. Actual desktop/mobile model browser checks pass.
- Still open: pressure/network short reproduction and long soak, production
  paired-settings runtime/conflict UI, browser-source ownership/organization,
  remaining platform/feature audits and qualified releases. Diagnostic reanalysis
  never substitutes for a failed original gate. No public SDK/firmware release
  or complete-plan claim is warranted yet.


### C65–C73 receipt and size checkpoint (2026-10-22)

- The C64/C67 compiler-budget comparisons and C65/C69 helper-outlining trials
  do not improve FC fit; none is promoted. FC CMake deliberately forces extra
  machine-outliner reruns off because of the earlier nested-helper miscompile.
- C68 classifies exhausted receipt credit consistently as transient transport
  pressure. C70 removes an unnecessary item clone from every reliable transmit;
  the frozen contract retains the logical ID, including reordered/duplicate
  endpoints. C71 removes redundant sorting/deduplication of BTreeMap keys.
  All seven factories pass; Linux FC uses474284/474624 bytes (340 free).
- A stronger route-change regression then exposes phantom receipt owners when
  a new endpoint joins while accepted data waits for window credit. A separate
  negative control exposes replacement of an explicitly frozen Packet cohort.
  C72's first fix is rejected by the real routed-radio integration: it must not
  restrict a targeted proxy from resolving its own downstream segment.
- C73 explicitly distinguishes downstream proxy resolution from frozen retry
  planning, filters owners using borrowed hashes without another owner array,
  and preserves both an existing Packet cohort and its explicit payload shape.
  Its 18 focused credit/hop/frozen cases pass; old-code negative controls fail.
  Full native heap/compact suites pass525/531 unit tests plus integrations and
  C-system tests. Required default all-target and embedded Clippy pass.
- Immutable SDK73 SHA256:
  a6b832ca0c8fc40109601b1ca1c8e8a27aec3c774578d6faece99982fa342544.
  All seven Linux factories and the matching host image pass. Linux FC simulator
  uses474532/474624 bytes (92 free). Independent macOS FC factory builds pass:
  production473948 bytes (676 free), simulator474436 bytes (188 free).
  No partition, source rate, memory cap, feature or acceptance threshold changed.
- C73 startup passes, but the original120-second network/pressure/SD gate
  fails. DAQ pressure rows fall below the original per-window threshold; both
  DAQ card inspections reject the pressure sequence gap. Restart and600-second
  gates are blocked. Source-preservation and FC-card checks pass separately;
  they do not replace the failed original gate.
- In a separate unpromoted copy, the40 existing discovery-disabled build warnings
  are fixed at their feature boundaries without suppression. Minimal std,
  std+time-sync, std+crypto/compression, default all-target and embedded strict
  Clippy pass; default heap/compact525/531 unit tests pass. The legacy minimal
  unit-test harness still imports discovery unconditionally and needs its own
  feature guards. This audit copy is not the running C73 source snapshot.
- Remaining work still includes production GS/RRLS setting synchronization and
  conflict UI, frontend source ownership/submodule/repository organization,
  remaining feature/platform audits, successful long network/SD qualification,
  owned source promotion and qualified releases. Saved replay is complete and
  pushed; it no longer remains an open checkbox above.


### C73 loss audit and retry ownership repairs (2026-10-23)

- Recovered DAQ SD contains29636 unique pressure samples, sequence0..29691:
  56 samples (14 complete four-sample batches) are missing. There are no duplicate
  or backwards arrivals and no sample-grid errors. Valve captured/published29712
  samples with zero capture/publication drops; DAQ reports no handoff drops.
  The additional20 accepted/written rows not yet flushed explain the file tail,
  not the internal holes. This is genuine delivery loss, not unordered arrival.
- Negative native controls reproduce stuck retry reservations after admission
  refusal, shared-budget/ring eviction, queue clearing and egress disable.
  Another control exhausts retries by repeatedly evicting a timer marker without
  advancing time. Reservation ownership now follows actual queue entries;
  retained payloads survive and reservation timestamps bound rescheduling.
- Three additional old-code failures prove that accepted original reliable
  packets can be silently evicted before their first dispatch. The candidate
  preserves these originals, reclaims disposable best-effort work first, and
  backpressures admission when no safe victim exists. Existing schema reclamation
  checks continue to pass. No original simulator gate has been relaxed.
- The disposability flag is computed before parking and fits existing metadata
  padding, checked at compile time on native and ARM targets. Borrowed target
  cohorts remove owned arrays from route selection. Prepared insertion removes
  a second admission/parking pass, with an executable single-parking regression.
  Embedded error reporting forwards existing length-delimited text without
  allocating a formatted copy; platform callbacks own line presentation.
- The current isolated candidate passes539 heap and546 compact unit tests and
  strict default/embedded checks. Full integration, factory and simulator gates
  remain required. Minimal-feature library warnings are fixed; the old minimal
  unit harness still needs discovery-specific feature guards.
- C75/C76/C77/C78/C79 fail Linux FC flash fit; C80 exposes an embedded feature
  guard missed by default builds and is rejected. C81 corrects that guard;
  C81/C82 still fail macOS FC fit. C83 reduces that overflow to476 bytes but
  remains rejected. These failures are retained and do not qualify a release.
- C84 shares the typed logging dispatch across direct/queued and implicit/
  explicit timestamp APIs, preserving queued producers' discovery-free path
  and callback deferral. macOS factories and full native integrations are
  running. No physical firmware has been flashed, and no long soak or complete
  release qualification is claimed.
