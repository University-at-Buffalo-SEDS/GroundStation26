# Fill and Turbocharge

After a qualified fill, less than 0.2 kg of liquid mass loss is considered full. Fill stays paused in that band; at a deficit of 0.2 kg or more, an existing paused fill may resume with fresh pressure below its configured limit. A new fill still requires its original zero-pressure checks. The upper overfill tolerance and automatic fill cutoff remain unchanged.

Turbocharge pressure deadband defaults to 12 psi and can be configured from 8 to 16 psi in GroundStation settings. Existing saved configurations load the default without changing their mass or pressure targets. The pressure target must exceed the deadband and remain below the hardware ceiling.

If the tank is already within the band below its target, Turbocharge keeps the valves closed and checks settling. When nitrogen is needed, it closes at the exact pressure target. Pressure noise within the band does not reopen nitrogen. Falling below the band pauses the operation closed; another top-up requires an explicit operator request. Completed and cancelled controllers never restart automatically.

Launch with Turbocharge evidence requires fresh valve-board pressure within the band and below its configured limit. A later pressure drop does not erase the original nitrogen mass accounting: an explicit top-up retains the first inventory anchor and cumulative nitrogen allowance. This pressure requirement applies to Turbocharge-qualified inventory and does not add a lower-pressure gate to an ordinary liquid-only fill.
