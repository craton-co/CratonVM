# GC: moving collector missed a lost-tag interpreter local root

**Status:** **CLOSED**. Fixed 2026-07-01 by adding a moving-GC-safe lost-tag fallback for interpreter
locals in `Frame::scan_local_objects` and `Frame::update_local_refs`.

## Symptom

Under `-Xmx1g` GC pressure in `--nojit`, the ES `RestClientSingleHostIntegTests` teardown could log
zero-header stale-object warnings such as:

```text
WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped ... index=0
     num_slots=0 class_id=ClassId(0) class_name=java/lang/Object
WARN cratonvm_vm::runtime::interpreter: Stale pointer detected in invokevirtual receiver
     (all-zero header)
```

The suite usually stayed green because guard paths dropped the bad access, but the moving Cheney young
collector had left a live reference pointing into reset from-space.

## Root Cause

The deterministic stale-reference verifier localized the missed root to:

```text
POST-GC ZERO-HEADER PARKED tid=2 frame[13]
    com/carrotsearch/randomizedtesting/RandomizedRunner.invoke local[3] pc=72
```

`local[3]` contained a live object-shaped value at GC time, but its compact tag was transiently not
`Object`, so `scan_local_objects` skipped it. The object was not copied, from-space was reset, and the
still-live local later read an all-zero header.

## Fix

The fix is scoped to interpreter locals, matching the localized failure site. `LKIND_LONG` and
`LKIND_DOUBLE` locals are still excluded before fallback probing, which preserves the existing guard
against primitive collision values. For `LKIND_OTHER` locals whose compact tag is not `Object`, the moving
collector now probes pointer-shaped encodings with `is_object_address` and only roots or remaps an address
that is a real heap object.

## Verification

Passed:

- `cargo test -p cratonvm-vm lost_tag_other --lib -- --nocapture`
- `cargo test -p cratonvm-vm scan_local_objects --lib -- --nocapture`
- `cargo test -p cratonvm-vm update_local_refs --lib -- --nocapture`

`cargo test -p cratonvm-vm --lib` is still not green because the unrelated baseline test
`jit::conservative_roots::tests::cross_thread_jit_gap_detector_trips_on_peer_in_jit` fails the same way on
`dev` before this fix (`detector must record the cross-thread JIT gap`).

## Related

- The reactor-thread leak it co-occurred with remains separate:
  [gc-rscache-reactor-shutdown-timing-race.md](gc-rscache-reactor-shutdown-timing-race.md).
