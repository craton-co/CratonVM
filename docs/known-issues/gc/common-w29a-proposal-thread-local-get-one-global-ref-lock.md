# common-w29a proposal: one global-ref lock per `ThreadLocal.get()` hit

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 23
> of 54).** Not built (no `resolve_global_roots2`). **Gate:** a contended
> `ReentrantReadWriteLock` read microbenchmark, interleaved binaries, medians;
> or a profile showing the second acquisition absent, which retires the page.
> **Size:** S.

- **Status:** PROPOSAL (for triage). Filed 2026-09-26, gc-common round
  2026-09-23, wave 29, lane A29.
- **Kind:** performance direction. No defect; nothing is wrong today.

## Where we are

Since gc-common w29-a (`docs/internal/gc-common-round-20260923/common-w28b-remaining-identity-hash-keyed-side-tables-FIXED-20260923.md`
rank 1), a `ThreadLocal` row in `native-builtins/src/phases_early.rs::TL_MAP`
names its ThreadLocal through a JNI WEAK global (`add_weak_global_root`), and
a lookup compares the resolved owner with the receiver. A `get()` hit now
resolves two handles:

1. the row's owner (`tl_row_owner` -> `resolve_global_root`), and
2. the value's strong root (`tl_value_to_java` -> `resolve_global_root`).

Each resolution takes the calling VM's `jni_global_refs` mutex
(`vm/src/vm/vm_exec.rs::resolve_global_root`). Before w29-a a hit took it
once. Every thread of a VM shares that mutex, and `ThreadLocal.get()` is hot:
`ReentrantReadWriteLock` reaches `readHolds.get()` on contended read
acquisitions, `FloatingDecimal` on every double-to-string, many frameworks per
request.

Each row also costs a weak handle (one `Box` plus a slot in the weak set that
every stop-the-world pause sweeps, `jni::sweep_weak_global_refs_with`).

## Proposal

Add one `NativeContext` method that resolves two global handles under ONE
acquisition, for example

```rust
fn resolve_global_roots2(&self, a: usize, b: usize) -> (Option<ObjectRef>, Option<ObjectRef>);
```

with a default of two `resolve_global_root` calls (mocks and other contexts
unchanged), and a VM override that locks `jni_global_refs` once. `native_tl_get`
then resolves the owner and the value together on the (overwhelmingly common)
single-row bucket.

A larger variant: share ONE weak owner handle per ThreadLocal across every
thread's row (minted once, cached in a per-VM table keyed by the
ThreadLocal's weak lock key and looked up only on a row insert), which removes
the per-row weak handle and its per-pause sweep cost. The lookup cost moves to
inserts, which already run a Java upcall on a miss.

## What would retire this page

The `get()` hit path takes the global-ref lock once, measured with a
contended `ReentrantReadWriteLock` read microbenchmark (interleave binaries
and take medians, per the repo's microbenchmark-noise note), or a triage
decision that the second acquisition does not show in a profile.
