# Optional RRLS push-to-talk

Radio voice is experimental and disabled by default. Set `GS_RRLS_VOICE=1`
on both the primary GroundStation and RRLS backend, retaining their existing
RRLS role and RFD configuration. No change is required to the MCU codec;
encoding and decoding run on the two Linux hosts.

Open Crew Voice, connect normally, then explicitly enable **Relay push-to-talk
over recovery radio**. This control appears only when configured and is usable
only with a fresh peer presence lease. Enabling it selects push-to-talk.
Switching to open microphone or losing the peer lease disables radio sending;
local voice remains available. Re-enable radio explicitly after reconnecting.
Microphone permission and a secure browser origin are still required.

The codec uses mono Opus at 16 kHz, 8 kb/s CBR and 60 ms frames. A frame has
60 encoded bytes, an 11-byte voice header, an 8-byte RRLS envelope and 4 bytes
of UART framing: approximately 11.1 kb/s before modem overhead (13.8 kb/s
on an 8N1 UART). Radio framing, error correction and competing traffic need
additional capacity. Codec bitrate is not a guaranteed air-link budget.

When radio voice is configured, each host defaults to a 25% radio airtime
share; RF retains 40%, leaving 10% unassigned. An 83-byte voice frame plus
the configured 16-byte modem allowance takes about 50 ms of this share,
below its 60 ms generation period. Without radio voice, the existing 40%
host default is retained. `GS_RADIO_AIR_SHARE_PERCENT` overrides the host
share; keep the sum of active transmitters below the link capacity.

Commands, acknowledgements, schema and recovery GPS take precedence. At most
three voice frames wait in the relay queue; frames expire after 180 ms.
Duplicates and older sequence numbers are discarded. Only one local radio
speaker transmits at once. Received radio audio is delivered to local voice
clients and never echoed into the radio. Camera/video streaming has no path
through this voice transport.

The RRLS envelope is not encryption or authentication. Use trusted radio
networks; application login controls local microphone access but does not
authenticate every over-air frame.

Automated qualification covers the actual Opus codec, paired host envelopes,
duplicates, expiry, link loss, command/GPS priority and browser opt-in/PTT
behavior with synthetic audio. Physical RFD airtime, microphone hardware and
intelligibility under real RF loss still require field testing.
