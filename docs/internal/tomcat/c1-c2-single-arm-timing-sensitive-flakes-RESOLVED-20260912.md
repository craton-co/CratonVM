# c1/c2 tier run, 2026-09-12: the single-arm FAILs filed as load/timing noise — RESOLVED 2026-09-12

## Status
**RESOLVED; moved from `docs/known-issues/tomcat/`.** The page filed five
classes as timing-sensitive noise and asked for one thing: re-run each
standalone, several times, on a quiet host. That was done in both JIT arms on
the same binary, and "noise" turned out to be the right call for only two of
the five.

| class | verdict | where it went |
|---|---|---|
| `TestAccessLogValve` | **real CratonVM defect**, deterministic, both arms | fixed (`05e4db41c`) |
| `TestRateLimitFilter` | **real CratonVM defect**, a data race | fixed (`1ab9bfaac`) |
| `TestFormAuthenticatorB` | not reproducible standalone | no action |
| `TestFormAuthenticatorC` | not reproducible standalone | no action |
| `TestMapperPerformance` | **real throughput gap**, not noise | split out: `docs/known-issues/tomcat/mapper-performance-native-field-access-gap-20260912.md` |

Reruns: CratonVM release build, local Windows box, real JDK 25, one class per
process with the suite's JVM arguments, 5 repetitions in each arm
(`CRATONVM_C2_SUPERSEDE=0` and `CRATONVM_JIT_FORCE_C2=1`), nothing else running.

## `TestAccessLogValve` — never a flake

The original report already contained the evidence: the page said "c1 only",
but the c2 arm of the same run failed the same case, `test[74:
pct-t-begin:umlaut_time_S, text]`, with the same message (`Access log line
empty after 1001 milliseconds`). Standalone it failed every time, and whichever
of the two `umlaut_time_S` cases ran first failed while the second passed;
HotSpot passed both.

The valve formats `%{begin:...SSS}t` through a fresh `SimpleDateFormat`, and
the FIRST one per locale took **1561 ms** against HotSpot's 23 ms. The test
waits 1000 ms for the log line. The time was in `Calendar.getInstance`, and in
two CratonVM natives on its path, both all-pairs `equals` scans:

* `Stream.distinct()` — `LocaleServiceProvider.isSupportedLocale` dedups the
  1152 CLDR language tags through `toLocaleArray`. 1600 strings: 904 ms, now 2.
* the `Set.of` / `Map.ofEntries` duplicate check —
  `CLDRLocaleProviderAdapter.createLanguageTagSet` builds `Set.of(<1152 tags>)`.
  1101 strings: 217 ms, now 2.

Both are hash-bucketed now, and `vm/tests/collection_dedup_is_hash_bucketed.rs`
counts `equals` calls rather than timing anything (1 999 000 for 2000 keys
before, 0 after). After: first `SimpleDateFormat(ru_RU)` 549 ms, the class
94/94, and 10/10 in the rerun.

## `TestRateLimitFilter` — a race that loses a `ConcurrentHashMap` entry

Standalone it still failed 1 run in 10, on an idle host, so the rerun did not
clear it. `Thread.sleep` accuracy was checked first and matches HotSpot exactly
(50 × `sleep(100)` = 5019-5026 ms on both). A client-timestamping copy of the
test (`RateLimitStallProbe`) then showed the real shape: in each late run ONE
client made zero requests. Its thread had died on its first request:

```
java.lang.NullPointerException: Cannot invoke "java.util.concurrent.atomic.AtomicInteger.incrementAndGet()"
	at org.apache.catalina.util.TimeBucketCounterBase.increment(TimeBucketCounterBase.java:105)
```

`increment` is `map.computeIfAbsent(key, v -> new AtomicInteger()).incrementAndGet()`,
four client threads on a fresh `ConcurrentHashMap`, and `computeIfAbsent`
returned null. CratonVM's native map installs its segments lazily on the first
insert; two concurrent first inserts could each install one (losing the other's
reservation), and a thread whose two reads straddled the install answered "no
segment". Measured with `ChmFirstInsertRace` (4 threads, fresh map per round):

| | computeIfAbsent bad rounds | put bad rounds |
|---|---:|---:|
| before | 1070 / 2000 | 808 / 2000 |
| after | 0 / 5000 | 0 / 5000 |
| HotSpot | 0 / 2000 | 0 / 2000 |

After: 10/10 (5 per arm), no NPE. `vm/tests/chm_first_insert_race.rs` guards it
(runs under `--features synthetic-jdk`).

## `TestFormAuthenticatorB` / `TestFormAuthenticatorC` — not reproducible

10/10 PASS each (5 per arm, 75-120 s per run). The original failures were
`SocketTimeoutException: Read timed out` in opposite arms of a 651-class
`-Parallel 2` sweep, which is what the page said they looked like; nothing in
the standalone reruns contradicts that, and nothing was changed.

## `TestMapperPerformance` — reclassified, and split out

Not noise: 5 of 10 standalone reruns failed on an idle host, in both arms, always
on `iowejoiejfoiew` (5.1-6.9 s against the 5 s budget), and CratonVM is 10-20×
HotSpot on every host. The measurements, the profile, and the two optimisation
attempts that were measured and reverted are in the split-out page.
