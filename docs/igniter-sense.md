# Igniter sense telemetry

The actuator samples `Igniter_ADC` (PA2 / ADC1_IN3, regular scan rank 3) with the existing normal DMA scan. Acquisition continues in all states; safety evaluation and its existing Abort eligibility retain their original state gate. This feature neither changes outputs nor confirms firing or continuity.

The fitted schematic profile is ACS71240LLCBTR-045B5 powered at 3.3 V, with a unity TLV8811 buffer. Allegro specifies 4.5–5.5 V operation for this sensor variant. Calibrated amperes are therefore unavailable (`model_supply_mismatch`). ADC counts and signal volts remain useful electrical observations. Do not apply the nominal 5 V sensitivity, infer zero current, or infer current from an output command. Correcting the hardware requires checking the entire sensor/buffer/ADC range, rather than simply raising the sensor supply.

Signal voltage uses the existing VREFINT measurement and the actual factory 3.0 V calibration reference. Invalid calibration, reference ages over 6 seconds, or computed VDDA outside the G491 ADC operating range of 1.62–3.6 V leave voltage unavailable. ADC rail counts are explicitly marked. Invalid data never becomes a zero-ampere measurement.

## Application wire format

`AB_IGNITER_SENSE` is an application-hashed `UInt32[12]` type (106291377), reliable Off, priority 0, sent at most 5 Hz. No SDK reserved IDs or extra reliability protocol are introduced. Words are version, flags, boot low/high, accepted scan sequence, original sample uptime low/high, raw count, VDDA millivolts, sample age milliseconds, reference age milliseconds, hardware profile. Version 1 fixes the ADC full scale at 4095. Flags are sample valid=1, reference valid=2, saturated=4, calibrated current unsupported=8, ADC fault=16. Profile 1 identifies the schematic mismatch above. Publication retries do not advance the sample sequence or timestamp.

The ground station retains the exact raw packet and records derived `IGNITER_SENSE_SIGNAL` values (raw counts, signal volts, unavailable amperes, exact owner metadata) and `IGNITER_SENSE_STATUS` (freshness, ages, capability and owner/observation metadata). Original sample UTC is retained when known. Boot/sequence/uptime ownership, original monotonic age, received freshness and reset floors reject stale restamping; unknown clocks remain explicit. Old firmware without the type remains supported and shows no reported capability.

The Data tab provides a read-only Igniter sense panel plus separate ADC/voltage graphs, including a fallback tab for existing saved layouts. History stays labelled recorded; it never becomes a live electrical confirmation. Capability mismatch is persistent configuration information rather than a repeated global error. No special startup flags are required.

## ADC recovery

Timeout/error invalidates ownership before disabling only the regular ADC1 DMA channel. Reuse requires a finite HAL stop, actual ADC/DMA quiescence, channel/pending-IRQ clearing and bounded stale-semaphore drainage. Failure stays unavailable and does not rearm. Shared ADC2 is untouched. HAL's conversion stop and ADC disable have separate 5 ms and 2 ms timeout constants, with tick rounding; the entire stop is not described as a single 5 ms bound.

References: [Allegro ACS71240](https://www.allegromicro.com/-/media/files/datasheets/acs71240-datasheet.pdf), [ST STM32G491 datasheet](https://www.st.com/resource/en/datasheet/stm32g491me.pdf).
