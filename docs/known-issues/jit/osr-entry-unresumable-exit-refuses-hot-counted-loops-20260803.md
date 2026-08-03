# Every OSR entry is refused `osr-entry-unresumable-exit` — a hot counted loop in a rarely-invoked method never leaves the interpreter

**Status:** OPEN. Found 2026-08-03 while re-measuring
`probes/StaticFieldProbe.java` for the `getstatic` work
(`docs/internal/jit-getstatic-costs-a-helper-call-FIXED-20260803.md`); that fix
does not touch this and does not depend on it.

## Symptom

A method that is invoked *few* times but whose loop is hot — the classic
benchmark harness, `main`, a once-called `run()` with the work inside — runs
**fully interpreted**, at ~60x compiled cost, with no crash, no deopt and no
JIT-failure signal. `--nojit` and default produce identical timings, which is
the tell.

Measured, `StaticFieldProbe` at 2M iterations (JDK 25 real-JDK boot, Azure
Linux):

| rung | interpreted (OSR refused) | compiled | HotSpot |
|---|---|---|---|
| control (`acc += i ^ (acc >>> 7)`) | 90.83 | 1.57 | 0.84 |
| instance field | 444.63 | 1.56 | 2.01 |

`CRATONVM_DBG=jit-method-stats` reports `still-interpreted=7 ... compiles: c1=0
c2=7 osr=7` — the artifacts are *built* (7 OSR compiles) and then refused at
every entry.

## What the trace says

```
$ CRATONVM_DBG=jitc cratonvm --java-home <jdk25> -cp <probes> StaticFieldProbe 200000
[cratonvm-jitc] OSR-compile StaticFieldProbe.staticMutInt(I)J entry_pc=4 len=751
[cratonvm-jitc] OSR-reuse   StaticFieldProbe.staticMutInt(I)J entry_pc=4
[cratonvm-jitc] OSR-refuse  StaticFieldProbe.staticMutInt(I)J entry_pc=4 JIT bailout
    [unsupported_shape]: unsupported shape: osr-entry-unresumable-exit
    (deopt point at bci 1 (OsrExit) reconstructs an unresumable frame) (memoed)
```

All **seven** rungs of `StaticFieldProbe` and all **four** of `VirtOnlyProbe`
are refused identically — every one of them an ordinary `for (int i = 0; i < n;
i++)` over a `long` accumulator, with no exception handler, no monitor, no
`invokedynamic`. The refusal names `bci 1`, which is *before* the loop: the
artifact's own `OsrExit` deopt point, not anything in the body.

The refusal is by design and documented (`docs/jit/on-stack-replacement.md`
§ refusal taxonomy, `OSR_REFUSE_UNRESUMABLE_EXIT` in `jit/src/lib.rs`) — it
exists to stop committed loop iterations being discarded by a bail that cannot
name a resume point, which was a real silent-corruption bug. The question this
doc raises is not whether the guard should exist; it is why the *simplest
possible counted loop* trips it, which makes OSR unavailable in general rather
than for an exotic shape.

Secondary observation: the refusal is logged as `(memoed)` yet reappears 406
times for the same `(method, entry_pc)` in one run, once per back-edge budget
retry. Whatever the memo is suppressing, it is not the re-attempt.

## Why it matters beyond benchmarks

This is one more entry in the "hot method never compiles" symptom class — 100x
slow, nothing crashes — alongside the deliberate admission bans (RBC.6's
handler-reads-unsafe-local, RBC.7's `invokedynamic` OSR denial, the `<init>`
complexity ban). Unlike those it is not a property of the *bytecode*, so no
source-level workaround exists. It also silently invalidates any microbenchmark
that warms up by
*iterations* rather than by *invocations*. Every number taken from such a probe
measures the interpreter. `probes/StaticFieldProbe.java` was corrected on
2026-08-03 to warm up 1200 invocations per rung (past `c1_threshold=500`) so it
tiers up normally; other probes in `probes/` have not been audited for this.

**Check the control rung against HotSpot before reading any marginal.** Within
~2x is compiled; 50-100x is the interpreter.

## Reproduction

```bash
CRATONVM_DBG=jitc cratonvm --java-home <jdk25> -cp <probes> StaticFieldProbe 200000 2>&1 | grep OSR-refuse
```

Any once-invoked method with a counted loop reproduces it; `StaticFieldProbe`
and `VirtOnlyProbe` both do, on every rung.

## Where to look

* `jit/src/lib.rs` — `OSR_REFUSE_UNRESUMABLE_EXIT`, `validate_osr_entry`,
  `deopt::frame_state_is_resumable`.
* `vm/src/runtime/interpreter/invoke.rs` (~16060) — the admission-time refusal
  and the reasoning for moving it there from the transfer site.
* `docs/jit/on-stack-replacement.md` §§ "refusal taxonomy", "OSR exit policy".

The first question to answer: what makes the `OsrExit` deopt point at bci 1
unresumable for a loop with an empty operand stack and only int/long locals —
`MaterializationRequired`/`Unsupported` slots, a non-`REEXECUTE` semantics tag,
or a `caller` scope that is `None` where the check expects `Some`.
