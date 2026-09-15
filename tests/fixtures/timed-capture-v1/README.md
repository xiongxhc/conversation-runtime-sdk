# Timed capture v1 vectors

Frozen before producer/consumer implementation. `frame.json` is a complete MED2
capture frame plus its independently inspectable88-byte provenance. Two s16 mono
samples at24kHz cover source numerator0..96000 /24000 at48kHz. Nominal host start
1000000000 and end1000083334 preserve the producer's directed rounding (4ns
arithmetic envelope, unknown physical capture error). PCM is0,32767. Source
session41 is NOT bridge identity. Authentication/epoch are required separately.

`metadata.json.authenticationAcceptedHex` is the mandatory server-to-bridge
MED2 kind9 confirmation for source41 and `bridge-independent`. Each new or
reconnected timed socket must consume this exact identity-bound response before
starting capture/renderer/pair sampling or sending epoch/PCM. A local auth write
or playback silence is not confirmation. The response contains no credential;
all pre-existing authentication and other fixture bytes are unchanged.

All integer JSON strings are canonical decimal; `control.json` pins command
field names and signed/unsigned boundaries. See normative docs for exact bounds
and forbidden unknown/trailing fields. This fixture does not transmit the local
64-piece source ledger or filter support; no exact interior reconstruction claim.

`metadata.json` freezes complete authenticated-envelope framing (synthetic token
only), epoch/pair/discontinuity bodies and03 plaintext records, plus u64 maximum
provenance and independent i64 minimum/maximum clock-pair codec vectors. Extreme
pairs are not an admitted wall interval. `output.json` freezes exact v4 typed
start/page/terminal envelopes. Its lease is valid only against the explicit
fixtureClockUnixMs test clock; it is not a live lease. Rust output encoding and
SDK parsing consume this same file. No authorization or PCM appears in pages.
