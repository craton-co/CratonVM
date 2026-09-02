# `VarHandle.compareAndSet` was served inside the funnel and never bound — 160.7 ns to 53.6

## Status
**FIXED, 2026-08-28.** The reference CAS is bound to a thin direct helper at
both compile doors. It lands on the bound-`set` floor, which is what a bound
`VarHandle` operation costs in this VM.

Measured twice: a three-arm run before the branch was brought up to date with
`dev` (which carries the `dev tip` column, and with it the proof that the funnel
refactor moved nothing), and a two-arm ABBA on the merged binary in the quietest
window available. Both agree.

**Three arms, one probe, ABBA x6, before the merge:**

| isolated CAS probe, ns/op | dev tip | bind OFF | bind ON | HotSpot | gain |
|---|---:|---:|---:|---:|---:|
| CAS reference, succeeding | 160.7 | 158.7 | **53.6** | 5.6 | **3.00x** |
| CAS reference, failing | 156.5 | 153.2 | **52.0** | 5.5 | 3.01x |
| CAS `int`, succeeding | 111.4 | 106.4 | **45.9** | 5.8 | 2.43x |
| `set` reference (CONTROL) | 53.9 | 54.0 | 53.9 | 1.6 | **1.00x** |

**The merged binary, ABBA x6, load1 2.6:**

| | bind ON | bind OFF | HotSpot | gain | vs HotSpot |
|---|---:|---:|---:|---:|---:|
| CAS reference, succeeding | **52.1** | 156.8 | 5.3 | **3.01x** | 9.8x |
| CAS reference, failing | 49.6 | 151.6 | 5.2 | 3.06x | 9.5x |
| CAS `int`, succeeding | 43.9 | 100.9 | 5.4 | 2.30x | 8.1x |
| `set` reference (CONTROL) | 51.1 | 51.0 | 1.5 | **1.00x** | 34.1x |

Against HotSpot the reference CAS goes **29.6x -> 9.8x**. The residual is
[`juc-primitives-and-composition-after-the-compile-refusals-CLOSED-20260901.md`](juc-primitives-and-composition-after-the-compile-refusals-CLOSED-20260901.md).

## What the cost actually was

A native profile of the isolated CAS probe below — a Java-frame sampler cannot
answer this, and that mistake has already cost this cluster three days once:

| | share |
|---|---:|
| `try_jit_site_cached_native_dispatch` | 13.2% |
| `forward_jit_reference_args` + `forward_jit_arg_at` | 11.9% |
| `CharSearcher::next_match` | 10.3% |
| `is_object_address` + `ZObjectStarts::contains` | 17.5% |
| `try_varhandle_instance_field_cas` (its own body) | 7.7% |
| `__memcmp_evex_movbe` | 5.7% |
| `compare_and_swap_field_shared` | **3.1%** |
| `hw_atomic_addr` | **1.2%** |

**The hardware atomic and its barriers are ~4% of what a CAS cost.** The CAS was
served from INSIDE the funnel (`try_varhandle_instance_field_cas`) rather than
bound, so it paid the funnel's entry and its reference-argument forwarding on
every call — a quarter of the total between them. And the funnel arm re-derived
the call site's operand kinds by PARSING ITS DESCRIPTOR per call, which is the
10.3% in `CharSearcher::next_match`; a bound slot knows the kind at bind time.

The reference CAS was already reaching the hardware, incidentally, and that was
worth establishing before assuming otherwise: `CRATONVM_DBG=hw-atomic` prints
`first Reference field atomic: class_id=436 index=0 layout=legacy`. Nothing was
falling back to the lock path.

## The objection that had gone stale

`compareAndSet` was deliberately left unbound, and the reason sat above
`VARHANDLE_WRITE_DIRECT_FNS`:

> `compareAndSet` is deliberately NOT bound yet. Its helper would be
> `(vm_ptr, vh, receiver, expected, new)` — five arguments, and Windows'
> ARG_REGS is four (RCX/RDX/R8/R9), so it needs the stack-argument setup this
> bind does not.

That was true when the write bind was written. It had stopped being true before
it was read: the `0xb6 | 0xb7 | 0xb9` arm calls
`emit_stack_arg_setup(&arg_slots, callee_needs_ctx)`, whose own comment says
*"stack-arg setup for invokespecial/virtual direct calls whose receiver+params
exceed ARG_REGS"* (Round-8 wave-3). The fifth argument lands on the stack on
Win64 and in `ARG_REGS[4]` on SysV. **Neither compile door needed a line changed
for it**, and the comment is retracted in place rather than deleted.

This is the recurring shape: a comment saying we lack a capability outlives the
day we build it. The cheap check is one grep for the capability, not a reading
of the comment.

## What is bound, and what deliberately is not

`VARHANDLE_CAS_DIRECT_FNS`, nine slots, one per value kind, registered at BOTH
compile doors through the same matcher.

**Only `compareAndSet`.** `weakCompareAndSet*` and `compareAndExchange` are not
bound with it: the registry maps each to its own callback with its own result
shape (`compareAndExchange` returns the witness value, not a `boolean`), so
binding them together would widen what a bound site may do. That is precisely
the property the write bind's four modes DO have — all four resolve to one
`varhandle_set` — and these do not.

**References are in scope**, on the write bind's argument rather than the read
bind's. The read table excludes `L`/`[` because a reference RETURN must be
published as a handoff root and the direct arm takes no thread borrow to publish
one with. `compareAndSet` returns `Z`; `expected` and `new` travel INWARD, in
registers the compiled caller's own frame already describes.

**The in-funnel arm stays.** It serves the interpreter and every site the JIT
declines, which a compile-time bind cannot reach. Both routes now call one
`varhandle_instance_field_cas_shared`, for the reason the write pair shares one:
the SATB pre-barrier fires on `expected` BEFORE the store and the post
`write_barrier` only on success, and two copies of that would drift.

## Evidence

**The bind engages, and the funnel count falls to zero.** One binary, the kill
switch as the only difference:

| | bind ON | bind OFF |
|---|---:|---:|
| CAS thin direct served / declined | **3 594 000 / 0** | 0 / 0 |
| CAS in-funnel served / declined | **0 / 0** | 3 594 000 / 0 |
| sites bound (single-pass / OSR) | **0 / 6** | 0 / 0 |

`singlepass=0 osr=6` is the load-bearing half of that. The write bind learned it
the expensive way — bound in the single-pass ladder alone its census moved by
zero, because a `main` loop is what OSR compiles — and the same is true here.

**Two controls, and the measurement is only as good as them.**

* `set reference` reads 53.9 / 54.0 / 53.9 across dev tip, bind-off and bind-on.
  This bind cannot touch a bound `set`, and it does not. An earlier run of the
  same A/B at load 7 had that control moving 0.61x, which is how the run was
  known to be junk before any conclusion came out of it.
* `bind OFF` is `dev tip` to within 2% on every row. The funnel arm was
  refactored to call the shared core; that refactor moved the funnel by nothing,
  which matters because the funnel is still what the interpreter uses.

**A third probe agrees, and it was not built to.** `HibfixVarHandleProbe`'s
`VarHandle.CAS reference` row is a composite — a `get` and then the CAS — and in
the same measurement window it reads 128.1 with its `get` row at 76.8. The
difference, 51.3, is the CAS alone, against `VhCasProbe`'s independent 52.1.
Two probes of different shapes agreeing to 1.5% is worth more than either.

**Correctness.** `regression-suite/src/RVarHandleAccess.java` gains a
reference-field CAS — it already pinned the `int` CAS and the three shapes the
fast path must decline (static-field, array-element, byte-array-view handles),
and the reference shape is the one this bind is for and the only one whose
payload path differs. Warmed in a loop so the compiled route is what runs,
alternating so a CAS that stored nothing would fail, and covering `null` on both
sides of the compare. Its output is **byte-identical to HotSpot over 37 `CK`
lines** with the bind on, with it off, and on HotSpot.

Full regression suite `SUITE=all`: 111 passed / 1 failed with the bind ON and
the identical 111/1 with it OFF (`RJdkEnumerations`, red on dev and documented
there). `cratonvm-jit`, `cratonvm-gc` and `cratonvm-types` all `rc=0`, with four
new slot tests that pin what the bind takes and — more usefully — what it
refuses. `cratonvm-vm` 2631 passed / 2 failed, both
`runtime::resolve::guard::*` reporting `find_method_recursive(` appears 4 times
in `vm/src/runtime/interpreter/invoke.rs` against an allowlist of 3 — a file
neither commit on this branch touches.

(The `-j 3` attempt at that gate was SIGKILLed by the OOM killer while linking a
fat-LTO test binary: 31 GB box, 24 GB already resident from other tenants. `-j 1`
links one at a time and completes.)

## Downstream

Composition does **10.47 `VarHandle.compareAndSet` per chain**
(`--dump-native-registry`), so the predicted gain is 10.47 x 107 ns = 1.12 us of
an 11.7 us chain, i.e. **9.6%**. Measured, interleaved, once the box settled:
936 ms -> 847 ms, **1.10x**.

The arithmetic and the measurement agreeing is the useful part. It is a check on
the residual page's cost accounting, not just on this change — and it says the
remaining ~90% of a composition chain is still unattributed.

## Repro

The probe is `VhCasProbe`, which sits with the others under `apps/probes/`. That
tree stopped being tracked in `3b2901531`, so the loop that matters is written
out here rather than left behind a path — and it is short, because the whole
point is that it holds ONE `VarHandle` call:

```java
// `expected` is carried in a Java local, so the timed loop contains no read.
Node cur = a;
for (int i = 0; i < iters; i++) {
    Node nxt = (cur == a) ? b : a;
    if (REF.compareAndSet(p, cur, nxt)) ok++;
    cur = nxt;
}
```

`REF` is a `VarHandle` for a `volatile Node` instance field. The other three arms
are the same shape: a CAS whose `expected` was never stored (the failing path),
an `int` CAS, and `REF.set` as the floor.

```bash
cratonvm --java-home <jdk> -Dprobe.iters=5000000 -cp <out> VhCasProbe
java -Dprobe.iters=5000000 -cp <out> VhCasProbe
```

`CRATONVM_JIT_VARHANDLE_CAS_DIRECT_HELPERS=0` sends every CAS back through the
funnel; `CRATONVM_DBG=intrinsic-stats` prints the served/declined pair and the
per-door site counts. Read them together — a bind that moved nothing looks
exactly like a bind that was never reached unless both are shown.

Note that `HibfixVarHandleProbe`'s CAS rows are COMPOSITES: each iteration does
a `VarHandle.get` and then the CAS, which is why the isolated probe exists.

## Related

- `performance/varhandle-writes-and-cas-have-no-fast-path-FIXED-20260827.md`
  — the `set` bind, the global mutex, the in-funnel CAS this replaces at the
  bound sites, and the stripe pool.
- [`juc-primitives-and-composition-after-the-compile-refusals-CLOSED-20260901.md`](juc-primitives-and-composition-after-the-compile-refusals-CLOSED-20260901.md)
  — the residual, where `VarHandle.get` on a reference field is now the top row.
