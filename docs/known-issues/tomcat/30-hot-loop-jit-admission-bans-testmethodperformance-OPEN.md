# Two conservative JIT-admission bans leave `TestMethodPerformance`'s whole hot path interpreted

**Status:** 🔴 **OPEN.** Residual of
[24](../../internal/fixed-suite-bugs/tomcat/24-stringcache-oom-under-load-FIXED.md) (whose `OutOfMemoryError` is FIXED).
This is a *throughput* residual, in the family of
[04](04-embedded-server-throughput-wall-OPEN.md) and
[29](../../internal/fixed-suite-bugs/tomcat/29-throughput-wall-recurrence-and-unconfirmed-CLOSED.md) — but unlike those it
is root-caused here to two **specific, named** admission gates, both of which
were added deliberately to close real silent-corruption bugs.

## Symptom

`org.apache.tomcat.util.http.TestMethodPerformance` runs 6 × 100 000 000
iterations of `mb.setBytes(...); mb.toStringType();` and then 6 × 100 000 000
of `Method.bytesToString(...)`. HotSpot finishes the class in **41.2 s**.

Measured end-to-end on the post-[24](../../internal/fixed-suite-bugs/tomcat/24-stringcache-oom-under-load-FIXED.md) binary,
from the class's own printout:

```
.MessageBytes conversion took :3820342393100ns      (CratonVM, one 100M loop)
MessageBytes conversion took :6573830400ns          (HotSpot, same loop)
```

**3820 s vs 6.6 s for the same 100M iterations — ~580x.** Six such loops put
phase 1 alone at ~6.4 hours, so the class cannot finish inside any suite
timeout. Before the bug-24 fix this was masked: the run died with a spurious
OOM at ~150-600 s and never reached a timeout.

That same run is also the end-to-end confirmation for bug 24 — it cleared
200 000 000+ iterations with no `OutOfMemoryError`, against a pre-fix baseline
that died before 10 000 000.

## Root cause — two independent admission bans on the same hot path

Both are visible in one `CRATONVM_DBG_JITC=1` run of the class:

### 1. The driving loop is permanently OSR-denied (RBC.7 `invokedynamic` ban)

```
[cratonvm-jitc] bg-compile org/apache/tomcat/util/http/TestMethodPerformance.testGetMethodPerformance()V tier=C2 optimized=true osr_bci=15
[cratonvm-jitc] OSR-compile FAILED org/apache/tomcat/util/http/TestMethodPerformance.testGetMethodPerformance()V osr_bci=15 — method marked OSR-denied for the rest of this process
```

`compile_osr_artifact` (vm/src/runtime/interpreter.rs) refuses any method
containing `invokedynamic`:

```rust
if !scan.indy_ops.is_empty() { return None; }
```

`testGetMethodPerformance` contains two, from the
`System.out.println("MessageBytes conversion took :" + duration + "ns")`
string concatenations *after* each loop (Java 9+ lowers `+` on strings to
`invokedynamic StringConcatFactory`). The ban exists for a real reason — see
`docs/internal/jit-osr-loop-duplicate-execution-silent-corruption-FIXED.md`:
the 0xba arm lowers every indy site to an unconditional deopt trap, and for an
OSR frame that bail resumes at the *stale* pre-OSR back-edge, silently
re-executing a loop whose side effects were already committed. The RBC.7
comment even names this exact shape ("a `System.out.println("..." + n + ...)`
immediately following the loop") as the motivating case.

The cost is that the test method is a once-invoked harness method with the hot
loop inline, so OSR is the *only* way it can ever run compiled. Denied, all
600 000 000 iterations of loop control run in the interpreter.

### 2. `StringCache.toString(ByteChunk, …)` never compiles (RBC.6 handler-safety gate)

```
[rbc6-dbg] try_compile_inner: local_handler_reads_unsafe_local=true for org/apache/tomcat/util/buf/StringCache.toString(Lorg/apache/tomcat/util/buf/ByteChunk;Ljava/nio/charset/CodingErrorAction;Ljava/nio/charset/CodingErrorAction;)Ljava/lang/String;
[cratonvm-jitc] compile-bail org/apache/tomcat/util/buf/StringCache.toString(...)Ljava/lang/String; backend_attempted=false
```

`local_handler_reads_unsafe_local` (jit/src/lib.rs) conservatively refuses a
method whose exception handler reads a local it has not itself written. The
`synchronized (bcStats) { … }` block in `StringCache.toString` compiles to a
javac-generated monitor handler that does exactly that (it reloads the monitor
local to `monitorexit` it), so the method is refused.

> **This gate is analysed canonically in
> [23](23-charsetcache-pathological-slowdown.md)**,
> which reached it independently the same day via
> `CharsetCache.getCharset` and went further: RBC.6 fires on *ordinary*
> try/catch too, because only **parameter** slots count as reconstructible, and
> the `precise_exception_frames` escape hatch whitelists only
> `invokestatic`/`monitorenter`/`monitorexit` — so one `invokevirtual` inside
> the try region is enough. Read 23 before touching this gate; the two docs
> describe one bug with two victims.

This one sits in the *middle* of the per-iteration call chain — `toStringType`
→ `ByteChunk.toString` → **`StringCache.toString`** → `ByteChunk.toStringInternal`
— and both of its neighbours DO compile, so every iteration pays a
compiled→interpreted→compiled transition. Note the hot path never even enters
the synchronized block: `tomcat.util.buf.StringCache.byte.enabled` is false by
default, so `bcCache` is null and the method falls straight through to
`toStringInternal`. The ban is purely static.

The bail used to be **silent**: it reported only `backend_attempted=false`
under `CRATONVM_DBG_JITC`, which reads like a transient resolver miss. Fixed
here — the refusal now names itself on the default trace as
`resolver-bail site=rbc6-handler-reads-unsafe-local`. The fastest first check
for this whole shape is `CRATONVM_DBG_JIT_METHOD_STATS=1`, which prints
`hot_but_stuck_in_interpreter=N` with the offending method and its
`tier_fail_count`.

## Supporting measurements (isolated probes, this host, JDK 25 HotSpot control)

> **Caveat on absolute rates.** This Windows box was multitenant throughout
> (17 concurrent `cratonvm` processes from other sessions at one point), so
> treat every absolute figure below as a *lower bound* — see
> `feedback_shared_host_multitenant_confound`. The HotSpot control ran under
> the same conditions, and the two structural findings above (OSR-denied,
> RBC.6-refused) are compile-time facts read out of a trace, not timings, so
> neither depends on host load.

Per-operation cost in a JIT-compiled loop, nanoseconds. HotSpot's figures are
escape-analysed for the allocating rows, so treat those as a floor rather than
a like-for-like ratio; the CratonVM *column* is the interesting part.

| probe | what it does | HotSpot | CratonVM |
|---|---|---|---|
| `arith` | no call, no allocation | ~0 | 1 |
| `scall` | one `invokestatic` | ~0 | 7 |
| `vcall` | one `invokevirtual` | ~0 | 56 |
| `pcall` | one `invokespecial` (private method) | – | 60 |
| `directSet` | `putfield` on an old receiver | – | 54 |
| `allocArr` | `new int[1]` | – | 126 |
| `allocNoCtor` | `new Plain()` (empty ctor) | – | 162 |
| `allocArg` | ctor takes an arg, empty body | – | 278 |
| `allocNEsc` | allocation inside a **C1**-compiled callee | 3 | 105 |
| `allocEsc` | same allocation **inline in the C2/OSR loop** | 3 | 2331 |
| `allocBody` | `new Body()` whose ctor writes one field | – | 2338 |

Two leads fall out of this table that are **not** explained by either ban
above and are worth their own investigation:

* an allocation whose constructor has a body costs ~20× one whose constructor
  is empty (2338 vs 162 ns), even though the C2 artifact compiles cleanly and
  does not deopt (`CRATONVM_DBG_DEOPT=1` shows compile-time map emission
  only); and
* a C2/OSR loop that mixes an allocation with another helper-call op can enter
  an **endless OSR recompile loop** — the `allocPutOld` probe logged
  **200 `OSR-compile` events for 200 000 iterations**, alternating between two
  code buffers, i.e. one full C2 compile per 1 000 iterations, with the loop
  running interpreted in between. `allocBare` (same loop without the second
  op) compiles once and reuses.

## What a fix would involve

Neither ban should simply be relaxed — each closed a confirmed
silent-corruption bug, and the corruption they prevent is invisible (wrong
results, not crashes). Plausible directions, roughly in order of
value/risk:

1. **Make the OSR recompile loop stop** (the `allocPutOld` lead). Whatever
   causes the artifact to be discarded every ~1000 iterations is pure waste;
   fixing it does not require relaxing any safety property.
2. **Link `invokedynamic` in compiled code** instead of lowering it to an
   unconditional trap. That removes RBC.7's premise rather than its check.
3. **Narrow RBC.6** — but see
   [23](23-charsetcache-pathological-slowdown.md)
   first, which spells out why the obvious narrowing (widening
   `precise_exception_frame_sites_supported` to `invokevirtual`) is *unsafe*:
   several invoke lowerings in `jit/src/x64.rs` deliberately bypass
   `emit_post_invoke_exception_check`, and inlined callees never reach the
   caller's check, so widening reintroduces silent-wrong-locals. Doing it right
   means auditing every invoke lowering to publish the reason-9 frame first.
4. ~~Make the silent bails self-reporting~~ — **done**, see the trace line
   above.

## Reproduction

```bash
CP=$(cat apps/tomcat/.suite/cp.txt)
CRATONVM_DBG_JITC=1 CRATONVM_DBG_RBC6=1 <cratonvm.exe> -Xmx2g -cp "$CP" \
  org.junit.runner.JUnitCore org.apache.tomcat.util.http.TestMethodPerformance \
  2>&1 | grep -iE 'TestMethodPerformance|StringCache'
```

Both diagnostic lines appear within the first ~30 s, long before the class
would finish.

## Update 2026-07-27 — ban 2 (RBC.6) is lifted; ban 1 (RBC.7) remains

`precise_exception_frame_sites_supported` now admits `invokevirtual` /
`invokespecial` / `invokeinterface` alongside `invokestatic` and the monitor
ops, so the RBC.6 refusal of `StringCache.toString` is gone. The widening was
gated on auditing every lowering the `0xb6 | 0xb7 | 0xb9` codegen arm can
select — two of its four exits emit no call at all, the inline path is already
unreachable under `precise_exception_frames` (`inline_sites.clear()`), and the
remaining two both end in `emit_post_invoke_exception_check`. The sibling
tail-call, which tears the frame down before the callee runs and so escapes the
handler, is now suppressed inside protected ranges
(`x64::Compiler::pc_is_protected`); that was a pre-existing hole for
`invokestatic`, which this whitelist always admitted.

This addresses direction 3 of "What a fix would involve" above — though by
proving the *lowerings* safe rather than by pattern-matching the javac
`synchronized` shape, which is strictly more general.

**This does not on its own close this document.** Ban 1 (RBC.7 — a method
containing `invokedynamic` is permanently OSR-denied) is untouched, and
`testGetMethodPerformance` is a once-invoked harness method whose hot loop is
inline, so OSR remains the only way it can run compiled. Expect the
compiled→interpreted→compiled transition through `StringCache.toString` to be
gone and the per-iteration cost to drop, but the loop control itself still
interprets until RBC.7's premise is removed (direction 2: link
`invokedynamic` in compiled code instead of lowering it to a trap).

Sibling doc `23-charsetcache-pathological-slowdown.md` shares this gate and was
half-closed by the same change (its `timeFull < timeNone` assertion now passes).
