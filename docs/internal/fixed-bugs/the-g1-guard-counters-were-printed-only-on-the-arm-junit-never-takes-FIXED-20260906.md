# G1's guard counters were printed only on the shutdown arm that no JUnit workload takes

| | |
|---|---|
| **Status** | **FIXED.** The process-static guard counters moved into `gc_metrics::collector_decision_report`; the collector-state half is now reached from the same both-arms hook through a `Weak<SharedVm>`. A pre-existing DOUBLE print of the decision report on the normal-return arm went with it. |
| **Symptom** | Ten G1 counters whose doc comments say they are printed "unconditionally … before the early return" so that a zero can be cited as evidence had never been printed once on any Tomcat, H2 or Spring Boot run. |
| **Left behind by** | `g1-promotion-tlabs-took-a-fresh-region-per-worker-per-pause-FIXED-20260906`, which found the same defect for one counter (`promo_dest`) and fixed only that one. |

## The shape

`vm-cli` has two shutdown routes and knows it:

* a normal Java-main return, which reaches `maybe_dump_shutdown_reports` from
  the `main-vm` thread after `run()` returns;
* `System.exit`, which **never unwinds Rust frames** and reaches the same
  function from the native pre-exit hook.

`maybe_dump_shutdown_reports` exists because of that split, and its own comment
says so — it was written when `gc_metrics::collector_decision_report` turned out
to be emitted only on the first arm. But the *other* shutdown census,
`VmHeap::print_gc_summary`, was left where it was: called from inside `run()`
at `vm-cli/src/main.rs:6122`, on the normal-return arm alone.

`org.junit.runner.JUnitCore` and `junit.textui.TestRunner` both end in
`System.exit`. Every class in the Tomcat, H2 and Spring Boot suites is run by
one of them. So for the entire suite corpus, none of these had ever appeared:

```
[GC] g1 evac_ref_rejected=… (torn=…) evac_holder_rejected=… evac_holder_clamped=… source_walk_desync=…
[GC] g1 non_object_roots_skipped=…
[GC] g1 young evacuation: parallel=… serial=… workers_last=…
[GC] g1 implausible_legacy_headers=… copy_shape_drift=…
[GC] g1 flat_walk_refused_array=… kept_seed_rejected=…
[GC] g1 heap: reserved=… committed=… backing=…
[GC] g1 ihop: threshold=… ceiling=… mark_ms=… to_space_exhausted=…
[GC] g1 free-scan: single(…) contiguous(…) regions=…
[GC] g1 card-clean: enabled=… cards_cleaned=… skip_rate=…
[GC] g1 tenuring: threshold=… configured=… ages=[…]
[GC-SUMMARY] young count=… p50_us=… p99_us=… max_us=…
```

The first five are the "expected to be ZERO" guards. Their whole purpose is
that a run can be **quoted** as evidence that a guard did not fire — and the
zero was never emitted, so no run ever could be.

## Measured

Two probes, same workload (enough churn to force G1 pauses at `-Xmx128m`), one
returning from `main` and one calling `System.exit(0)`, `--verbose:gc`. `before`
is a dev-based binary from the same day; `after` is this branch. Numbers are
occurrences in stderr:

| line | before, return | before, `System.exit` | after, return | after, `System.exit` |
|---|---:|---:|---:|---:|
| `[GC] decision histogram` | **2** | 1 | **1** | 1 |
| `[GC] g1 evac_ref_rejected=` | 1 | **0** | 1 | **1** |
| `[GC] g1 non_object_roots_skipped=` | 1 | **0** | 1 | **1** |
| `[GC] g1 young evacuation:` | 1 | **0** | 1 | **1** |
| `[GC] g1 flat_walk_refused_array=` | 1 | **0** | 1 | **1** |
| `[GC] g1 heap:` | 1 | **0** | 1 | **1** |
| `[GC] g1 ihop:` | 1 | **0** | 1 | **1** |
| `[GC] g1 free-scan:` | 1 | **0** | 1 | **1** |
| `[GC] g1 card-clean:` | 1 | **0** | 1 | **1** |
| `[GC] g1 tenuring:` | 1 | **0** | 1 | **1** |
| `[GC-SUMMARY]` | 1 | **0** | 1 | **1** |

Exactly once on each arm afterwards, which is the double-print check as well as
the coverage one.

**And the `2` in the first row is a second defect this found.** The decision
report was printed by `maybe_dump_shutdown_reports` *and* by
`VmHeap::print_gc_summary`, so on the normal-return arm the whole report — the
histogram, the decision, the peer ledger, the G1 cycle, the root-coverage and
JIT-publication rates — went to stderr twice. It now has one printer.

## The fix, in two halves

**The process statics moved.** `evac_ref_rejected`, `non_object_roots_skipped`,
the `young evacuation` pair, `implausible_legacy_headers`, `copy_shape_drift`,
`flat_walk_refused_array` and `kept_seed_rejected` are all free functions over
`AtomicUsize` process statics, so they went into
`gc_metrics::collector_decision_report` beside `[GC] g1 promo_dest`, which was
put there on 2026-09-06 for exactly this reason. That report is emitted on both
arms and needs no VM handle at all, so those lines now print even from a
shutdown that cannot reach the collector.

**The collector state could not move**, because it is `&self`:
`reserved_bytes`/`committed_bytes`, the IHOP model, the free-region scan
denominator, the card-clean counters, the tenuring histogram and the
`[GC-SUMMARY]` percentiles all belong to one `G1Collector`. Rather than
duplicate them into a snapshot published on the pause path — a per-pause cost
for a shutdown report, and a second source of truth for numbers that already
have one — `vm-cli` stashes a `Weak<SharedVm>` as soon as `Vm::new` returns and
`maybe_dump_shutdown_reports` upgrades it.

`Weak`, not `Arc`, and that is the load-bearing choice: a strong reference in a
process-lifetime static would keep the VM alive past the end of `run()` and
change what its `Drop` does and when, which is a real behaviour change for a
diagnostic. On the `System.exit` arm the VM is unambiguously alive — the hook
runs before the process goes away — and that is the arm this exists for.

**Both arms reach the same call now**, so the print is claimed:
`GC_SUMMARY_PRINTED` is a one-shot latch and `claim_gc_summary_print()` is
compare-exchanged at both sites. The normal-return arm still prints from inside
`run()`, where the VM is owned rather than upgraded from a `Weak`; the shutdown
hook prints only if nobody has. The gate itself moved into
`gc_stats_requested()` so the two sites cannot drift — the hook used to ask a
two-term question (`--verbose:gc` or `CRATONVM_GC_STATS`) where `run()` asked a
three-term one that also admits `CRATONVM_DBG_G1ACCESSOR`.

## Tests

`the_decision_report_carries_the_g1_guard_counters_with_no_collection`
(`gc_metrics`) asserts the report carries every one of the moved line prefixes
**on a thread where nothing has collected** — the "unconditional" property the
old comments claimed and the old location could not deliver. It asserts
prefixes, not values: the counters behind them are process statics shared with
every other test in the binary, and pinning their values would be asserting
test ordering rather than wiring.

`the_report_carries_the_evacuator_census_the_collector_recorded` (`g1`) closes
that gap from the other side: it runs a real collection and parses
`parallel=`/`serial=` back out of the report, asserting their sum is non-zero.
Those two statics only ever go up and a pause bumps exactly one of them, so it
holds however the rest of the binary is scheduled.

## What is still on the normal-return arm only

`VmHeap::print_gc_summary` now reaches both arms, so everything inside it does
— including the card census, the parallel-evac census, the moving-young
fallback rows and the ZGC block, all of which had the same defect. What remains
one-armed is the rest of the `if gc_stats_requested` block in `run()`: the
cross-thread peer-scan coverage counters (`XT_*`). They are process statics and
could follow the same route; they were left alone here because this change is
already two mechanisms wide.
