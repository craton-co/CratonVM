# `TestSsl.testPost` — the connection dies mid-transfer, 4 runs in 11, and `testPost` is ~50× slower than HotSpot

| | |
|---|---|
| **Status** | OPEN — filed 2026-08-04 |
| **Severity** | medium — intermittent, and it manufactures false regressions (it nearly did; see "Why this is worth a page") |
| **HotSpot** | PASS 3/3. `OK (21 tests)`, whole class in **7.4–8.6 s**, `testPost[JSSE]` itself in **3.8 s** |
| **CratonVM** | `testPost[JSSE]` FAILS **4 of 11** runs. `testPost` alone takes **~190 s** — the whole class 205–320 s |
| **Discovered** | 2026-08-04, while regression-checking the COV-07 athrow fix (`f9e560dc5`) |

## Symptom

`Tests run: 21, Failures: 2` on `org.apache.tomcat.util.net.TestSsl`, the second
failure being `testPost[JSSE]` on top of the known by-design
`testClientInitiatedRenegotiation[JSSE]`:

```
1) testPost[JSSE](org.apache.tomcat.util.net.TestSsl)
java.lang.AssertionError: expected:<0> but was:<1>
	at org.apache.tomcat.util.net.TestSsl.testPost(TestSsl.java:249)
```

`testPost` starts **8 threads**, each opening its own `SSLSocket` and POSTing a
**16 MiB** body in 128 KiB blocks with `sleep(10)` between blocks, then reading
the echoed body back byte by byte. Any thread that errors increments a shared
`errorCount`; the assertion is `errorCount == 0`. So `was:<1>` means exactly one
of the eight connections died.

It dies in one of two ways — the same event seen from two places in the loop:

| runs | manifestation | branch |
|---|---|---|
| 3 | `Byte in position [N] had value [-1] rather than [1]` | the read loop's own EOF branch (`read()` → `-1`) |
| 1 | `java.io.IOException: ... (os error 10053)` | the `catch (Exception)` branch |

`os error 10053` is `WSAECONNABORTED`. The EOF positions vary and are always
partway through the 16 MiB readback — **5 930 928**, **13 090 736**,
**15 728 560** — i.e. nothing special about the offset; the connection simply
stops at whatever byte it had reached.

## The measurement

All 11 runs on the same Windows 11 host, same fixture, same suite environment,
via `apps/tomcat-suite-runner/run-one.ps1 -Class org.apache.tomcat.util.net.TestSsl`:

| arm | `testPost` failures |
|---|---|
| CratonVM `dev` **without** the athrow fix | **2 / 7** |
| CratonVM `dev` **with** the athrow fix (`f9e560dc5`) | **2 / 4** |
| HotSpot 25 | **0 / 3** |

The two CratonVM arms are the same population — the athrow fix is not
implicated, and neither is anything else in that change. It was measured across
two binaries only because the flake first appeared during that work.

## Why this is worth a page rather than a "known flaky" line

**It already manufactured a false regression.** During the athrow work the first
three CratonVM runs were clean, then `testPost` failed on the first run after
the fix landed. On that evidence — 3 clean before, fail after — it reads as a
clear regression from the JIT change, and the change would have been backed out.
Four more runs on the *pre-fix* binary showed 2/7 failures there too, which is
the only reason it was not.

A 4-in-11 failure in a class people run as a TLS regression check means **one
clean `TestSsl` run is not evidence that a change is safe, and one dirty run is
not evidence that it is not.** Anything gating on this class needs repetitions.

## What this is probably not, and what it probably is

**Probably not a TLS correctness defect.** The bytes that do arrive are correct —
the failure is always "the stream ended", never "the wrong byte". A broken
cipher/record path would corrupt content, not truncate cleanly at a random
offset.

**Leading hypothesis: it is the slowness.** `testPost` takes ~190 s on CratonVM
against 3.8 s on HotSpot — **~50×**, far worse than the ~25× the whole class
shows, so this test is disproportionately affected. The test itself sets

```java
// Increase timeout as default (3s) can be too low for some CI systems
Assert.assertTrue(connector.setProperty("connectionTimeout", "20000"));
```

so its author already expected the connector timeout to be the fragile part.
With 8 concurrent TLS connections each pushing 16 MiB, one connection idling
past the 20 s window — a GC pause, a scheduling gap, a stalled TLS write — has
Tomcat close it, and the client then sees exactly this: a clean mid-stream EOF
or an aborted connection, with no client-side exception and correct data up to
that point.

**What is missing to confirm it:** no server-side timeout or close message
appears in the log window for the failing test, so the above is inference from
shape and timing, not an observation. Confirming it is cheap:

* raise `connectionTimeout` well past 20 s in a local copy of the test — if the
  failures stop, the mechanism is settled;
* or log the server side of the closed connection.

If raising the timeout does **not** stop it, the hypothesis is wrong and this
becomes a real connection-handling defect under concurrent bulk TLS, which is a
much more serious finding.

## The throughput number is the real headline

Even setting the intermittent failure aside, `testPost` at ~190 s vs HotSpot's
3.8 s is a 50× gap on concurrent bulk TLS transfer — 8 connections × 16 MiB in
each direction. That is worth its own investigation regardless of whether it is
also the cause of the failures here, and it is the thing to fix: if the transfer
were not 50× slow, the timeout could not be reached in the first place.

## Reproduction

```bash
apps/tomcat-suite-runner/run-one.ps1 -Class org.apache.tomcat.util.net.TestSsl -Exe <cratonvm.exe>
```

~205–320 s per run; expect roughly 1 failure in 3. Compare against
`-Vm hotspot`, which finishes the whole class in about 8 s.

## Not to be confused with

* `testClientInitiatedRenegotiation[JSSE]` — the by-design TLS 1.2
  renegotiation gap, fails every run, see
  `docs/internal/fixed-suite-bugs/tomcat/testssl-client-initiated-renegotiation-FIXED.md`.
* The 90 `gen_heap::get_field: out-of-bounds field read dropped ... index=7
  num_slots=3` warnings for `TesterSupport$ClientSSLSocketFactory` that this
  class emits on **every** run, failing or not. Guarded, count identical across
  builds, unrelated to this — but a real latent defect in the
  `SSLSocketFactory` → `SSLContext` field walk that also wants its own page.
* The four `TestSsl` failures seen on the Azure Linux fixture on the same day —
  a different set of tests and a settled cause, see
  `docs/internal/fixed-suite-bugs/tomcat/testssl-four-new-failures-on-dev-20260804-FIXED.md`.
