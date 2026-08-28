# `CRATONVM_SCALAR_DEOPT`: a second, independent soak — same verdict, one new blocker

`scalar-deopt-gauntlet-soak-20260827.md` (netty + hibernate) and this one ran in
parallel without knowing about each other. **They agree on the verdict and on
the reason** — the flag is green everywhere and the green is vacuous, because
the feature never executes. That page is the record; this one carries only what
it does not, and narrows one word in it.

## New: the designated pre-flip gate is RED

`docs/feature-designs/activate-ir-optimizer.md` names the differential harness as
the gate for this class of change — *"Every widening shipped behind its own flag
with a differential test"*. It was not run. It fails:

```
$ CRATONVM_SCALAR_DEOPT=1 cargo test -p cratonvm-jit --test ir_vs_singlepass
---- ir_elidable_trivial_init_on_fresh_new_is_still_elided ----
assertion `left == right` failed: the field written is the field read back
  left: 969312034704        <- varies per run; an address
 right: 11
test result: FAILED. 141 passed; 1 failed
```

`int f(int n) { Corpus c = new Corpus(); c.f0 = n; return c.f0; }` returns a
heap-address-shaped value. Still red on `d967c2515`, i.e. after the other soak
merged. Full write-up — including the control that isolates the elision as the
trigger, and the end-to-end run that does NOT reproduce it — in
`docs/known-issues/jit/scalar-deopt-elision-returns-the-object-in-the-differential-harness-20260827.md`.

### The word this narrows

That record concludes *"every deopt takes the whole-method re-run it always did
— which is also why the flag is safe."* The reasoning is right and the scope is
one step too wide. It establishes that no descriptor is ever CONSUMED, so no
resume can be misled by one. It does not cover the compiler emitting a wrong
value in a graph where the elision does happen — which is what the differential
catches, and which needs no deopt at all to hurt.

"Safe **because nothing it produces is ever read**" is the supported claim;
"safe" unqualified is not, while that test is red.

## New: Tomcat, 651 classes, four arms

The other soak used netty (200 classes) and one hibernate class. This is the
larger suite, and it reaches the same place from further away.

| arm | PASS | FAIL | HANG | CRASH | wall | **allocations elided** |
|---|---:|---:|---:|---:|---:|---:|
| A (none) | 620 | 27 | 4 | 0 | 45.9 min | **0** |
| B `SCALAR_DEOPT` | 617 | 28 | 5 | 1 | 44.8 min | **0** |
| C `+ C2_ALLOC_UPGRADE` | 611 | 30 | 10 | 0 | 49.1 min | **0** |
| D `+ IR_INLINE` | 618 | 27 | 6 | 0 | 48.0 min | **0** |

Zero engagement in every arm, including the two that lift the gates upstream of
this flag. The flags did reach the workers — each arm's logs carry the
launcher's own deprecation line naming them — so this is a result about the flag
rather than a broken experiment.

One class run by hand with the diagnostic
(`org.apache.catalina.mapper.TestMapperPerformance`): 11 allocation-bearing
methods reach escape analysis at C2, and all 22 of their allocations are refused
`Escapes(GlobalEscape)` — `Objects.requireNonNull`, `MessageBytes.<init>`,
`CopyOnWriteArrayList.<init>`, `StringUTF16.getChar`. The same finding as the
other page's 3 381-to-0 refusal census, on a third codebase.

### A noise floor, free

Four runs of effectively identical code gave **620 / 617 / 611 / 618** PASS: a
±9-class swing with no compiled-code change at all. The classes that churn are
the ones the runner's own header warns about, and they moved in BOTH directions
— two arms "fixed" a class the baseline failed:

```
TestMulticastPackages, TestTcpFailureDetector, TestNonBlockingAPI,
TestHostConfigAutomaticDeployment{Addition,Modification}, TestOcsp*,
TestGenerator, TestDefaultServletEncoding*
```

multicast, TCP failure detection, file-watching with sleeps, an OCSP responder.
**Anyone reading a Tomcat A/B on this host should treat a swing of this size as
nothing**, and should report which classes moved rather than only how many.

Without the engagement column, the defensible-looking readings of that table
were "B crashed a class and C lost six more to HANG — regression" and "A against
D is 620/618 — no harm found". Both are fiction about a flag that never ran.

## New: a compile-time half to the engagement census

The other soak added the runtime counter — one line per
`materialize_virtual_objects`, debug-gated. `cratonvm_types::scalar_deopt_census`
adds the compile-time half and makes both unconditional:

```
[scalar-deopt] census: rescued=6 blocked=0 materialized=0
```

* **`rescued`** — allocations elided BECAUSE a descriptor was available.
* **`blocked`** — proved replaceable, kept for want of one. This is what makes a
  zero readable: `rescued=0 blocked=0` means nothing in this shape exists and
  the arm proves nothing, while `rescued=0 blocked=N` would mean the flag was on
  and still could not describe them.
* **`materialized`** — runtime reconstructions, as before.

Two questions, not one: `rescued` says the compiler took the flag's path,
`materialized` says the recipe was executed. On the positive control
(`probes/ScalarDeoptProbe.java`, with `CRATONVM_JIT_C2_ALLOC_UPGRADE=1`) the
pair reads `rescued=0 blocked=6` with the flag off and `rescued=6 blocked=0`
with it on — the cleanest available demonstration that this gate is the only
thing the flag moves.

Unconditional, because it is two relaxed atomics bumped once per
scalar-replacement plan, and because an engagement census you have to know to
ask for is how a soak gets run without one. Printed from the `System.exit` path,
where `cell_census` reports and for the reason recorded there: a JUnit runner
never reaches `vm-cli`'s normal-return arm, so a census printed there produces
zero lines across a whole suite sweep — which is exactly the 651-class shape
above.
