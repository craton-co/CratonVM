# `TestLargeUpload` — HTTP/2 large POST body truncated (65535 expected, 13107 received)

**Status:** OPEN. Confirmed CratonVM-only regression — passes on HotSpot in
the same fixture. Fast/deterministic (~14-17s across two runs).

## Symptom

```
1) testLargePostRequest[0: true JSSE]](org.apache.coyote.http2.TestLargeUpload)
java.lang.AssertionError: expected:<65535> but was:<13107>
```

A large HTTP/2 POST body (expected to fully arrive as 65535 bytes) is only
partially received server-side — 13107 bytes, almost exactly 1/5 of the
expected size (13107 × 5 ≈ 65535).

## Analysis

The near-exact 1/5 ratio is a strong clue: 65535 = `2^16 - 1`, the default
HTTP/2 flow-control window size. 13107 ≈ 65535/5 — could indicate the
server is only processing one out of every N `DATA` frames, or a
flow-control `WINDOW_UPDATE` isn't being sent/honored correctly so the
client stalls after its initial window is exhausted and only a fraction of
frames get through before the test's read completes/times its assertion
window. Not root-caused to the exact frame-handling code path in this
session — worth checking `Http2UpgradeHandler`/`Stream`'s `DATA` frame
processing and `WINDOW_UPDATE` frame emission on the CratonVM side.

## Reproduction

```powershell
.\apps\tomcat-suite-runner\run-tomcat-suite.ps1 -Category failed -RefCsv <ref> -TimeoutSec 60 -Parallel 1 -RunName largeupload-repro -Exe <cratonvm.exe>
```
Consider adding HTTP/2 frame-level debug logging (`-Xlog` equivalent or
existing Coyote HTTP/2 trace flags, if any) to see the actual `DATA`/
`WINDOW_UPDATE` frame sequence exchanged.
