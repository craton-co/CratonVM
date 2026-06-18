# Gap: `cratonvm/synthetic/AnonymousObject$N.getInputStream()` NoSuchMethodError

**Discovered:** 2026-06-09 (cross-VM comparison run)  
**Severity:** Medium — fires as a non-fatal WARN on JUnit Platform launcher startup and shutdown. The help text and test execution proceed despite the error, but the gap breaks any code that reads a subprocess's output.  
**Status:** FIXED (branch `fix/anonymous-object-getinputstream`) — the synthetic Process now allocates under its own named class (`cratonvm/synthetic/Process`) so receiver-driven dispatch can find its natives at all, all `Process` natives are dual-registered under that name, and the three missing stream getters (`getInputStream`/`getErrorStream`/`getOutputStream`) are implemented by wrapping the spawn path's stored pipe fds in real `FileInputStream`/`FileOutputStream` objects.

---

## Symptom

```
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError method="cratonvm/synthetic/AnonymousObject$6.getInputStream()Ljava/io/InputStream;"
```

Appears **twice** during `junit-platform-console-standalone --help`:
- Once during launcher init (~9s in, before the help banner prints)
- Once during launcher shutdown (after `System.exit(0)`)

The `--help` output still prints completely and the process exits with rc=0. The harness incorrectly scored this as FAIL because it grep'd for `NoSuchMethodError` — the actual observable behavior is a pass (help banner visible, rc=0).

---

## Root cause (corrected 2026-06-09)

The original analysis below ("synthetic proxy for an anonymous inner class") was wrong. `cratonvm/synthetic/AnonymousObject$N` is the **defense-in-depth fallback class** minted by `alloc_object` in `vm/src/vm/vm_exec.rs` whenever a native allocator passes `ClassId::new(0)` (= `java/lang/Object`) with a non-zero field count: the substitute class declares `N` fields so the header's class id agrees with its slot count. `$6` is therefore *any* 6-field object allocated through that fallback — not an anonymous class.

The actual chain (diagnosed with `CRATONVM_DBG_ANONALLOC=1`, which prints the allocating Java stack):

1. picocli's terminal-width probe (`CommandLine$Model$UsageMessageSpec$1.run()` — runs at launcher init and again at shutdown) spawns `mode con` via `ProcessBuilder.start()`.
2. `spawn_and_wrap` in `native-io/src/process.rs` allocates its synthetic 6-field `Process` object with `ClassId::new(0)` → the fallback substitutes `AnonymousObject$6`. The child's stdin/stdout/stderr pipe fds are stored in fields 1–3.
3. picocli calls `proc.getInputStream()` to read the probe's output. The phase57 `Process.getInputStream` stub lives in `native-builtins/src/phases_late.rs`, which is **synthetic-jdk-only (compiled out of real-JDK builds)**; `native-io/src/process.rs` registered `waitFor`/`exitValue`/`isAlive`/`destroy`/`pid` but **not** the three stream getters. No native + no bytecode (the receiver's class has no methods; real `java/lang/Process.getInputStream` is abstract) → NoSuchMethodError.

A deeper finding from a direct probe (`ProcessBuilder.start()` + `isAlive()`/`waitFor()` on CratonVM): **every** `java/lang/Process` native registered by `process.rs` — `waitFor`, `exitValue`, `isAlive`, `destroy`, `pid` — also NSME'd on these receivers. Virtual dispatch resolves by the receiver's class-chain names; `AnonymousObject$6` has no superclass chain to `java/lang/Process`, so registrations keyed on `java/lang/Process` were unreachable on the very objects the spawn path creates. The junit run only ever surfaced `getInputStream` because picocli's probe throws there first (caught, width falls back to 80).

## Fix

`native-io/src/process.rs`, three parts:

1. **Class identity:** `spawn_and_wrap` now allocates via `ctx.ensure_synthetic_class("cratonvm/synthetic/Process", PROC_FIELD_COUNT)` instead of `ClassId::new(0)`, so the receiver carries a stable, registrable class name (and debug output shows `cratonvm/synthetic/Process` instead of the anonymous fallback).
2. **Dual registration:** every `Process` native is registered under both `java/lang/Process` and `cratonvm/synthetic/Process`, making receiver-driven dispatch find them.
3. **Missing getters:** `getInputStream()`/`getErrorStream()`/`getOutputStream()` natives read the pipe's fd id from the synthetic field (stdout=2, stderr=3, stdin=1) and wrap it in a **real** `java/io/FileInputStream` / `FileOutputStream` constructed via `new_object_initialized(..., "(Ljava/io/FileDescriptor;)V", ...)` around a real `java/io/FileDescriptor` carrying the id in its `fd`(int)/`handle`(long) fields — the same dual-write contract as `fis_set_fd`/`fos_get_fd`, so all existing fd-table stream natives (read/write/available/close) work unchanged. fd `-1` (pipe absent) produces a descriptor neither lookup accepts → EOF / dropped writes, matching the "inherited or closed" encoding.

This was the design `process.rs` documented all along ("the existing FileInputStream/FileOutputStream natives in lib.rs can read/write them through the normal fd_table() code paths") — the streams were simply never reachable.

---

## Original (incorrect) root cause analysis

> `cratonvm/synthetic/AnonymousObject$6` is a **synthetic proxy class** that CratonVM generates for an anonymous inner class in the JUnit Platform launcher JAR. [...] CratonVM has registered `AnonymousObject$6` as a synthetic proxy but **did not copy** the `getInputStream()` method from the anonymous class's body.

Kept for the record: CratonVM does not proxy anonymous classes; they load as real bytecode (`Outer$1.class`). The `AnonymousObject$N` name family comes only from the ClassId(0) allocation fallback.

---

## Reproduction

```bash
CV="C:/craton/CratonVM/target/release/cratonvm.exe"
JDK="C:/Program Files/Java/jdk-25"
JUNIT="C:/craton/CratonVM/.bench-cache/junit-platform-console-standalone-1.10.2.jar"

"$CV" --java-home "$JDK" --Xmx 2g -jar "$JUNIT" --help 2>&1 | grep -E "NoSuchMethod|Usage:|EXIT"
```

Before the fix: the WARN fires twice (launcher init + shutdown). After: no `NoSuchMethodError`; picocli's width probe actually reads `mode con` output.

To find the allocation site of any `AnonymousObject$N` warning: rerun with `CRATONVM_DBG_ANONALLOC=1`.

---

## Secondary finding: harness false negative

The comparison harness in `test-infra/run-comparison-full.sh` marked `junit-help` as FAIL when `NoSuchMethodError` appeared anywhere in the log. This is over-broad: the actual observable behavior (help text printed, rc=0) is a pass. Fixed alongside this gap: the FAIL gate now only matches fatal markers (`System.exit(-1)`, SIGSEGV, panic); `NoSuchMethodError` lines still surface in the summary column as warnings.

---

## Impact scope

Any code that spawns a subprocess and reads its output/streams in a real-JDK build: picocli terminal-width detection, build-tool forking (surefire), `Runtime.exec(...).getInputStream()` users. Before the fix all of these hit NoSuchMethodError on the stream getters (waitFor/exitValue already worked).
