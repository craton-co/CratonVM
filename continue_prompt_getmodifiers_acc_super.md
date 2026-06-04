# Bug: Class.getModifiers() leaks ACC_SUPER (0x0020)

Small, self-contained JVMS spec-compliance fix. **Already implemented + verified this
session**, but the change currently sits in the working tree entangled with a concurrent
agent's broad `native-builtins` edits — this file is so it can be re-applied / verified to
have landed cleanly.

## Symptom
`TransformUtilsTest.getModifiers()` returned `0x21` (PUBLIC | ACC_SUPER) on CratonVM vs
`0x1` on HotSpot. JVMS §4.1 / `JVM_GetClassModifiers`: `Class.getModifiers()` MUST NOT
report the VM-internal `ACC_SUPER (0x0020)` flag (javac sets it on virtually every class).
Breaks any consumer doing `cls.getModifiers() == Modifier.PUBLIC`.

## Fix (verified: now returns 0x1, matches HotSpot)
In `native-builtins/src/lang_class.rs`, fn `native_class_get_modifiers`, just before the
final `Ok(Some(Value::Int(...)))`:
```rust
// ACC_SUPER (0x0020) is a VM-internal class flag that Class.getModifiers() MUST NOT
// report (HotSpot strips it; JVMS §4.1). Mask it out of the final value.
let effective_flags = (effective_flags as i32) & !0x0020;
Ok(Some(Value::Int(effective_flags)))
```

## Verify
Compile a probe that prints `Integer.toHexString(SomeClass.class.getModifiers())` for a
plain `public class`; expect `1` (not `21`). Or `AnnProbe3` (see
`continue_prompt_junit5_discovery.md`) — its `modifiers=0x1` line.

NOTE: this is NOT the cause of the JUnit5 0-tests discovery gap (verified: discovery still
0 after the fix). It is an independent correctness fix.

## Repro essentials
- VM: `target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25"`.
- Memory: `reference_junit5_console_launcher` (records this fix + verification).
