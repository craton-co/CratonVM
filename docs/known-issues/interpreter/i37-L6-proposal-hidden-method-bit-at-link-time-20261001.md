# Proposal: record `Method::is_hidden` once, at link time, for every frame shape

**Status: proposal — filed 2026-10-01 by interpreter round i1 wave 37, lane
L6. Not implemented.**

## Where things stand

After waves 34, 35 and 37 a hidden frame is decided in four places, three of
them per capture:

| Frame shape | Where | Cost per capture |
|---|---|---|
| interpreted, cached method | `CachedBytecodeMethod::hidden_frame` (`OnceLock`) | one load |
| compiled activation / inlined level | `stackwalker::compiled_label_is_hidden`: a label screen, then a per-thread memo row keyed by label, store and class id | a prefix test; a hash of the label for a JDK or hidden-class frame |
| `Owned` interpreter frame | `stack_entry_is_hidden`: class-name screen, then `method_is_hidden` (a method scan and an annotation read) | a scan per JDK frame |
| `StackWalker` / another thread's entries | the same, once per walk, under the class-manager read | a scan per JDK frame (an index hit when the capture resolved one) |

Each copy re-derives the same fact, `Method::is_hidden`, from the class
store: `Class::is_hidden()`, or `@jdk.internal.vm.annotation.Hidden` on a
privileged class's method (`entry_may_be_hidden`, the name approximation of
"privileged"). Wave 37 had to align them by hand once already (the
interpreted arm skipped the privilege screen).

## Proposal

1. **Link time.** When a class is linked, compute a per-class bit set
   "method `i` is hidden" (`Class::is_hidden()` sets every bit; otherwise the
   `@Hidden` annotation on a method of a class the defining loader makes
   privileged: the boot or platform loader, which is what HotSpot's
   `ClassFileParser` asks, rather than a name prefix). A `Box<[u64]>` on
   `Class`, empty for the overwhelming majority of classes (no hidden method).
   Owner: lane L5 (`classloading`).
2. **Readers** (lane L6): `method_is_hidden` becomes a bit test by method
   index; `StackTraceEntry::method_index` is already present on most entries,
   and a `CachedBytecodeMethod` can carry its index. The label memo and the
   `hidden_frame` `OnceLock` can go.
3. **The JIT** (lanes L2/L3): the emitter that records an `InlinedLevel`
   knows the callee's method; stamping the bit into the level (and into the
   artifact for its own method) makes a compiled or inlined frame's answer a
   field read, with no class-store access at capture time at all. That also
   lets a capture decide hidden frames BEFORE the `MaxJavaStackTraceDepth` cut,
   inside `capture_throwable_slots`, instead of wave 37's redo of a cut
   capture.
4. **Redefinition**: a retransformed class recomputes its set (annotations
   may change), which the memo keyed by label cannot notice today.

## Why it is worth it

* One rule, one place: the next walker (JVMTI `GetStackTrace` filtering,
  `StackWalker.getCallerClass`, JFR stack traces) reads a bit rather than
  growing a fifth copy.
* The throwable capture of a trace through compiled JDK frames stops hashing
  labels, and a walk stops scanning methods under the class-manager lock.
* HotSpot's privilege rule replaces the name approximation.

## Measure first

`L7/L7W34ThrowableCostBench` has no compiled JDK frame in its trace; add a
row that throws through compiled `java.util` frames (a `HashMap.computeIfAbsent`
whose function throws, say) and compare wave 37's memo against the bit.
