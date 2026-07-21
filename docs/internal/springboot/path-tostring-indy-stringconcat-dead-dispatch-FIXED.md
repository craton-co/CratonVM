# `java.nio.file.Path.toString()` dead-dispatched to `Object.toString()` via `invokedynamic` string concatenation — FIXED

**Status: FIXED — 2026-07-19. Fix #1 below (`vm_exec.rs`) silently regressed
off `dev` sometime after this date and was re-fixed 2026-07-21 — see
"Regression" at the bottom.**

## Symptom

`"file:" + somePath` (a `Path` operand in an `invokedynamic`-based Java
string concatenation, `StringConcatFactory.makeConcatWithConstants`)
stringified the `Path` as `java.nio.file.Path@<hash>` (the default
`Object.toString()` format) instead of the real path text, e.g.:

```
14:12:43.265 [main] WARN ...DefaultTemplateResolverConfiguration -- Cannot find template location: file:java.nio.file.Path@6ddc2 (please add some templates...)
```

Hit in both `module/spring-boot-thymeleaf`'s `ThymeleafReactiveAutoConfigurationTests`
and `ThymeleafServletAutoConfigurationTests`, method `templateLocationEmpty(CapturedOutput, Path)`.

## Root cause

**Not actually the `invokedynamic` bootstrap itself.** `"literal:" + aPath`
javac-compiles (JDK 25) to a real `invokestatic
java.lang.String.valueOf(Ljava/lang/Object;)Ljava/lang/String;` call
*ahead of* the `StringConcatFactory` indy call site — the indy bootstrap
only ever receives an already-stringified `String` argument for this shape
(confirmed via `javap -v`: both the `"file:" + p` and `"result:" + s`
concat sites in a two-argument-`println`-style test program shared the
identical `(Ljava/lang/String;)Ljava/lang/String;` dynamic descriptor).
`String.valueOf(Object)`'s own real bytecode (`obj == null ? "null" :
obj.toString()`) is what actually dispatches `Path.toString()`.

That real-bytecode `obj.toString()` call site has `java/lang/Object` as its
constant-pool symbolic reference class (`obj`'s declared type in
`String.valueOf`), not `java/nio/file/Path`. `vm/src/vm/vm_exec.rs`'s
`invoke_on_class_shared_inner` only retargets dispatch onto the RECEIVER's
actual runtime class when the ORIGINAL (CP-symbolic) class at the call site
is itself an interface or abstract class (`this_is_iface_or_abs`) —
`java/lang/Object` is neither, so no retargeting happens, and the
`java/nio/file/Path` force-native entry deep in the `check_override`/native-
lookup chain (keyed on the — unretargeted — `class_name`, which stays
`java/lang/Object`) never gets a chance to fire, even though the actual
receiver is one of CratonVM's synthetic `java/nio/file/Path` value objects.
Real dispatch resolution therefore lands on `java.lang.Object.toString()`
(`getClass().getName() + "@" + hashCode`), producing the garbage default
form.

This is a **third, independent gap** in the same family as
`docs/internal/springboot/path-tostring-dead-dispatch-breaks-inprocess-javac-FIXED.md`
(which fixed the interpreter's own `invokevirtual` force-native gate, plus
one `ctx.invoke_virtual` native-to-native call site in
`native_javac_file_manager_infer_binary_name`) — this one is specifically
about `Object`-typed call sites (`String.valueOf(Object)`,
`StringBuilder.append(Object)`, any user code with an `Object`-declared
local/parameter) whose *receiver* happens to be a synthetic Path value.

## Fix

Two complementary fixes, both in worktree `fix/thymeleaf-groovy-residuals-20260719`:

1. **`vm/src/vm/vm_exec.rs`** (`invoke_on_class_shared_inner`): added a
   standalone, receiver-aware `toString()` check — independent of the
   resolved/retargeted `class_name` — right after `class_name` is computed,
   mirroring the existing `VirtualMachine.attach`/`SSLServerSocket.accept()`
   special cases already in this function. If the ACTUAL RECEIVER
   (`args[0]`)'s class `is_subclass_of` `java/nio/file/Path` (walking both
   the superclass chain AND implemented interfaces via the `ClassId`-based
   `is_subclass_of`, NOT the exception-`catch_type` `is_subclass_of_by_name`
   fallback, which deliberately skips interfaces), dispatch the registered
   `java/nio/file/Path.toString()` native directly.
2. **`vm/src/runtime/invokedynamic.rs`** (`value_to_string`, used by
   `StringConcatFactory.makeConcatWithConstants`'s own argument
   stringification): a defensive companion fix for the case where a `Path`
   value IS passed directly into the indy bootstrap (no `String.valueOf`
   pre-conversion — e.g. a recipe with 2+ dynamic arguments, or a future
   javac version that stops pre-converting). Same receiver-class check,
   routing through `cratonvm_native_builtins::phases_late::p57_path_display_string`
   directly (made `pub`, was `pub(crate)`) instead of `ctx.invoke_virtual`.

Both fixes needed the SAME correction mid-implementation: the first attempt
used `ClassManager::is_subclass_of_by_name`, which — being the
exception-handler `catch_type` fallback — deliberately walks only the
superclass chain (`catch_type`s are always classes, never interfaces per
JVMS §4.7.3/§6.5.athrow) and so can never match an interface like `Path`.
Switched to the `ClassId`-based `is_subclass_of` (resolving `Path`'s
`ClassId` via `class_id_by_name`/`find_class_by_name` first), which walks
interfaces correctly.

## Verification

Binary: `cratonvm-thymeleaf-residuals-20260719.exe`, worktree
`fix/thymeleaf-groovy-residuals-20260719` (`dev` @ `c41220849`).

- Standalone repro (`"file:" + Paths.get("some","dir","here")`): CratonVM
  output now byte-identical to HotSpot (`file:some\dir\here` on Windows).
- `ThymeleafReactiveAutoConfigurationTests`: **21/21** (up from 20/21 —
  `templateLocationEmpty` now passes).

## Affected classes (now passing)

- `module/spring-boot-thymeleaf` | `ThymeleafReactiveAutoConfigurationTests` | `templateLocationEmpty(CapturedOutput, Path)`
- `module/spring-boot-thymeleaf` | `ThymeleafServletAutoConfigurationTests` | `templateLocationEmpty(CapturedOutput, Path)` (same fix; not independently re-run because the class also contains the still-open `createLayoutFromConfigClass` hang — see
  `thymeleaf-groovy-layoutdialect-metaclass-introspection-hang.md`)

## Regression (found + re-fixed 2026-07-21)

Because `ThymeleafServletAutoConfigurationTests`'s copy of
`templateLocationEmpty` was never independently re-run (blocked by the
`createLayoutFromConfigClass` hang, per the note above), a later silent
regression of fix #1 (the `vm_exec.rs` `invoke_on_class_shared_inner` hunk)
had no test coverage to catch it. Once the hang was fully closed
(2026-07-21, see
`../../internal/springboot/thymeleaf-groovy-layoutdialect-metaclass-introspection-hang-FIXED.md`)
and the full class ran for the first time ever, `templateLocationEmpty`
failed with the exact original symptom
(`file:java.nio.file.Path@<hash>`). `git log -S "let is_path = {"` showed
the hunk was added exactly once (this doc's fix, `a6ce01fe2`) and never
explicitly removed by any single commit — it was evidently dropped silently
during a merge conflict resolution somewhere in `dev`'s history between
2026-07-19 and 2026-07-21. Re-added verbatim; verified
`ThymeleafServletAutoConfigurationTests` 27/27 including this method, and
`ThymeleafReactiveAutoConfigurationTests` 21/21 unchanged. The two sibling
fixes (#2 in this doc, `invokedynamic.rs`'s `value_to_string`; and the
separate `interpreter.rs` `force_native_over_real_jdk_bytecode`-style gate)
were confirmed still present and untouched by the regression.
