# Four stale receivers in the Properties side table — and a guard that cannot see them

| | |
|---|---|
| **Status** | FIXED 2026-09-06. **No measured effect on the workload that found them** — stated up front because that is the honest result, not a caveat. |
| **Scope** | `native-builtins/src/properties_sidetable.rs`. Worst under `--XX:UseGc Generational`, where a freed object is zeroed in place. |
| **Lever** | `CRATONVM_PROPS_UNROOTED_RECEIVERS=1` restores all four |

## The shape

The side table is keyed off the receiver's IDENTITY (`key_for` →
`identity_hash_code`), so every store must hold a live receiver, and
`identity_hash_code` inflates a monitor — it allocates. The file knows this: it
pins receivers and re-reads them through `read_native_pin` after each call, and
`ordered_snapshot_kv` takes its receiver by `&mut` with a doc comment stating
why — *"it forces every caller's own receiver to be refreshed across the walk
instead of silently carrying a pre-GC address into the pins and virtual
dispatches that follow."*

Four places did not follow it:

1. **`native_properties_property_names`** walks the `defaults` chain on a bare
   `ObjectRef` across two calls that re-enter Java. It looked handled, because
   `collect_own_property_names` calls `ordered_snapshot_kv(ctx, &mut this)` —
   but that helper takes its receiver **BY VALUE** and shadows it
   (`let mut this = this;`), so it satisfies the `&mut` with a COPY and throws
   the refreshed address away at the return. Now walked under a
   `NativeHandleScope`, re-read at every use; the helper takes `&mut ObjectRef`.
2. **`native_properties_put_all`**, snapshot loop — refreshes `this` after every
   other call and not inside `for (k, v) in &snapshot { put_kv_units(..) }`.
3. **`native_properties_put_all`**, map branch — same omission in the
   `str_collected` loop, and the `mirror_` call after it.
4. **`native_properties_put`** — no pin at all, while `put_kv_units` staled the
   receiver that `mirror_loaded_entries_to_properties_backend` and
   `is_system_props` then read.

`store_parsed_entries` (same file) already had the correct form — refresh
INSIDE the loop — so these are omissions against a discipline the file
already carries, not an unknown rule.

## Why the file's own guard missed it

There is a test in this file that scans for exactly this hazard. It is
**intra-procedural**: it walks each function that contains a literal
`ordered_snapshot_kv(` call and flags `args`-derived locals used after it. Two
holes, and case 1 falls in both:

* the refresh contract is expressed as a PARAMETER MODE, and a wrapper that
  takes the same receiver by value launders it — the guard never looks at the
  wrapper, because the wrapper is where the call is, not where the hazard is;
* the guard has no notion of a LOOP. A call inside `for` is a GC point on every
  iteration, and a refresh placed before or after the loop reads as compliant.

## Measurement, and it is null

`KafkaAutoConfigurationIntegrationTests#testEndToEndWithRetryTopics`,
`--XX:UseGc Generational --nojit`, one binary, interleaved:

| | fixed | `CRATONVM_PROPS_UNROOTED_RECEIVERS=1` |
|---|---:|---:|
| SIGSEGV, x10 (all four sites) | 7 | 6 |
| SIGSEGV, x10 (earlier, sites 1-2) | 6 | 5 |

Flat. A `gdb` census of five crashes on that workload says why: four are in
`native_object_hash_code` and one in `execute_invoke_kind` — none in this file.
The open defect that workload has is a receiver already dead on ENTRY to a
native, tracked in
`generational-young-sweep-frees-an-interpreter-held-object-FIXED-20260908.md`.

These four are landed because they are provable by inspection against a
contract the file states, not because a measurement moved. The lever exists so
the next person can re-test them against a workload that does exercise
`Properties.putAll`/`put` under collection pressure — this one evidently does
not reach them often enough to matter.

## Gates

`cargo test -p cratonvm-native-builtins` 4211 passed / 0 failed,
`cargo test -p cratonvm-types` green, `regression-suite/run.sh` 92 of 92,
`tools/flag-census/check-surface.sh` clean.
