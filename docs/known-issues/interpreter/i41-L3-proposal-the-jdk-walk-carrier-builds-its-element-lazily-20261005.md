# Proposal: the JDK walk's carrier builds its `StackTraceElement` at first read, not at the walk

**Status: proposal — filed 2026-10-05 by interpreter round i1 wave 41, lane
L3, from the remainder of
`docs/internal/fixed-bugs/interpreter-L3-a-stack-frames-line-and-file-are-not-read-lazily-FIXED-20261005.md`.
Not built. Only `CRATONVM_SW_JDK_WALK=1` reaches the carrier today.**

## The problem it removes

HotSpot resolves a `StackWalker.StackFrame`'s line and file at its first
`getLineNumber()` / `getFileName()` / `toStackTraceElement()` / `toString()`,
and answers `-1` / `null` when the frame's method is no longer its class's
current version by then (a redefinition between the walk and the read). The
native walk's carrier (`reflect_invoke::populate_stack_frame`, the default)
does the same since wave 40 (`p59_frame_settle`). The JDK walk's carrier,
`native-builtins/src/lang_stackwalker.rs::populate_sfi` (selected by
`CRATONVM_SW_JDK_WALK=1`), writes a finished `StackTraceElement` into the
`StackFrameInfo.ste` field at the walk. JDK 25's
`StackFrameInfo.toStackTraceElement` (read in `C:\craton\jdk25src`,
`java.base/java/lang/StackFrameInfo.java`) returns `ste` when it is non-null
without calling the VM, so a frame walked before a redefinition and read
after it keeps its line and file under the lever:
`tools/probes/interp/L3/L3W40WalkedFrameReadAfterRedefinition.java` row
`unread` would print `line=line file=...` there instead of HotSpot's
`unread line=-1 file=null`.

## The idea

* `populate_sfi` leaves `ste` null and records, per frame, what the lazy
  read needs: the entry's class id, its line and file as captured, and the
  class-redefinition count at the walk. The `StackFrameInfo` has no spare
  field; a per-VM side table keyed by the frame object (pruned by the GC's
  reconcile, as `classloader::loader_namespace_id_store` is) or a hidden
  field slot beyond the JDK layout are the two options -- no process global.
* `StackTraceElement.initStackTraceElement(ste, sfi)`
  (`native-builtins/src/lib.rs::native_init_stack_trace_element`) fills the
  element from that record, answering `-1` / no file when the ring
  (`cratonvm_classloading::for_each_redefined_class_between`) names a
  redefinition of the frame's class since the walk, as `p59_frame_settle`
  does. The JDK caches the result in `ste`, so a frame read before the
  redefinition keeps what it read, as HotSpot's does.

## What to check before building

* `native_init_stack_trace_element` today prefers the pre-built `ste` it is
  handed through the frame; a frame built by another path (`LiveStackFrame`)
  must keep its answer.
* The lever was kept OFF because the JDK's batched walk is slower
  interpreted (`reflect_invoke::stackwalker_jdk_walk_enabled`'s table);
  building this is worth it only together with a plan to make that walk the
  default.

## Positive control

The wave-40 one, once the new read prints it too: `CRATONVM_SW_JDK_WALK=1
CRATONVM_DBG_RETRANSFORM=1` on `L3W40WalkedFrameReadAfterRedefinition` should
print one `[redefine] stack frame read after its class's redefinition: no
line` line for the `unread` row (today only the default carrier's
`p59_frame_settle` prints it).
