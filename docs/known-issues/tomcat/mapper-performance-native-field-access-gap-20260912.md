# `TestMapperPerformance`: the `Mapper.map` shadow is 10-20× HotSpot, and the heaviest host sits at 57-80 % of the 5 s budget — OPEN

## Status
**OPEN — real throughput gap, not host noise.** Split out of
`docs/internal/tomcat/c1-c2-single-arm-timing-sensitive-flakes-RESOLVED-20260912.md`,
which had filed it as a timing flake.

The test makes 10^6 `Mapper.map()` calls per host against an absolute 5 000 ms
budget, with one rerun. On an idle host it passes; it fails when an outlier
batch lands on the heaviest host twice in a row. The fix is a lower typical
cost, not a retry.

## Measured (2026-09-12, local Windows box, real JDK 25, release build)

Per-host time for 10^6 calls, whole class, one process each:

| | `xxxxxxxxxxx` | `iowejoiejfoiew` | `foo.net` | other 6 hosts |
|---|---:|---:|---:|---:|
| HotSpot | 88-90 | 363-377 | 154-159 | 75-150 |
| CratonVM, `Mapper` natives (default) | 1 732-2 284 | **3 171-4 207** | 2 864-4 050 | 1 643-2 577 |
| CratonVM, Tomcat bytecode (`CRATONVM_TOMCAT_MAPPER_NATIVES=0`) | 2 950-4 613 | **9 793-11 135** | not reached | — |

* Standalone reruns (5 per JIT arm): **5 of 10 FAIL**, all on `iowejoiejfoiew`
  at 5 164-6 863 ms after the rerun.
* Loop probe (`MapperLoopProbe`: the test's loop on `iowejoiejfoiew`, 45 s, no
  debugger): 2 887-3 468 ms per 10^6, with one batch at **7 898 ms**. That
  outlier shape is what fails the test.
* The shadow's memo is not the problem: `CRATONVM_DBG_MAPPER=1` reports
  `hit_rate=1.0000` on every host (one miss each), so the `internalMapWrapper`
  callback into Java is off the hot path.

## Where the time goes

cdb samples of the loop, 25 stacks inside `native_mapper_map`: **23** in
`get_field_by_name` / `set_field_by_name`. A memo hit costs about 28 by-name
reads, 23 by-name writes, 14 `identity_hash_code` calls and 4 global-mutex
operations — ~3 µs. The leaf cost of a by-name access is split between the
class-manager read lock plus hierarchy walk, the heap-membership validation in
`load_and_forward_checked`, and slot bounds checks / write auditing.

## Tried and reverted

| attempt | `iowejoiejfoiew` loop | verdict |
|---|---:|---|
| baseline | ~3.2 s / 10^6 | — |
| memoised slot index, then `get_field` / `set_field` by index | 3.4-4.0 s | no gain: dropped the lock + walk but added a forwarded class-id lookup per access |
| per-class `(slot, descriptor)` memo, one `get_fields_typed` batch per receiver | 2.82-2.97 s | ~10 %; also broke four source-text witness tests in `native-builtins/src/lib.rs` and needed a GC re-read after `MessageBytes.toString` |

Neither reached the ≤ 1.6 s that would leave room for a 2.5× outlier batch, so
neither was kept.

## What would close it

A memo hit does per-field VM round trips that HotSpot's compiled bytecode does
as plain loads and stores. Candidates, not attempted:

* cut the writes a hit performs: `mapper_apply_fast_entry` rewrites the
  `CharChunk` views of `requestPath` / `wrapperPath` field by field (5 + 3
  stores each) after `MappingData.recycle()` cleared them;
* drop the per-call validation reads of the memo (`versions`, `contextList`,
  `contexts`, `paused`, and seven identity hashes) in favour of an epoch the
  `Mapper` mutators bump;
* a native API that validates a receiver once and exposes both typed reads and
  writes on it for the rest of the call.

## Repro

```powershell
cd C:\craton\CratonVM\apps\tomcat-suite-runner
powershell -ExecutionPolicy Bypass -File .\run-one.ps1 -Exe <cratonvm.exe> -Class org.apache.catalina.mapper.TestMapperPerformance
# per-host Time [..]ms lines are in the output; CRATONVM_DBG_MAPPER=1 adds the memo hit rate
```
