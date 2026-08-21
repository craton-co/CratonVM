# `HttpHeaderValidationUtilTest` — the caught exception is fixed; what is left is 1.67 deopts per iteration

**Status: OPEN, throughput — and the reason has moved for the third time.**

Two things this page led with are now CLOSED:

* the two exhaustive `@Test` loops ran entirely interpreted, because the OSR
  door refused any method with an exception table (`RBC.6b`, lifted
  2026-08-17);
* **a caught exception cost one OSR round trip, ~1 200-2 900 ns.** Compiled
  local exception handlers landed 2026-08-20 and are default-ON
  (`CRATONVM_JIT_LOCAL_HANDLERS`); a `catch` block in a compiled method now
  runs in compiled code, in the same frame. On this class's own probe the OSR
  round trips go **127 072 -> 0**.

What is left is a different defect, measured 2026-08-20 and named at the bottom
of this page: **1.67 `ReceiverTypeChanged` deopts per loop iteration**, ending
in `MakeNotCompilable`, on a receiver that is genuinely bimorphic. That, not the
throw path and not the call chain, is now the whole gap.

All numbers below: **Azure Linux host `vm1`, idle (load 0.0)**, 2026-08-20,
release build, real-JDK mode, one binary with the flag off and on, against
HotSpot 25 on the same host.

## Where the class stands

| | found | started | ok | wall |
|---|---:|---:|---:|---:|
| HotSpot 25, whole class | 5506 | 5506 | 5506 | **20.230 s** |
| CratonVM, default (everything below ON) | — | — | — | **no `@@RESULT` at a 1500 s cap** |

The 180 s per-class wall over 8 589 934 592 iterations is **21 ns/iteration**;
HotSpot does it in ~2.4. The class is still roughly an order of magnitude out,
and the section that says why is [What is left](#what-is-left).

**`probes/io/netty/handler/codec/http/HeaderValidationLoopRate.java` remains
uncalibrated on this host and its ABSOLUTE numbers must not be used** — it hits
a throw rate far above the real 7.7% and extrapolates the class to 10-15x what
it costs. Read it only as a same-host, same-binary ratio between two arms, and
use `CratonRunner` on the real class for anything else. Its COUNTERS, on the
other hand, are exact, and everything decisive on this page below is a counter.

## FIXED: a caught exception no longer leaves compiled code

The finding this page carried was right and is now closed. The compiled body
could not enter its own handler, so every caught exception left the artifact
entirely — reason-9 deopt, exceptional-frame reconstruction, handler search,
in-place transfer into the live interpreter frame — ran one interpreted
iteration, and re-entered at the next hot back edge.

`jit/src/x64/deopt_stubs.rs` now emits, at every throwing site inside one of the
method's own protected ranges, a **local-handler stub**: it asks
`jit_local_handler_lookup` which of that bci's candidate handlers takes the
pending throwable (JVMS order, catch-all first-match, typed entries resolved
through the compiling class's own loader — the same rule
`find_jit_exception_handler` applies), and on a hit jumps straight into the
compiled handler block with the throwable already stored in the frame slot the
handler's operand stack starts at. On a miss it falls through to exactly the
edge that ran before, so nothing about the propagating case changes.

**Counted, on `HeaderValidationLoopRate` at `n=100 000`, one binary:**

| | `CRATONVM_JIT_LOCAL_HANDLERS=0` | default (ON) |
|---|---:|---:|
| `osr_entered` | 127 081 | **9** |
| `osr_exception_handler_entered` | 127 072 | **0** |
| `local handlers: entered` | 0 | **127 072** |
| `local handlers: propagated` | 0 | 0 |
| frame-deopt entries | 167 384 | 167 133 |

Every catch is taken in compiled code and the OSR round trip is gone. The last
row is the point of the rest of this page: **the deopts did not move**, because
they were never the exception.

`probes/OsrExcRateProbe.java` prices the throw itself. Five identical
once-invoked loop bodies differing only in throw rate, so the rate=0 arm is the
control and `(t(rate) - t(0)) x rate` is the per-throw cost. Two interleaved
rounds:

| throw rate | HotSpot | flag OFF | flag ON |
|---|---:|---:|---:|
| 1/64 | 44.6-45.8 ns | 1 173-1 286 ns | **77-117 ns** |
| 1/8 | 7.8-8.0 ns | 1 186-1 195 ns | **89-94 ns** |
| 1/1 | 3.2-3.4 ns | 1 200-1 257 ns | **93-100 ns** |

**~12.5x**, and the gap to HotSpot goes from ~250-370x to ~12-25x. `sink` and
`caught` are byte-identical in every arm, so this is a speed result and not a
correctness one.

On the class's own loops (`HeaderValidationLoopRate`, ratio only): the NAME loop
goes **5 493-6 723 -> 3 486-3 508 ns/iter**, ~1.6-1.9x. The VALUE loop does not
move at all, and the deopt census below is why.

### Two OSR-admission defects fell out of building it

Both were pre-existing, both are one-line refusals that had grown a second
meaning, and both are why the feature engaged on nothing at all in its first two
builds — caught by `methods-armed=0`, not by any test:

* **`RBC.6` refused OSR for any method containing `athrow`** (`has_athrow`),
  because an `athrow` inside a protected range had no compiled handler to go
  to. With local handlers that premise is gone for exactly the methods that
  have them, and the refusal is now conditional. It was also refusing methods
  whose `athrow` is not in a protected range at all, which was never necessary
  — the sibling page's own `StatusLoopArmsProbe` lost an arm to it.
* **`osr_entry_bci_is_admissible` refused any OSR entry pc inside a protected
  range.** Every `try { for (..) {..} } catch` in the language has its back edge
  inside the `try`, so this refused OSR for the whole shape this class is made
  of. The reason it gave — that the entry contract cannot describe a frame
  mid-`try` — is about the HANDLER's frame, which is now the compiled frame's
  own.

Fixing the first without the second produced `methods-armed=2 sites-emitted=2
entered=0`: armed, emitted, and never reached. A count of what was EMITTED is
not a count of what RAN, and only the third counter said so.

### What the feature deliberately does not cover

* **`athrow` inside a `try`.** A `throw` statement caught by its own method
  still leaves compiled code. Only sites that go through
  `record_exception_check_edge` — a call, an allocation, a `checkcast` — get a
  local-handler stub. This class throws from a CALLEE, which is the covered
  case; a rethrowing handler is not.
* **A pending NPE / AIOOBE / `ArithmeticException` signal**, which is a request
  to BUILD a throwable rather than a throwable, and a bare deopt, which is not
  an exception. Each answers "not mine" and takes the old route.
* **A method the register allocator has not modelled handler edges for.** The
  feature requires `precise_exception_frames`, which is what makes
  `allocate_registers_with_handlers` see the exception edges; without them a
  local jumping into a handler could read a register another local owns there.

## What is left

**One thing, and it is not on this page's previous list.**
`CRATONVM_DBG_DEOPT=1` on `HeaderValidationLoopRate`, `n=100 000`:
**167 133 frame-deopt entries — 1.67 per loop iteration**, and they are the same
with the local-handler flag off (167 384), so they are pre-existing and this
page simply never looked. The census names two sites:

```
[cratonvm-deopt] HeaderValidationLoopRate.oldHeaderValueValidationAlgorithm:(Ljava/lang/CharSequence;)V
                 reason=ReceiverTypeChanged bci=6 action=MakeNotCompilable
[cratonvm-deopt] HttpHeaderValidationUtil.validateValidHeaderValue:(Ljava/lang/CharSequence;)I
                 reason=ReceiverTypeChanged bci=1 action=MakeNotCompilable
```

The receiver at those sites alternates between `AsciiString` and the
`CharSequence` wrapper the test builds — the loop calls each validator with
both, deliberately. So the speculation is not mis-tuned; it is **wrong about the
program**, and no amount of recompiling fixes it. `recommend_action` gives
`ReceiverTypeChanged` `RecompileAndReinterpret` on every occurrence until the
per-method count crosses `max_deopts_per_method`, then `MakeNotCompilable` — so
the method is recompiled per call for a while and then **barred from compilation
permanently**. `jit/src/x64/bytecode_walk.rs`'s own header comment describes
this exact trap for a different guard, and moves that screen to compile time to
avoid it.

**And the mechanism that was built to prevent it is inert here.** The per-bci
de-spec registry does fire — the same census carries 482 of these:

```
[cratonvm-deopt] per-bci de-spec: ...oldHeaderValueValidationAlgorithm bci=6
                 (N deopts >= 4) — speculation suppressed on next compile
                 (method stays compilable)
```

but `despec_contains` is only ever CONSULTED at four places
(`jit/src/x64/driver.rs`'s two loop-hoist gates and two intrinsic sites in
`bytecode_walk.rs`), all of them speculative **bounds-check** elimination.
Nothing on the receiver-guard path reads it. So the registry records the site,
prints a line claiming the speculation is suppressed, the next compile emits the
same receiver guard anyway, and the method is blacklisted regardless — a feature
that reports itself on while being structurally inert, the same shape as the
field-site cache that shipped switched off.

**The next lever is therefore to make receiver-type speculation consult the
de-spec registry**, so a bci that has already deopted `PER_BCI_DESPEC_LIMIT`
times is compiled without the guard and the method stays compiled. That is one
read in the emitter and a policy arm in `recommend_action`; what makes it
delicate rather than trivial is that the `MakeNotCompilable` escalation is
load-bearing and was tuned by measurement (see the Tomcat
`TestResponsePerformance` note on `DeoptReason::OsrExit`), so it needs its own
A/B with the deopt count as the counter — **1.67 per iteration is the number to
move.**

Everything else this page used to list as remaining is either done or measured
dead:

1. ~~**Compiled exception handlers.**~~ **DONE 2026-08-20**, above.
2. ~~Two cheap items on the OSR round trip.~~ DONE 2026-08-17.
3. **The call chain and the 21 ns/iteration floor.** Unchanged and still real:
   `probes/CallArgCostProbe.java` prices a compiled static call at **4.13 ns**,
   one taking a reference at **6.46**, a virtual one at **8.19-8.96**, against
   HotSpot's ~0. Ten call frames cost 40-80 ns before any of them works, and the
   budget for the whole iteration is 21. But this is now the SECOND item, not
   the first: at 1.67 deopts per iteration the call frames are not what the
   iteration is spending its time on.
4. **The inline chain the sibling page documents does not reach this loop, and
   cannot.** `compile_osr_artifact` hands the backend an EMPTY `inline_sites`
   map, so an OSR artifact splices nothing — ever — and a `@Test` body invoked
   once has no other compiled form. Everything the sibling page's five steps
   bought is collected inside the CALLEES. `probes/OsrVsEntryInlineProbe.java`
   prices wiring the planner into the OSR door by running one loop body through
   both doors in one process, and the answer is **no difference** (47.16 vs
   47.43, 49.76 vs 53.99, 46.44 vs 47.23 ns/iter over three rounds, with
   `CRATONVM_JIT_MAIN_INLINE` inert in both). Do not spend a session on it.

## Repro

```bash
cd apps/netty-suite-runner
timeout 1500 java @common.args CratonRunner io.netty.handler.codec.http.HttpHeaderValidationUtilTest
```

```bash
timeout 1500 cratonvm --java-home <jdk> -Xmx1500m @common.args CratonRunner io.netty.handler.codec.http.HttpHeaderValidationUtilTest
```

The A/B for the local-handler fix, one binary:

```bash
cratonvm --java-home <jdk> -cp . OsrExcRateProbe 2000000
```

```bash
CRATONVM_JIT_LOCAL_HANDLERS=0 cratonvm --java-home <jdk> -cp . OsrExcRateProbe 2000000
```

The counters that prove it:

```bash
CRATONVM_DBG=jit-method-stats cratonvm --java-home <jdk> @common.args io.netty.handler.codec.http.HeaderValidationLoopRate 100000 2>&1 | grep -E 'local handlers|OSR lifecycle'
```

The deopt census that names what is left:

```bash
CRATONVM_DBG_DEOPT=1 cratonvm --java-home <jdk> @common.args io.netty.handler.codec.http.HeaderValidationLoopRate 20000 2>&1 | grep 'reason=' | sort | uniq -c | sort -rn | head
```

The eight-shape correctness gate for compiled handlers — two typed handlers over
one range, a strict-subclass catch, a propagating throw, a `finally`, a nested
`try`, a rethrowing handler, and a locals-survive arm — which must print the
same digest under HotSpot, under `--nojit`, and with the flag off and on:

```bash
cratonvm --java-home <jdk> -cp . LocalHandlerShapeProbe 200000
```

## What was ruled out

* **The `ByteBuffer.putInt(0, i)` native floor.** Thin direct helpers for
  `Buffer.session()` and `ScopedMemoryAccess.{put,get}IntUnaligned` were written
  and reverted, because the census says they never bind: those JDK accessors
  take the optimizing (IR) pipeline, whose direct-call lowering is register-only
  (`emit_direct_cross_call` requires `num_args + needs_context <= 4` on
  Windows), so a 6-argument `putIntUnaligned` cannot be bound there at all. With
  the helpers on and off, `--dump-native-registry` reports the identical census.
  Anyone picking this up should start by giving the IR path stack-arg
  marshalling, not by writing more helpers.
* **The 120 s JUnit method timeout not firing.** `common.args` sets
  `-Djunit.jupiter.execution.timeout.default=120s`, and Jupiter's default
  `SAME_THREAD` mode cannot preempt a synchronous non-interruption-checking
  loop. Not a separate defect. (The sibling class demonstrates the consequence:
  it now runs 13/13 `ok` in 193.8 s with that property set.)
* **Wiring the inline planner into the OSR door**, item 4 above — measured, not
  argued.

## Related

* `fixed-suite-bugs/jit/osr-refuses-any-method-with-an-exception-table-FIXED-20260817.md`
  — what this page's first blocker turned out to be, and the fix. It also
  records the two vacuous greens the fix passed through, both caught by an
  engagement counter and neither visible in any correctness result.
* [`httpresponsestatustest-exhaustive-loop-timeout-20260816.md`](httpresponsestatustest-exhaustive-loop-timeout-20260816.md)
  — the sibling, which is 7% off its wall rather than an order of magnitude, and
  which carries the measurement that the OSR door plans no inline sites.
* [`httpcontentdecompressortest-hang-20260816.md`](httpcontentdecompressortest-hang-20260816.md)
  — the native-call floor this class's `ByteBuffer.putInt` pays once per
  iteration.
* [`fastthreadlocal-2e9-iteration-throughput-wall-20260812.md`](fastthreadlocal-2e9-iteration-throughput-wall-20260812.md)
  — the same family of finding, with per-component throughput measurements.
