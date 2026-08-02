# RBC.6 still refuses javac `synchronized` blocks: a protected `getfield`/`putfield` publishes no precise exceptional frame

**Status:** 🔴 **OPEN**, split out 2026-08-02 from
[dateformatsymbols-getproviderinstance-compile-bail](../../internal/dateformatsymbols-getproviderinstance-compile-bail-FIXED-20260802.md)
when fixing `ldc <Class>` made this the only remaining reason the
date-formatting path leaves hot JDK methods interpreted.

This is not a new defect — it is a deliberately deferred one, previously
recorded only in a source comment (`precise_frame_publishing_opcode`,
`jit/src/lib.rs`). It gets a doc because
`31-synchronized-code-never-jit-compiled-FIXED.md` claims the opposite and
would otherwise be the only thing a reader finds.

## Symptom

Three hot JDK methods on `probes/DateFormatPatternProbe`, all with one
cause:

```
[cratonvm] JIT method stats: 3 hot method(s) whose COMPILE FAILED …
    299512 tier_fail_count=3  sun/util/locale/provider/JRELocaleProviderAdapter.getDateFormatSymbolsProvider()…  reason=rbc6-handler-reads-unsafe-local
    119493 tier_fail_count=3  java/text/DecimalFormat.format(JLjava/text/Format$StringBuf;…)                     reason=rbc6-handler-reads-unsafe-local
      5497 tier_fail_count=3  sun/util/locale/provider/JRELocaleProviderAdapter.getNumberFormatProvider()…        reason=rbc6-handler-reads-unsafe-local
```

`reason=` is new (same change that fixed `ldc <Class>`); before it these
were three anonymous `tier_fail_count=3` lines indistinguishable from a
codegen hole.

## Why they are refused

`getDateFormatSymbolsProvider` is javac's double-checked
`synchronized (this)` idiom:

```
25: aload_0
26: dup
27: astore_2          <-- monitor object into a NON-parameter local
28: monitorenter
29: aload_0
30: getfield  dateFormatSymbolsProvider     <-- inside the protected range
33: ifnonnull 41
36: aload_0
37: aload_1
38: putfield  dateFormatSymbolsProvider     <-- inside the protected range
41: aload_2
42: monitorexit
…
46: astore_3          <-- the synthetic cleanup handler
47: aload_2           <-- reads local 2, assigned BEFORE the try
48: monitorexit
49: aload_3
50: athrow
   Exception table: 29-43 -> 46 (any), 46-49 -> 46 (any)
```

The handler reads local 2, which is not a parameter, so
`local_handler_reads_unsafe_local` fires: `route_jit_exception_through_
method` reconstructs the handler frame from `this` plus the declared
parameters only, and local 2 would come back null — the synthetic
`monitorexit` would then run on a null monitor.

The escape hatch is the precise (reason-9) exceptional frame, which
publishes the real locals at the throwing site. It requires every
potentially-throwing opcode in the protected range to be one whose lowering
publishes such a frame, and `getfield`/`putfield` are not. They were
admitted in `5bf306bb0` (2026-07-28) and taken out again after
`probes/Rbc6FieldProbe.java` measured what the top-level field arms actually
emit: an inline fast path that neither null-checks nor publishes a frame.
A protected `getfield` NPE let the handler read a non-parameter local as
`0` instead of `38`; a `putfield` on a null receiver did not throw at all.
Both are silent wrong answers, which is exactly what RBC.6 exists to
prevent — so the gate is doing its job and must not simply be widened.

## What closing it needs

A precise null trap at the **top-level** `getfield`/`putfield` arms of
`x64.rs` (the existing one added in `5bf306bb0` has a single call site, on
the inlined-callee `putfield` path), publishing the pending NPE and its
exceptional frame before routing to the Java handler — the same thing the
invoke arms do via `emit_post_invoke_exception_check`. Then
`precise_frame_publishing_opcode` can re-admit `0xb4`/`0xb5`, gated behind
its own opt-out flag so one binary can be A/B'd against itself, as the
virtual-invoke admission was.

`probes/Rbc6FieldProbe.java` is the acceptance test that already exists and
already fails; it must pass before the opcodes go back in the list.

## Blast radius

Wider than date formatting. Every javac `synchronized (…) { … }` block whose
body touches a field is refused by this gate, in every suite. The
date-formatting path is simply where it became the *only* remaining reason
after `ldc <Class>` was fixed.

## Related

* `docs/internal/fixed-suite-bugs/tomcat/31-synchronized-code-never-jit-compiled-FIXED.md`
  — claims `getfield`/`putfield` "are therefore admitted at the relevant
  precise-frame sites". **Stale**: they were removed again the same week.
* `docs/internal/fixed-suite-bugs/tomcat/23-charsetcache-pathological-slowdown.md`
  — the measured cost of leaving one of these interpreted (~15 µs per call
  against ~2 µs compiled).
* `docs/feature-designs/jit-local-exception-handlers.md` — RBC.6's design.
