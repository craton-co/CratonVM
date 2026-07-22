# `java.lang.ThreadGroup` native accessors used a stale field-index layout — FIXED

**Status:** FIXED. **Found/fixed:** 2026-07-09, while investigating the
Tomcat WebSocket close-delay bug (see
`../../known-issues/tomcat/wsremoteendpoint-close-delay-near-deadlock.md`).
Commit: see `git log --oneline -- native-builtins/src/phases_late.rs` for
`register_p71_thread_extras` on `dev`.

## Summary

`native-builtins/src/phases_late.rs::register_p71_thread_extras` registers
native overrides for `java.lang.ThreadGroup`'s `<init>` (1-arg and 2-arg),
`getName`, `getParent`, `isDaemon`, `setDaemon`, `toString`,
`getMaxPriority`, `setMaxPriority`, and `list`. These are registered
unconditionally (called from `register_essential_natives`, so they apply in
**real-JDK mode too** — the comment at the call site: *"ThreadGroup is
normally late-phase, but process-controller bootstrap builds a
JBossThreadFactory before those late registrations are enough"*), and they
win over the real bytecode for this bootstrap-critical class.

The accessors used a **hardcoded, stale synthetic field layout**:
`name=0, parent=1, daemon=2, maxPriority=3` (a comment at the top of the
block said as much). The real JDK 25 `java.lang.ThreadGroup` class file
layout (confirmed via `javap`) is actually:
`parent=0, name=1, maxPriority=2, daemon=3, ngroups=4, groups=5, nweaks=6,
weaks=7`. **`parent` and `name` are swapped** relative to what these
natives assumed (and `daemon`/`maxPriority` are also at different slots).

Since these natives are the *only* thing that ever writes/reads a
`ThreadGroup`'s fields (they win over real bytecode), any `ThreadGroup`
built through them — including the VM's own hand-rolled "system"/"main"
bootstrap groups (`vm/src/vm/vm_exec.rs`, which construct the initial
groups via `invoke_on_class_shared(..., tg_class, "<init>", ...)`) — ended
up with its `name` String stored where `parent` belongs and vice versa.

`jdk.internal.misc.InnocuousThread.<clinit>` (real bytecode, used by
`java.lang.ref.Cleaner`'s dedicated cleanup thread — `Cleaner.create()`) has
to find the root thread group without going through `getParent()` (to avoid
security-manager entanglement); it does this via
`Unsafe.objectFieldOffset(ThreadGroup.class, "parent")` +
`Unsafe.getReference(group, offset)` + `checkcast ThreadGroup`, in a loop
walking to the root. `Unsafe.objectFieldOffset` (via `declared_fields()`,
which reads the *real* class metadata, unaffected by this bug) correctly
reports `parent` at index 0 — but that slot, on any ThreadGroup built by
these natives, actually held the *name* String. The `checkcast` then threw:

```
java.lang.Error: java.lang.ClassCastException: java.lang.String cannot be cast to java.lang.ThreadGroup
	at jdk.internal.misc.InnocuousThread.<clinit>(InnocuousThread.java:177)
	at jdk.internal.ref.CleanerFactory$1.newThread(CleanerFactory.java:43)
	at jdk.internal.ref.CleanerImpl.start(CleanerImpl.java:110)
	at java.lang.ref.Cleaner.create(Cleaner.java:200)
	at jdk.internal.ref.CleanerFactory.<clinit>(CleanerFactory.java:40)
```

100% reproducible any time `Cleaner`/`InnocuousThread` gets triggered under
real-JDK mode (very common — DirectByteBuffer, FileChannel, HTTP clients,
`java.lang.ProcessHandle`, etc. all use `Cleaner`). In the Tomcat repro this
fired during `StandardServer` boot (`LifecycleException: Failed to
initialize component [StandardServer[-1]]`), before any application/webapp
code ran, blocking the websocket-close-delay investigation entirely.

## Fix

Rewrote all the `ThreadGroup` native accessors in
`register_p71_thread_extras` to use `ctx.get_field_by_name`/
`ctx.set_field_by_name("parent"|"name"|"daemon"|"maxPriority", ...)`
instead of hardcoded numeric indices. This makes them self-consistent with
the real field layout (and with `Unsafe.objectFieldOffset`/
`java.lang.reflect.Field`/`declared_fields()`, all of which already agreed
with each other — only these natives disagreed). Also dropped the
`object_num_fields(this) > 3` guards, which are unnecessary once the
lookup is by name (a missing field degrades gracefully via
`get_field_by_name`'s existing default-value behavior instead of needing a
manual length check).

Did **not** change the pre-existing semantic gap where the 1-arg
`ThreadGroup(String)` native sets `parent = null` unconditionally (real JDK
semantics would inherit `Thread.currentThread().getThreadGroup()`) — that
is a separate, narrower issue not implicated in the bug being fixed here,
left as-is to keep this change minimal.

## Verification

- Standalone repro (`OffsetProbe.java`/`ReflProbe.java`): before the fix,
  `Unsafe.objectFieldOffset`/reflection all reported `parent` holding the
  `name` String and `name` holding `null`; after the fix, both correctly
  report `parent=null` (no parent group; 1-arg ctor semantics unchanged)
  and `name="probeGroup"`, with `getParent()`/`getName()` bytecode agreeing.
- `CleanerProbe.java` (`Cleaner.create()` + register + explicit `clean()`):
  before the fix, crashed immediately with the `ClassCastException`/`Error`
  above; after the fix, runs clean with no exception.
- `ThreadGroupSanity.java` (main group → child group → worker thread,
  `getName`/`getParent`/`activeCount`): all assertions pass, hierarchy
  (`main`'s parent is `system`, per the existing bootstrap design in
  `vm_exec.rs`) is intact.
- `cargo test --release -p cratonvm-native-builtins --lib` (full crate,
  2938 tests): 2937 passed, 1 failed — the 1 failure
  (`security_manager::policy::tests::wp68_substitution_dollar_escape_preserves_literal`)
  is unrelated (a `$USER`-substitution policy-file test, pre-existing,
  nothing to do with `File`/`ThreadGroup`) and reproduces identically on
  unmodified `dev`.
- Full Tomcat `TestWsRemoteEndpointImplServerDeadlock` repro: after this
  fix (stacked on the `File.FS` fix), `StandardServer` init no longer
  throws the `ClassCastException`/`Error`; Tomcat gets substantially
  further into `StandardContext`/`WsSci` bootstrap before hitting the next
  (separate, still-open) blocker — see
  `docs/known-issues/enumset-of-broken-for-non-jdk-enums.md`.
