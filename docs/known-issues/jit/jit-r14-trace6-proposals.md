# JIT round 14 proposals, lane trace4 (wave 5)

Status: OPEN (proposal book; ideas, not work items, until the owner queues one)
Area: VM-served JDK and native frames in throwable captures (`vm/src/runtime/stackwalker.rs`,
`vm/src/vm/vm_exec.rs` capture, the natives in `native-builtins`)
Found by: round 14 wave 5 lane trace4

Ranked by expected benefit over cost. Earlier books: `jit-r14-trace-proposals.md`,
`jit-r14-trace3-proposals.md`, `jit-r14-trace4-proposals.md`, `jit-r14-trace5-proposals.md`.

## TR6-1. Middle native frames for natives that call Java before throwing (`Class.forName0`)

**What.** Wave 5's one-frame rule (`native_leaf_frame`) adds a native's frame only at the
INNERMOST end. A native that calls back into Java and lets that Java throw leaves the native's
frame missing in the MIDDLE: `Class.forName("x")` -> `forName0` -> the loader's `loadClass`
throws `ClassNotFoundException`; HotSpot shows `BuiltinClassLoader.loadClass`,
`ClassLoaders$AppClassLoader.loadClass`, `ClassLoader.loadClass`, `Class.forName0(Native Method)`,
`Class.forName`. Rule: frame `i` stands at an `invokestatic` / final / private call of an
`ACC_NATIVE` method `N`, frame `i + 1` is not `N` -- and `N` is on a short census of natives that
call Java (`forName0` -> `loadClass`, `NativeMethodAccessorImpl.invoke0` -> any,
`NativeConstructorAccessorImpl.newInstance0`), so an unrelated middle gap is never filled.
**Benefit:** every class-loading failure trace in logs (Spring, Tomcat, JDBC driver probing) gets
HotSpot's middle frame. **Cost:** small (the middle-insertion loop of
`insert_thread_run_middle_frames` is the template). **Risk:** a native served by a Bridge that
calls a DIFFERENT Java method than the real native: the census names the callee. **First step:**
a probe row printing `Class.forName("nope")`'s first six frames on HotSpot 25 and CratonVM.

## TR6-2. Throw-site hints for JDK bodies served natively (`VirtualThread.sleepNanos`, `sleep(long, int)`)

**What.** Generalize TR5-3's timed-join hint (patch page
`r14w5-trace4-thread-run-memo-and-timed-join-hint-patch-FIXED-20260929.md`) into one per-thread
`(method, site)` hint a served native sets on its throwing return: "the real body would have left
from call / `new` site k". The census then picks site k instead of first / last, and a new
`athrow` row kind (the frame at the `invokespecial <init>` of site k, no leaf) covers Java bodies
that raise the throwable themselves. Rows it unlocks: a virtual thread's registered `sleep` under
`--compatible` (`VirtualThread.sleepNanos`: pre-interrupted site vs after-park site), `Thread.sleep(
long, int)` / `join(long, int)` IAE (two sites each). **Benefit:** the remaining `--compatible`
residue of item 5. **Cost:** medium (one `JvmThread` field, the natives, a row kind). **Risk:** a
hint read by the wrong capture -- take it on every capture. **First step:** land TR5-3.

## TR6-3. A per-callee census of served native frames (measure hook)

**What.** `CRATONVM_DBG_STTRACE=1` prints each capture's frames; add one line per capture that got
a native leaf / stand-in (`STTRACE_DBG_LEAF <class.method> <throwable>`), and one per REFUSAL of
the one-frame rule with its reason (virtual dispatch, receiver NPE, linkage, hidden). **Benefit:**
the Spring / Tomcat censuses tell which refusals matter (e.g. how often a non-final virtual native
throws) before any rule is widened. **Cost:** small. **Risk:** none (debug-gated, and the gate is
already read per capture). **First step:** the two `eprintln!` lines behind the existing
`env_cache::dbg_sttrace()`.

## TR6-4. Exact dispatch for non-final virtual natives from the interpreted receiver

**What.** The one-frame rule refuses `invokevirtual` of a non-final native (`Object.hashCode`,
`Object.clone` via a public override chain, `Thread` natives that are overridable) because the call
may have dispatched to a VM-served override. For an INTERPRETED innermost frame the receiver is
still in its operand-stack slot below the arguments at the invoke (the VM pops by moving `sp`, the
slot is intact until the next push): its class decides the dispatch exactly. **Benefit:** small
(few non-final natives throw). **Cost:** small-medium (reading a frame's dead stack slot needs the
frame's `max_stack` layout; GC may have moved the object -- only its class id is read, from the
header). **Risk:** a slot overwritten by a nested call's frame setup; validate the header's class
is a subclass of the resolved method's class, else refuse. **First step:** count TR6-3's
virtual-dispatch refusals on a Spring census.

## TR6-5. Retire `--compatible` registrations whose only effect on traces is a lost hop (`Array.newInstance`)

**What.** `Array.newInstance(Class, int)` is Java in JDK 17-25 (`newArray` is the native); a
registration over it (if `--compatible` has one) drops `Array.newInstance(Array.java:...)` from a
`NegativeArraySizeException` trace. The census could rebuild it (a row `Array.newInstance` ->
`newArray`), but the cleaner fix is T3-2's: leave the Java body to run. **Benefit:** exact
reflection-allocation traces. **Cost:** small per family (the `Optional` retirement of wave 2 is the
template). **Risk:** speed of reflective allocation under `--compatible`; measure. **First step:**
`R14Trace4NativeLeaf` row `newArrayNase` under `--compatible` against HotSpot.
