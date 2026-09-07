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

## Two rule corrections the triage forced, and 132 -> 84

Reading the rows changed the rule twice, both times because it was reporting
one question as many:

* **Only the LAST binding of a name before the loop.** `register_s2_bytebuffer`
  re-`let`s `this` ten times on the way down; exactly one of those is live when
  a loop is entered. Reporting all ten against three loop spans turned one
  question into **30 rows in one file** and buried the rest.
* **The loop HEADER is not the body.** `for x in helper(ctx, this)` evaluates
  `helper` ONCE, before the body — it is not a per-iteration GC point. Counting
  it made every `for .. in helper(ctx, this)` a row.

`native-builtins` 132 -> **67**, `native-collections` 19 -> **16**,
`native-io` **1**, `native-api` 0.

## Triage of the 67 `native-builtins` rows

Classified by the shape of the use, with six read by hand. **This is a
classification, not 67 individual verdicts** — the buckets are named so the
next reader can start from the shape rather than the row.

| bucket | rows | verdict |
|---|---:|---|
| `ctx.invoke*` on the carried reference | 26 | real |
| passed to a local helper that can GC | ~26 | real, same defect one call down |
| `monitor_wait` / `monitor_wait_release` loop | 4 | real |
| iterator loop (`hasNext`/`next` on a carried `it`) | 3 | real |
| rebound by the reporting statement itself | 3 | false positive |
| a pin is handed to the callee alongside the name | 3 | false positive |

The "passed to a local helper" bucket is the classifier's `needs reading` pile:
`sha1prng_next(ctx, this, ..)`, `java_list_get(ctx, list, ..)`,
`native_es_add(ctx, ..)`, `store_property_in_sidetable(ctx, wrapper, ..)`,
`lucene_data_output_write_byte_direct(ctx, this, ..)`,
`IoFutureInner::fire_notifier(ctx, this, ..)`. Same defect, one frame down.

**The rule is a LOWER BOUND, and the triage proved it.** It requires the
GC-capable statement to NAME the binding. In `drain_to_lbq_bounded` the loop is

```rust
let elem = ctx.get_array_element(arr, i);                 // uses `arr`
ctx.invoke_virtual(coll, "add", .., &[elem])?;            // GCs, names `coll`
```

`arr` is just as stale on the next iteration and is **not reported**, because
no GC-capable statement mentions it. Widening to "any GC in the body stales
every reference the body carries in" is correct and would raise the count
substantially; it is not done here because the narrow form is what the
calibration above measures.

## Fixed here: the two `letsgo_compat` drains

`drain_to_lbq_bounded` and `drain_to_abq_bounded` carry `coll`, `arr` AND
`this` across a loop whose body runs `invoke_virtual(coll, "add", ..)`; `this`
is used after the loop as well. All three are now pinned before the loop and
re-read — the re-reads shadow inside the body, so the outer bindings are
untouched. No unpin: both are native entry points and `safe_native_call`
truncates the pin stack to its entry floor on return.

They are `#[cfg(feature = "synthetic-jdk")]`, so `cargo check -p
cratonvm-native-builtins` does not type-check them at all without
`--features synthetic-jdk`. That is the same trap that had dev unbuildable on
Linux the same morning; the check was run with the feature on.

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
