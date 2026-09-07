# The audit's straight-line rule cannot see a loop, and that is where the one proven defect family lives

| | |
|---|---|
| **Status** | OPEN — rule added and calibrated, **123 rows unswept**. |
| **Scope** | `native-io` 1, `native-builtins` 103, `native-collections` 19, `native-api` 0. |
| **Tool** | `scripts/unpinned-native-local-audit.py --loops` |
| **Calibration** | reports all four of the 2026-09-06 `properties_sidetable.rs` defects before their fix, and none of them after |

## The hole

`scan()` asks "is there a GC-capable statement BETWEEN the binding and the
use", in statement order. Inside a loop body that order is a lie: the last
statement precedes the first on the next iteration, so a GC anywhere in the
body stales every reference the body carries in from outside — including one
used EARLIER in the text, and including one used in the very statement that
allocates.

That last case is the one that matters, because the straight-line rule
excludes it deliberately and correctly: a use inside the same statement as the
call is an ARGUMENT, evaluated before the call runs. True for one iteration.
On the next, that argument is a pre-GC address.

```rust
for (k, v) in &snapshot {
    put_kv_units(ctx, this, k, v);   // `put_kv_units` calls ctx.force_gc()
}                                    // `this` is stale from iteration 2 on
```

## Why this matters more than the row count suggests

**The only instances of this family anyone has observed FAILING are loop
instances.** The four `properties_sidetable.rs` defects found by hand on
2026-09-06 (`internal/fixed-bugs/properties-sidetable-four-stale-receivers-FIXED-20260906.md`)
are all this shape, and the crate they live in was swept by the
2026-08-25 pass. Both existing rules — default and `--any-binding` — report 18
rows on that file before the fix and **not one of them is one of the four.**

The file's own in-source guard test has the same blind spot, for the same
reason: it walks statements and has no notion of a loop.

## Calibration

`native-builtins/src/properties_sidetable.rs` at `80db5d314` and its parent:

| | rows | the four hand-found defects |
|---|---:|---|
| before the fix | 13 | **all four reported** |
| after the fix | 9 | **none reported** |

The four that drop out are exactly the `native_properties_put_all` rows. The 9
that remain are triage, not noise — see below.

Rule 1's own calibration (`native_fcimpl_open`, 7/7 before `3950eed48` and 0
after) is unchanged by this addition: `--loops` is a separate mode, not a
widening of the default.

## The 123 rows, and what the first triage says

Two rows read by hand, one of each kind:

* **False positive, a family the `native-io` page already names.**
  `native_process_wait_for_timeout` refreshes through
  `end_blocking_region_refs(&mut held)` and then `this = updated;` — the
  refresh goes through an ARRAY, not through the name, so no name-based scan
  can follow it. That page lists 7 rows of exactly this kind. It is also why
  `native-io` scores 1: the crate has just been swept, and its residue is
  documented.
* **Real candidate.** `native_collections::native_lbq_put_blocking` binds
  `this` from `args`, calls `ctx.monitor_enter(this)`, then loops on
  `ctx.get_field(this, LBQ_FIELD_SIZE)` around a `monitor_wait` — which is
  GC-capable — with no refresh anywhere in the body.

103 of the 123 are in `native-builtins`, the crate the 2026-08-25 pass covered
with the weak rule. That is the tranche to read.

## What this does NOT establish

The same caveat the two predecessor pages carry, and it is load-bearing here:
**no dynamic proof for any of the 123.** The difference from those pages is
that this rule's shape has a proven instance — the 2026-09-06 defects were
found by reading a crash backtrace, not by a rule — so "structurally identical
to something that failed" is a statement about a real failure rather than an
analogy.

Note also that those four fixes' own A/B was FLAT on the workload that found
them (7 vs 6 SIGSEGVs in 10). Fixing a loop-carried stale receiver is right
whether or not a workload currently reaches it; it is not a claim that the 123
are live bugs.

## Next

* Read the 103 `native-builtins` rows. The mechanical
  pin-after-binding/re-read-before-use insertion the `native-io` pass used does
  not apply unchanged — a loop needs the re-read INSIDE the body, which is a
  different edit.
* The inter-procedural half is still unseen: `collect_own_property_names` took
  its receiver BY VALUE and satisfied `ordered_snapshot_kv`'s `&mut` refresh
  contract with a copy. No line-based rule scoped to one function can catch
  that; it wants a check that a helper taking `ObjectRef` by value never passes
  it as the `&mut` argument of a refreshing API.
