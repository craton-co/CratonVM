# H2 — `TestNetUtils` aborts: missing `PBEWithHmacSHA256AndAES_256` AlgorithmParameters

## Status
**OPEN** — missing JCA algorithm.

## Severity
**MEDIUM** — fatal `runtime error` aborts the test (and any TLS/keystore path
that negotiates this PBE).

## Affected test class
`org.h2.test.unit.TestNetUtils` (CRASH in both sweeps — pre-existing).

## Symptom
```
Error in thread "main" runtime error: not implemented:
no AlgorithmParameters PBEWithHmacSHA256AndAES_256 implementation in any provider
```
The VM aborts (no `RUNONE_RESULT`), classified as a crash. `TestNetUtils`
sets up TLS sockets; the JSSE/keystore path requests
`AlgorithmParameters.getInstance("PBEWithHmacSHA256AndAES_256")` (PKCS#12 /
PBES2), which no registered provider implements.

## HotSpot behavior
PASS — SunJCE provides `PBEWithHmacSHA256AndAES_256`.

## Root cause
CratonVM's JCA provider set lacks the `PBEWithHmacSHA256AndAES_256`
`AlgorithmParameters` SPI (the PBES2 scheme used by modern PKCS#12 keystores).

## Fix options
- Register the `PBEWithHmacSHA256AndAES_256` (and sibling
  `PBEWithHmacSHA*AndAES_128/256`) `AlgorithmParameters`/`SecretKeyFactory`/
  `Cipher` SPIs, or route them to the real SunJCE bytecode as done for other
  PBE/cipher paths (cf. the PEMFile PBE work).
- Until then, surface a checked `NoSuchAlgorithmException` rather than aborting
  the VM with a `runtime error`.

## Repro
`AlgorithmParameters.getInstance("PBEWithHmacSHA256AndAES_256")`, or
`org.h2.test.RunOne org.h2.test.unit.TestNetUtils mem`.
