# Hibernate `DelayedCdiSupportTest` — REFUTED (was: hangs during/after Weld CDI bootstrap)

| | |
|---|---|
| **Status** | ⚪ REFUTED (2026-07-06) — not a CratonVM bug. Original "hang" was a shared-host-contention false positive. |
| **Area** | CDI/Weld bootstrap interaction with Hibernate's `delayed` `BeanContainer` access type |
| **Original symptom** | No exception, no crash — the VM process appeared to stop making progress after Weld SE container init and never reached `@@RESULT` within a 200s wrapper timeout. |
| **Discovered** | 2026-07-06, while re-running the Hibernate `others.txt` non-passed list (50 classes, 4 shards, 1200s per-class timeout) against a binary carrying the OSR allocation-region gate fix. |
| **Refuted** | 2026-07-06, same day, on the Azure Linux host (worktree `wt-hib-delayedcdi-hang-20260706`, branch `investigate/hib-delayed-cdi-hang-20260706`, unmodified `dev` @ `9f1db39d`). |

## Original claim

The original doc asserted the hang was "confirmed in isolation, not shared-host
contention" because a sibling class in the same sweep (`SortNaturalTest`) that
also showed `HANG` in the parallel batch turned out to pass cleanly when
re-run alone, while `DelayedCdiSupportTest` allegedly kept hanging even when
re-run alone with `-Dcraton.batch=1`.

## What this investigation found

Re-running the **exact repro command** from the original doc (same
`-Dcraton.batch=1 -Dcraton.trace=1`, same single-class listfile, same
`--Xmx 1500m`, unmodified `dev` @ `9f1db39d`) on the Azure host:

```
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout 200 ./cratonvm-delayedcdi-hang \
  --java-home /home/victor/jdk25 --Xmx 1500m @common.args \
  -Dcraton.batch=1 -Dcraton.trace=1 CratonRunner delayed-only.txt 0
```

**`DelayedCdiSupportTest` passed cleanly 4/4 times**, taking 2.6s–7.8s each —
nowhere near the 200s timeout:

- Solo run 1: `ok=1 failed=0 ms=5937`
- Solo run 2: `ok=1 failed=0 ms=6611`
- Solo run 3: `ok=1 failed=0 ms=7835`
- As part of a 3-class `standard`/`extended`/`delayed` comparison batch: `ok=1 failed=0 ms=2605`

All CDI-strategy siblings (`StandardCdiSupportTest`, `ValidExtendedCdiSupportTest`,
`CdiHostedConverterTest`, `DelayedCdiHostedConverterTest`,
`ImmediateMixedAccessTests`, `ExtendedMixedAccessTest`, `DelayedMixedAccessTest`)
also passed cleanly — no `delayed`-strategy-specific defect exists.

**Direct evidence the original "isolation" wasn't actually contention-free**:
while investigating, a *separate* class in the same comparison batch
(`ExtendedMixedAccessTest`) was itself killed by a 250s `timeout` wrapper with
no `@@RESULT` printed — reproducing the *identical* symptom pattern
(`@@BEGIN` printed, then silence) described in the original doc. `uptime` at
that moment showed **load average 24.01 on a 16-core host** (a concurrent
Gradle daemon alone was consuming 222% CPU). Re-running `ExtendedMixedAccessTest`
alone immediately after (nothing else queued) completed in 5.5s, `ok=1`. This
is the same false-hang mechanism already correctly identified and ruled out
for `SortNaturalTest` in the original sweep — it just also (incorrectly)
escaped that classification for `DelayedCdiSupportTest`, most likely because
the original "isolated" rerun still coincided with heavy concurrent load from
other sessions sharing the same box (this Azure host routinely runs a dozen+
concurrent worktree builds/tests — see `reference_azure_build_host` memory).

The `gen_heap::get_field: out-of-bounds field read dropped` warnings for
`ImmutableList$ImmutableListCollector`/`ImmutableSet$ImmutableSetCollector`
noted in the original doc are confirmed harmless (as the original doc already
suspected) — they still print identically on every passing run and are not
correlated with any actual failure.

## Lesson

Before deep-diving into VM-side root-causing of a "hangs even in isolation"
claim on this shared Azure host, check `uptime`/`ps` load **at the moment of
the hang**, not just "no other shard of my own harness was running." A
16-core box at load average 24+ (other sessions' concurrent builds/tests)
can starve a single-threaded interpreter/JIT process badly enough to blow
past a 200s wrapper timeout on a workload that completes in under 8s when
the host is quiet — indistinguishable from a genuine hang without checking
load. Same pattern as [[reference_hib_functiontests_translation_cluster_refuted]]
and [[reference_elasticsearch_loggerfactory_provider_null_not_a_bug]]: verify
the failure reproduces cleanly before root-causing it as a VM defect.
