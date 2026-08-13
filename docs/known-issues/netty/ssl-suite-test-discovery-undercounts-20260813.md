# Several `handler.ssl` classes discover far fewer test instances than HotSpot on the same classpath

**Status:** OPEN, not root-caused (2026-08-13). Found on Windows, commit
`ae2e1d9c8`, isolated (`--shards 1`, `--timeout 180`), same classpath both
VMs.

## Symptom

Five classes report a drastically smaller `found` count on CratonVM than
HotSpot reports for the identical class on the identical classpath — this
is a **JUnit-discovery-time** gap (fewer test instances enumerated), not a
runtime pass/fail difference on the tests each VM does run:

| class | CratonVM found/ok | HotSpot found/ok | CratonVM sees |
|---|---|---|---|
| `io.netty.handler.ssl.ParameterizedSslHandlerTest` | 7 / 7 | 63 / 63 | **11%** of HotSpot's instances |
| `io.netty.handler.ssl.SniClientTest` | 3 / 3 | 27 / 4 | **11%** |
| `io.netty.handler.ssl.SniHandlerTest` | 12 / 12 | 32 / 32 | **38%** |
| `io.netty.handler.ssl.SslErrorTest` | 0 / 0 (NOTESTS) | 72 / 72 | **0%** |
| `io.netty.handler.ssl.OpenSslPrivateKeyMethodTest` | 0 / 0 (NOTESTS) | 24 / 2 | **0%** |

Everything CratonVM *does* discover and run in `ParameterizedSslHandlerTest`,
`SniClientTest`, and `SniHandlerTest` passes cleanly — the failures visible
in HotSpot's `SniClientTest` run (23/27 fail there) are not reproduced
because CratonVM never generates those extra 24 test instances in the first
place. `SslErrorTest` and `OpenSslPrivateKeyMethodTest` are total discovery
misses: CratonVM's JUnit Platform Launcher finds **zero** tests in classes
where HotSpot finds dozens.

## Why this matters more than the raw pass-rate suggests

A class showing `PASS, found=7` reads as healthy in a results.tsv scan, but
here it means **56 test executions that never happened**, not 56 that
passed. `NOTESTS` similarly reads as "nothing to test," not "72 tests this
VM can't see." Any dashboard or investigate-batch page built purely from
`results.tsv` status will systematically under-report defects in these
classes' untested 89-100%.

## Hypothesis, not confirmed

These are all `handler.ssl` classes and several use JUnit5
`@ParameterizedTest`/`@MethodSource`/dynamic test generation
(`ParameterizedSslHandlerTest` and `SniClientTest` by name are clearly
parameterized suites; `SslErrorTest`/`OpenSslPrivateKeyMethodTest` finding
literally zero suggests a class-level discovery failure — possibly a
static-init exception being swallowed, or a JUnit5 extension/condition that
errors out during discovery on CratonVM rather than at runtime). Not yet
distinguished between "parameterization source evaluation fails silently on
CratonVM" and "these classes' static setup throws before JUnit can even
enumerate them" — the two would need different fixes and this doc doesn't
have a raw discovery-phase log captured to tell them apart.

## Repro

```bash
cd apps/netty-suite-runner
printf '%s\n' io.netty.handler.ssl.ParameterizedSslHandlerTest io.netty.handler.ssl.SniClientTest io.netty.handler.ssl.SniHandlerTest io.netty.handler.ssl.SslErrorTest io.netty.handler.ssl.OpenSslPrivateKeyMethodTest > /tmp/discovery.txt
CV_BIN=bin/cratonvm-netty-zgc.exe bash run-netty-suite.sh --list /tmp/discovery.txt --gc zgc --shards 1 --timeout 180 --out /tmp/repro
bash run-netty-suite.sh --list /tmp/discovery.txt --hotspot --shards 1 --timeout 180 --out /tmp/repro-hs
```
`found` column in each run's `results.tsv` is the number to compare.

## Related

- `docs/known-issues/netty/ssl-cert-validation-residuals-20260813.md` —
  other `handler.ssl` classes from the same triage pass with genuine
  runtime failures rather than discovery gaps.
