# Proposal: scope a class-filtered method event request to the classes it matches

**Status: proposal — filed 2026-10-03 by interpreter round i1 wave 39, lane
L1, while holding the interpreter-only withdrawal across a stepping session
(`docs/internal/fixed-bugs/interpreter-L1-each-jdwp-step-withdraws-every-compiled-body-FIXED-20261003.md`).
Not implemented.**

## Problem

An IDE's "method breakpoint" is a JDWP `MethodEntry` (and often
`MethodExit`) request with a `ClassMatch` or `ClassOnly` modifier naming the
method's class; the IDE filters the method itself on its side. In this VM
such a request concerns EVERY method (`debug::publish_debugger_gates`:
`all_methods` includes `method_entries || method_exits`), so while it is in
force:

* every method of the program runs interpreted (the doors answer
  `DebuggerGates::requires_interpreter` true for all), and
* its first arm withdraws every compiled body of the VM
  (`jvmti_events::note_every_method_needs_the_interpreter`, `source=jdwp`),
  and the hot set is recompiled when the request goes.

IDEs warn that method breakpoints are slow on HotSpot as well (not measured
here: a timing probe against HotSpot is part of the verification below).
This VM's compile doors and its withdrawal can already be scoped by class
(the wave-38 breakpoint path and the wave-39 compile-door check), so a
class-filtered request need not cost the whole program its compiled code.

## Design

1. `publish_debugger_gates` splits method event requests in two: those
   whose modifiers restrict them to a set of classes it can name
   (`ClassOnly`, or a `ClassMatch` pattern without a leading `*`, resolved
   against the session's class table, `DebugState::class_signatures`), and
   the rest. Only the rest set `all_methods` and the `WITHDRAWAL_BY_JDWP`
   bit.
2. The class-scoped ones publish a per-class set beside the breakpoint set
   (`DebuggerGates::method_event_classes`, one bit mask plus an exact set,
   as `breakpoint_classes` / `breakpoint_methods`). `concerns_method` answers
   true for every method of such a class; the frame push's method-entry
   pre-filter and the native funnel's gate are unchanged (they already fire
   only where the request's filters match).
3. On the edge where a class gains such a request, its dependents are
   withdrawn with the breakpoint's scoped path
   (`note_breakpoint_classes_gained`, `JitRealm::withdraw_class_for_the_interpreter`),
   and the wave-39 compile-door check (`breakpoint_bars_compiling`) asks the
   same set, so no compile splices or binds a method of the class while the
   request stays. A class the JIT expands without a record (a JDK class)
   takes the whole-cache path, as a breakpoint does.
4. A class prepared after the request (a `ClassMatch` pattern) joins the set
   at its `ClassPrepare` (`class_prepared_on_thread`), before any of its
   methods can be compiled.

## Cost

Nothing without such a request. With one, the program's other classes stay
compiled, which is the point. The per-method answer for a class holding
neither a breakpoint nor a method event request stays one load.

## Verification

A JDI probe: a warmed program with two hot classes; a `MethodEntryRequest`
with `addClassFilter` on one; HotSpot's transcript (every entry of that
class's methods reported, in order) must match in all four modes, and
`CRATONVM_DBG_JITC=1` must show one `breakpoint withdrawal: class=<the
class> scoped=true` line and no `interpreter-only withdrawal:` line. A
timing probe (`*Bench*`) of the other class's loop before and during the
request shows it staying compiled.
