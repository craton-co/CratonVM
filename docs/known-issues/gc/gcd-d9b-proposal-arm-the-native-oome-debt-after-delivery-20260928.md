# Proposal: arm the native funnel's OOME debt when the error is delivered, not when it is decided

*Filed 2026-09-28 by gcd d9/b (lane ladder9), from the d7 run `d4jt_overhead`
(`CRATONVM_DBG_GC_OVERHEAD=1 GenR4W5ThreadsOomProbe -Xmx128m`) and reading.
Not built.*

- **Kind:** proposal (waste on the OOME path; no wrong result).
- **Owners:** lane d (`vm/src/runtime/exceptions.rs`,
  `vm/src/runtime/interpreter/exception_dispatch.rs`) for the delivery hook,
  lane b (`vm/src/runtime/interpreter/gc_and_alloc.rs`) for the debt.

## What happens today

`maybe_dump_heap_on_oom_for` arms the native funnel's debt
(`note_heap_oome_raised`: the raising thread, two payments) at the moment the
ladder DECIDES the error. The first native the raising thread then calls is
inside the error's own construction (`Throwable.<init>` ->
`fillInStackTrace`), before any handler has run, so the first payment runs
`majors_to_decide_oome` on the heap the verdict was just taken on. In
`d4jt_overhead` every error shows the same pair:

```
[GC_OVERHEAD] oome-majors: site=overhead-limit-major thread=4 before=100544496 after=100544496 ... second_major=true
[GC_OVERHEAD] native-oome-debt: thread=4 recovered=false pay=true left=1
[GC_OVERHEAD] oome-majors: site=native-after-oome-major thread=4 before=100544608 after=100544608 ... second_major=true
[GC_OVERHEAD] native-oome-debt: thread=4 recovered=false pay=true left=0
[GC_OVERHEAD] oome-majors: site=native-after-oome-major thread=4 before=100663280 after=6678976 ... second_major=false
```

The middle payment is two futile majors (`before == after`, the second one
run because the first freed nothing); the last one, after the handler's
`shared = null`, is the productive collection the debt exists for (gcd
d3/o's shapes A and B). At `-Xmx128m` a futile major costs 250-450 ms here,
so about 0.6-0.9 s per error, 5-7 s over the probe's eight errors. gcd d5/q
kept the second payment precisely so that the construction-time payment
could not starve the post-drop one; it did not remove the construction-time
cost.

## Proposal

Arm the debt when the error is DELIVERED to a Java handler (or leaves the
thread uncaught), not when the ladder decides it:

1. `note_heap_oome_raised` records a PENDING debt on the raising thread
   (a `JvmThread` field, per thread, no global).
2. The exception dispatcher, on the frame that catches a
   `java.lang.OutOfMemoryError` (or on the thread's uncaught path), moves the
   pending debt into `alloc_ladder.oome_native_debt` with ONE payment.
3. `native_call_owes_oome_major` is unchanged otherwise (only the raising
   thread pays; a recovered old generation clears it).

A native call during construction then finds nothing owed; the first native
after the handler (the drop is Java code in the handler) pays once.

Alternative with no dispatcher hook (lane b alone): a per-thread
"constructing a heap OOME" depth, raised by `exceptions.rs` around the
construction of the throwable it was asked to build for a heap
`OutOfMemoryError` and read by `native_call_owes_oome_major` (a payment is
skipped, not consumed, while it is non-zero). Smaller, but it still needs
the one hook in lane d's file.

## Risks

- A program whose handler drops its data through a NATIVE call before any
  Java code runs (none known) would meet the pre-drop heap in that native;
  today the construction-time payment would not have helped it either (the
  drop had not happened yet).
- Uncaught errors: the thread's `dispatchUncaughtException` runs Java; the
  debt must be armed on that path too or the shutdown-hook shape of reading
  (1) on `../../internal/gc/gcd-d4j-threads-oom-probe-crawls-on-a-full-old-gen-FIXED-20260928.md`
  could return.

## How to verify

```
P="--java-home $JDK -XX:+UseGenerationalGC -Xmx128m -cp tools/bench"
CRATONVM_DBG_GC_OVERHEAD=1 timeout 300 cratonvm $P --verbose:gc GenR4W5ThreadsOomProbe 2>d.err
grep -c 'native-oome-debt: .* pay=true' d.err
grep '\[GC\] oome_ladder:' d.err
```

Expected: one `pay=true` line per error (the productive one), none with
`before == after` on `site=native-after-oome-major`; `ladder_debt_payments`
equal to `ladder_oomes` or lower; HotSpot's seven lines. Gates that must not
move: `GenR4W4NativeStringOomProbe` (d3/o shape A), `GenR4W4HeapFullThrashProbe`,
`GenR4W6JitOomRootProbe -Xmx64m`, `Gcd1PinnedCalleeOomeProbe`, each at its
d7 pass rate, default and `CRATONVM_GC_OVERHEAD_PROGRESS=0`.
