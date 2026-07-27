# Two conservative JIT-admission bans leave `TestMethodPerformance`'s whole hot path interpreted

**Status:** 🔴 **OPEN.** Residual of
[24](24-stringcache-oom-under-load.md) (whose `OutOfMemoryError` is FIXED).
This is a *throughput* residual, in the family of
[04](04-embedded-server-throughput-wall-OPEN.md) and
[29](29-throughput-wall-recurrence-and-unconfirmed.md) — but unlike those it
is root-caused here to two **specific, named** admission gates, both of which
were added deliberately to close real silent-corruption bugs.

## Symptom

`org.apache.tomcat.util.http.TestMethodPerformance` runs 6 × 100 000 000
iterations of `mb.setBytes(...); mb.toStringType();` and then 6 × 100 000 000
of `Method.bytesToString(...)`. HotSpot finishes the class in **41.2 s**.
CratonVM sustains ≈ **33 000 iterations/s** on the first loop against
HotSpot's ≈ 15 000 000/s, i.e. the class needs on the order of **hours**, not
seconds. Before the [24](24-stringcache-oom-under-load.md) fix this was masked:
the run died with a spurious OOM at ~150-600 s and never reached a timeout.

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

This one sits in the *middle* of the per-iteration call chain — `toStringType`
→ `ByteChunk.toString` → **`StringCache.toString`** → `ByteChunk.toStringInternal`
— and both of its neighbours DO compile, so every iteration pays a
compiled→interpreted→compiled transition. Note the hot path never even enters
the synchronized block: `tomcat.util.buf.StringCache.byte.enabled` is false by
default, so `bcCache` is null and the method falls straight through to
`toStringInternal`. The ban is purely static.

The bail is also **silent** by default: it reports only
`backend_attempted=false` under `CRATONVM_DBG_JITC`, which reads like a
transient resolver miss. Naming the gate requires the separate
`CRATONVM_DBG_RBC6=1`. A "hot method never compiles, no diagnostic says why"
shape is worth making self-reporting.

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
3. **Narrow RBC.6** to the handlers it actually needs: a javac-generated
   `synchronized`-block monitor handler is a recognisable shape whose "unsafe"
   local read is provably the monitor slot the `monitorenter` wrote.
4. **Make the silent bails self-reporting** — fold the `rbc6-dbg` reason into
   the default `compile-bail` line so "hot method never compiles" is one run,
   not three.

## Reproduction

```bash
CP=$(cat apps/tomcat/.suite/cp.txt)
CRATONVM_DBG_JITC=1 CRATONVM_DBG_RBC6=1 <cratonvm.exe> -Xmx2g -cp "$CP" \
  org.junit.runner.JUnitCore org.apache.tomcat.util.http.TestMethodPerformance \
  2>&1 | grep -iE 'TestMethodPerformance|StringCache'
```

Both diagnostic lines appear within the first ~30 s, long before the class
would finish.
