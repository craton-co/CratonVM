# Proposal: in `--jdk-only`, `Method.invoke` and `Constructor.newInstance` run the JDK's own code

**Status: proposal — filed 2026-10-09 by interpreter round i1 wave 45, lane
L3. Not implemented.**

## The gap it closes

`Method.invoke` and `Constructor.newInstance` are served by `Bridge` natives
in both modes (`lang_reflect::native_method_invoke_boxed`,
`lang_class::native_constructor_new_instance`; the interpreter's cached-invoke
door keeps them native, `dispatch_virtual::native_override_for_cached_reflect_invoke`),
although the JDK's class bytes carry real bodies. Every JDK frame HotSpot
shows for a reflective call has therefore been SYNTHESIZED, one view at a
time, over four waves:

* wave 43: a thread's own `Throwable`, `Thread.getStackTrace` and the stack
  walks (`stackwalker::reflective_splices`, the per-kind step tables read
  from JDK 25's bytes);
* wave 44: the same under compiled targets (`ActiveCompiledFrame::chain_index`);
* wave 45: the published trace other threads read, and the
  `InvocationTargetException` the native raises (`reflective_raise_entries`,
  which picks the accessor's `catch` arm by the class of what was thrown);
* still open (`docs/known-issues/interpreter/i43-L3-a-throwable-a-reflective-native-raises-lists-no-reflection-frames-20261007.md`):
  the argument checks' exceptions (each needs its own row of HotSpot's
  frames: `checkArgumentCount:324`, `checkReceiver:199`, `invoke:108`,
  `Method.invoke:557`), JVMTI / JDWP frame lists, JDK 17's accessors, and the
  hidden method-handle frames HotSpot's `getAllStackTraces` lists.

Each is a table of what JDK 25's code would have done, kept in step with it by
hand. `AGENTS.md` says real class bytes are authoritative over a registered
`Bridge`; these two are the most visible exception left.

## The proposal

Behind a switch, `--jdk-only` only (`--compatible` byte-for-byte unchanged),
let the two methods run their bytecode, in two stages:

1. **Stage 1: the JDK's native accessor.** Start the VM with the JDK's own
   `jdk.reflect.useNativeAccessorOnly` behaviour for the accessor choice
   (`ReflectionFactory`), so `Method.invoke` runs its bytecode (access check,
   `acquireMethodAccessor`, `DirectMethodHandleAccessor.NativeAccessor`) and
   only `NativeAccessor.invoke0` / `DirectConstructorHandleAccessor.NativeAccessor.newInstance0`
   is a VM native (HotSpot's `JVM_InvokeMethod` / `JVM_NewInstanceFromConstructor`):
   the argument checks, the unboxing and the `InvocationTargetException`
   wrapping are the JDK's, and the frames exist. Measure against HotSpot run
   with `-Djdk.reflect.useNativeAccessorOnly=true`, which lists (measured,
   JDK 25.0.3, `L3W43ReflectionFrames` row `throw-through-invoke-text`)
   `DirectMethodHandleAccessor$NativeAccessor.invoke0(Native Method)`,
   `DirectMethodHandleAccessor$NativeAccessor.invoke(DirectMethodHandleAccessor.java:227)`,
   `Method.invoke(Method.java:565)` -- a different trace from HotSpot's
   default, so stage 1 alone trades one divergence for another and is a
   stepping stone.
2. **Stage 2: the method-handle accessor.** Let `MethodHandleAccessorFactory`
   build `DirectMethodHandleAccessor`s over direct method handles, as
   HotSpot's default does. The frames and lines are then HotSpot's by
   construction, including the checks'. This depends on lane L4's
   method-handle invocation being complete and fast enough for reflection's
   call rate (JUnit, Spring, Jackson); the interpreter-side cost of a
   `LambdaForm` chain per reflective call is the measurement that decides it.

Retired when stage 2 is on: `JvmThread::reflective_calls`,
`reflective_frames_named`, `reflective_accessors_loaded`, the step tables,
the splice and raise code in `stackwalker`, the two natives' `Bridge`
registrations, and the cached-invoke override (a `docs/jdk-only/` record for
each retired bridge, and the stub ratchet rows if any move).

## Risks

* Caller sensitivity: `Method.invoke` is `@CallerSensitive`; its bytecode
  calls `Reflection.getCallerClass()`, which must skip the frames HotSpot
  skips (`@CallerSensitive` / reflection frames) -- lane L5's and L4's area.
* Boot order: the JDK itself calls `Method.invoke` early (before
  `java.lang.invoke` is usable); HotSpot's `ReflectionFactory` falls back to
  the native accessor until `VM.isModuleSystemInited()`. The stage-1 switch
  must keep that fallback.
* Speed: the native is one Rust call; stage 1 adds a few interpreted frames
  per call, stage 2 a method-handle chain. Bench `ByNameCallBench`-style
  reflective rows (`--nojit` and JIT, fat LTO, interleaved) before any
  default flips.

## How to verify

`L3W43ReflectionFrames`, `L3W44ReflectionCompiledTarget`,
`L3W45ReflectiveRaise` and `L3W45PublishedReflectiveTrace` must print
HotSpot's lines with the synthesizing code switched off; a positive control
is `STTRACE_DBG_REFLECT` printing nothing while the rows still match.
