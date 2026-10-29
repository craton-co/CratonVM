# A two-receiver `WeakReference.get()` site whose receivers pick the same `Reference.get` stays a call (45 ns per get, HotSpot under 1 ns)

Status: OPEN (found 2026-10-28 by j27 builder lane TRIC while triaging `CRATONVM_JIT_IR_REFERENT_SPLICE_ZGC_ROW`)
Area: JIT optimizing tier, splice admission (`ir-splice-target-not-provably-monomorphic`)

## Evidence

Windows, tree of 2026-10-28 ~19:42, release `CARGO_PROFILE_RELEASE_LTO=off`, `--jdk-only`, pinned to P-cores, ns per 16 gets, median of 5:

| row (`J27WeakRefSpliceRow -Dj27.time=true`) | ZGC default | ZGC `REFERENT_SPLICE_ZGC_ROW=1` | G1 | HotSpot 25 ZGC |
|---|---|---|---|---|
| weakref-get | 714 | 684 | 835 | 10 |
| subref-get | 753 | 736 | 808 | 19 |
| weakref-refersto | 80 | 64 | 60 | 19 |

The facts match HotSpot in every arm. The probe's `getKernel(WeakReference r)` is called with `WEAK` (exact `WeakReference`) and
`SUB` (`final class Sub extends WeakReference`, no `get` override), so the `r.get()` site profiles two types at 50 % each. Both receivers
select the same method, `Reference.get()`. `CRATONVM_DBG_JITC=1` shows the splice refused on the optimizing tier and on the single-pass tier:

```
inline-resolve REFUSED java/lang/ref/WeakReference.get()Ljava/lang/Object; depth=1: ir-splice-target-not-provably-monomorphic [tier=ir route=cp]   (x20)
inline-resolve REFUSED java/lang/ref/WeakReference.get()Ljava/lang/Object; depth=0: splice-reference-referent-load [tier=sp route=receiver=595]
ir-bimorphic-census java/lang/ref/WeakReference.get()... at pc=11 in J27WeakRefSpliceRow.getKernel(...): obs=35264 types=2 top=595:50% second=...
```

HotSpot inlines `Reference.get` here: its class-hierarchy check on the static type `WeakReference` finds no override below it, so the
receiver type does not matter. The ZGC row switch therefore has nothing to engage on in this probe (subref-get is flat with it on),
and the row measures this refusal instead. `Reference.get` and `refersTo` sit at shared kernels like this one in `WeakHashMap`,
`ThreadLocalMap` and caches, all of which see more than one `Reference` subclass.

## What would fix it

When every profiled receiver, or a CHA answer for the static type, resolves to ONE target method, admit the splice behind a
type-set guard (or the CHA dependency) and do not treat the site as polymorphic. HotSpot's rule is a CHA unique-concrete-method
check on the declared type (`WeakReference` has no `get` override in its subtree once `PhantomReference` is excluded). A probe
row with separate kernels per receiver class would then show the ZGC row switch's own effect. Re-run the
`CRATONVM_JIT_IR_REFERENT_SPLICE_ZGC_ROW` A/B on that row before flipping the switch.
