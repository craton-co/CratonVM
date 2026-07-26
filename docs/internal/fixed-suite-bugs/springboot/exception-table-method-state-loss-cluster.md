# RETRACTED — all three "VM interpreter/JIT state-loss" instances were stale native-override stubs, not a VM bug

**Status: CLOSED 2026-07-24 — all three instances retracted.** Originally
written up as a suspected high-priority CratonVM interpreter/JIT
correctness gap (methods with an exception table silently losing
correctly-computed state on their normal path), found across three
independent call sites in the Spring Boot core Cluster C (logging
bootstrap) batch. **All three turned out to be ordinary native-override
bugs** — each failing method had an existing native registration in
`native-builtins/src/phases_early.rs` or `phases_late.rs` that either
stubbed the method to a no-op or intercepted it with stale/wrong logic,
fully explaining the observed symptom without any VM-level defect. Kept
as a retrospective, not deleted, because the *investigation shape* that
produced (and then had to walk back) three false positives in a row is
itself worth learning from.

## The lesson

Every instance had the same signature: "every individual sub-operation
works correctly when replicated by hand outside the real method's control
flow, but the real, composed method produces wrong results." That pattern
feels like strong evidence for a VM bug in the *composition* — but it's
equally, and far more cheaply, explained by **something intercepting the
composed method before any of its sub-operations run**. In all three
cases here, that's exactly what was happening: a native override for the
*exact* failing method (not its sub-operations, which is why they tested
fine individually).

**Before attributing a "real bytecode doesn't produce the right result"
symptom to a VM interpreter/JIT bug, grep for an existing native override
on the exact failing method first.** It takes a few minutes and would have
prevented every hour spent on the writeups below.

## Instance 1 — `java.util.logging.SimpleFormatter.format(LogRecord)` — FIXED
Real cause: `SimpleFormatter.<init>`/`format` were natively stubbed in
`phases_early.rs` (the same legacy layer, predating the `logmanager.rs`
rewrite, that also had a matching stale `ConsoleHandler` stub). `<init>`
was a bare no-op; `format` read `LogRecord` raw slot 1 (that layer's OLD
message-slot convention, no longer accurate — the real layout moved to
`get/set_field_by_name(_, "message")`), missed, and fell through to a
**hardcoded `"INFO: {text}\n"` / `"INFO: \n"`** regardless of the record's
actual level or message — exactly the "date and message dropped" symptom
originally attributed to the interpreter. Removed; see
`javaloggingsystemtests-simpleformatter-args-drop.md` (now marked FIXED).
`JavaLoggingSystemTests` went from 11/12 to 12/12.

## Instance 2 — `org.springframework.util.ClassUtils.forName(String, ClassLoader)` — FIXED
Real cause: `ClassUtils.forName`'s native override
(`spring_class_utils_for_name_impl` in `phases_late.rs`) deliberately
ignores the passed `ClassLoader` for anything classified as a "built-in
loader" (comment: "the classLoader arg is deliberately ignored"),
resolving through CratonVM's global unified classpath scanner instead —
correct for the application loader (whose whole job is exactly that), but
wrong for the platform/bootstrap loader, which per the JLS can never see
application classes. Fixed with a narrow check scoped to
`ClassLoaders$PlatformClassLoader`/`$BootClassLoader` specifically (not
the app loader). See
`classutils-forname-platform-loader-false-positive.md` (now marked
FIXED). `LogbackRuntimeHintsTests` went from 3/4 to 4/4.

## Instance 3 — `org.springframework.boot.logging.logback.DefaultLogbackConfiguration.apply(LogbackConfigurator)` — FIXED
Real cause: `apply()` was entirely stubbed to a no-op in `lib.rs`, dating
from when `LoggerContext` was synthetically allocated and NPE'd on
`monitorenter` — a premise that stopped holding once `LoggerContext`
construction moved to real bytecode, but nobody removed the now-stale
stub. `DefaultLogbackConfigurationTests` went from 4/7 to 6/7 (the last
failure was ~~an unrelated Mockito/`java.io.Console` mocking
limitation~~ — **retraction, 2026-07-26: this was also a fixable native
gap, not a real limitation.** `Console`'s `<clinit>` (JDK 25) calls a
`private static native int ttyStatus()` that CratonVM never registered,
so `<clinit>` threw `UnsatisfiedLinkError`; Mockito's
`InlineBytecodeGenerator` triggers class-init before mocking specifically
so it can report a clean `MockitoException` instead of an `NCDFE`, and
that's what surfaced: "Mockito cannot mock this class: class
java.io.Console". Verified against real JDK 25 on the same host/classpath
that real HotSpot passes this test — confirming it was never a genuine
Mockito/Console limitation. Fixed by registering `java/io/Console
.ttyStatus()I` → `0` (mirrors the pre-JDK-25 `istty()Z` → `false`
convention already used for the same class). `DefaultLogbackConfigurationTests`
is now 7/7. See `classutils-forname-platform-loader-false-positive.md`'s
2026-07-26 update.

## What would have ruled all three out immediately

```bash
grep -rn '"format"\|"formatMessage"' native-builtins/src/*.rs   # → SimpleFormatter stub, instance 1
grep -rn '"org/springframework/util/ClassUtils"' native-builtins/src/*.rs   # → forName override, instance 2
grep -rn "DefaultLogbackConfiguration" native-builtins/src/*.rs   # → apply() stub, instance 3
```

No VM-level interpreter/JIT investigation is warranted from this cluster's
findings — there is no remaining evidence of the originally-suspected bug
class.
