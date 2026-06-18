# Round-2 ARRAYLEN agent — investigation of "arraylength on non-array (ArrayList)"

## Repro

    ./target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" \
        --stack-dump-on-timeout 0 \
        -jar .bench-cache/junit-platform-console-standalone-1.10.2.jar --help

Observed:

    [ARRAY-LEN-GUARD] non-array object class=java/util/ArrayList kind=Object
        caller=org/junit/platform/console/shadow/picocli/CommandLine$Model$UsageMessageSpec$1.run()V pc=94
    ...
    java.lang.NullPointerException: Cannot load from null array
        at ... CommandLine$Help$TextTable.putValue(CommandLine.java:17369)
        at java.text.BreakIterator.getBreakInstance(BreakIterator.java:554)
        at java.text.BreakIterator.createBreakInstance(BreakIterator.java:575)
        at sun.util.locale.provider.BreakIteratorProviderImpl.getBreakInstance(...:170)
    [cratonvm] System.exit(-1) called — process terminating

## Conclusion: NOT an interpreter opcode bug. No edit to interpreter.rs.

There are **two independent symptoms**, neither caused by interpreter opcode mishandling:

### 1. The ARRAY-LEN-GUARD warning is a non-fatal red herring (ProcessBuilder native)

`javap -c -p CommandLine$Model$UsageMessageSpec$1` shows `run()` has **no
`arraylength` opcode at all**, and pc=94 is `astore_1` immediately after
`invokevirtual java/lang/ProcessBuilder.start()`. The guard message is emitted
by the `array_length` **trait helper** in `vm/src/vm/vm_exec.rs:1409`, whose
`caller=` field is just `self.thread.frames.last()` — i.e. the topmost JVM frame
while a **native builtin** (the reflective `ProcessBuilder` path used by
picocli's `getTerminalWidth()`) called `array_length` on a `List` returned by
`ProcessBuilder.command()`. The guard `return 0`s and execution continues; it is
NOT what aborts the process. (Owner: native-builtins / ProcessBuilder, not the
interpreter.)

### 2. The fatal NPE is a locale-data gap (BreakIterator), already documented in-tree

`javap -c -p -l sun.util.locale.provider.BreakIteratorProviderImpl`, method
`getBreakInstance(Locale,int,String,String)`:

    14: aload 5                       // LocaleResources
    16: ldc "BreakIteratorClasses"
    18: invokevirtual LocaleResources.getBreakIteratorInfo(String):Object
    21: checkcast [Ljava/lang/String;
    24: astore 6                      // local 6 = null  (lookup returned null)
    ...
    45: aload 6                       // null array
    47: iload_2
    48: aaload                        // -> "Cannot load from null array" (NPE)

The `aaload` at pc=48 throws because `local 6` genuinely **is null**:
`LocaleResources.getBreakIteratorInfo("BreakIteratorClasses")` returned null.
The interpreter's `aaload` handler (interpreter.rs:5731-5758, via
`pop_object_ref_ctx(..., "Cannot load from null array")`) is behaving **exactly
per JVM spec** — NPE on array-load from a null reference. The stack/operand
tracking is correct; the array is legitimately null.

The codebase already documents this exact failure in
`vm/src/vm/vm_exec.rs:8563-8593` (the `BREAKITER` allow-list comment): under
CratonVM's partial locale bootstrap, `jdk.localedata`'s class-based resource
bundles are not surfaced through the jimage path, so
`getBreakIteratorInfo("BreakIteratorClasses")` returns null and the JDK bytecode
NPEs at `BreakIteratorProviderImpl.getBreakInstance`. The intended fix is the
native override registered in
`native-builtins/src/phases_late.rs::register_p66_break_iterator`
(`getLineInstance`/`getWordInstance`/...), gated ON by the `check_override`
allow-list in `vm_exec.rs:8586-8593`.

The trace reaching `BreakIteratorProviderImpl.getBreakInstance:170` shows that
override is **not short-circuiting this call path** in the `--help` scenario
(descriptor/ordering of `check_override`, or the synthetic BreakIterator's
instance methods re-entering real bytecode). That repair lives entirely in
`vm_exec.rs` (allow-list) + `phases_late.rs` (native registration) +
locale-resource data — the **LOCALE agent's** files.

## Decision

Per round-2 STRICT RULES: the root cause is not in `vm/src/runtime/interpreter.rs`
(the `aaload`/`arraylength` handlers are correct), so this agent edited **no
source** and hands the fatal NPE to the LOCALE agent and the (non-fatal)
ProcessBuilder guard to native-builtins. Only this docs note was added.
