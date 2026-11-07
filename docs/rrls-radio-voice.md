# Optional RRLS push-to-talk

Radio voice is experimental and disabled by default. Set `GS_RRLS_VOICE=1`
on both the primary GroundStation and RRLS backend, keeping their existing
RRLS role and RFD configuration. No change is required to the MCU firmware;
encoding and decoding run on the two Linux hosts.

Open Crew Voice, connect normally, then explicitly enable **Relay push-to-talk
over recovery radio**. This control appears only when configured and is usable
only with a fresh peer presence lease. Enabling it selects push-to-talk.
Switching to open microphone or losing the peer lease disables radio sending;
local voice remains available. Re-enable radio explicitly after reconnecting.
Microphone permission and a secure browser origin are still required.

## Codec and airtime

The codec is Codec2 at 1200 bit/s (8 kHz, 40 ms frames of 6 bytes). Eight
frames go in one radio packet, so a packet carries 320 ms of speech. Browser
audio (16 kHz) is low-pass filtered and decimated before encoding, and
interpolated back after decoding. Codec2 is built for narrow radio links; it is
intelligible but sounds clearly synthetic.

Airtime on the matched profile (32 kbit/s usable = 4000 B/s, 16 B modem
allowance per frame):

| Item | Bytes |
| --- | ---: |
| Speech, 8 × 6 B | 48 |
| Voice header (kind and direction, stream, sequence) | 7 |
| RRLS envelope | 8 |
| UART framing | 4 |
| Modem allowance | 16 |
| **Per 320 ms packet** | **83** |

That is 259 B/s while someone talks:

| Sender and share | Share in B/s | Voice uses |
| --- | ---: | ---: |
| Primary, 25% (direct path) | 1000 | 26% |
| Primary, 15% (RRLS relaying) | 600 | 43%: not sent |
| RRLS, 35% (standby) | 1400 | 18.5% |
| RRLS, 50% (relaying) | 2000 | 13% |

The primary sends voice only while its share is at least 25%. While the RRLS
relays and the primary is held at 15%, the primary still receives RRLS voice
but its own radio push-to-talk is unavailable; local voice keeps working.
For comparison, the earlier Opus 8 kbit/s profile needed 99 B every 60 ms
(1650 B/s) and fit no host share.

## Priority, expiry and limits

Commands, acknowledgements, schema and recovery GPS take precedence: voice is
the lowest priority on the radio queue. At most two voice packets wait; a
packet older than 400 ms is dropped rather than sent late. Duplicates and older
sequence numbers are discarded. Only one local radio speaker transmits at once.
Received radio audio is delivered to local voice clients at real time and never
echoed into the radio. Camera/video streaming has no path through this voice
transport.

Mouth-to-ear delay is at least the 320 ms packet plus air time and queueing,
so expect around half a second. A lost packet drops 320 ms of speech.

The RRLS envelope is not encryption or authentication. Use trusted radio
networks; application login controls local microphone access but does not
authenticate every over-air frame.

Automated tests cover the airtime arithmetic, the Codec2 round trip, packet
size, duplicates, expiry, link loss, the 25% sender rule and command/GPS
priority. Physical RFD airtime, microphone hardware and intelligibility under
real RF loss still need a field test on two stations.
