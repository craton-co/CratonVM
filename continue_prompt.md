# CratonVM — Continuation: actionable gaps left here

Context: this session ran a genuine cross-VM comparison (CratonVM CPU/GPU vs HotSpot
vs TornadoVM) and an orchestrated fix pass on the JIT-on triage. Already committed to
`dev` (rounds 1–2 + diagnostics): BufferedWriter `write([BII)V` dispatch, JIT→JIT
dispatch native-stack guard (catchable SOE), JIT hot-method promotion, the round-7
JIT-on heap-corruption **leak self-heal** (`prune_returned_jit_entries`), BreakIterator
real-JDK NPE, inline-alloc header ordering, and the `CRATONVM_DBG_FULLSTACK_SCAN`
diagnostic. See memory: `reference_jit_junitcore_corruption` (rounds 7–10),
`reference_cross_vm_comparison_harness`, `reference_junit5_console_launcher`.

## Out of scope here (a separate effort owns these — do NOT touch)
- **bintrees18** non-moving-sweep corruption → root-caused to a **JIT missing-safepoint-spill**
  (live object ref held across an allocating call without spilling to a GC-visible slot;
  decisively NOT a stack/callee-saved-register miss — full-stack scan + reg capture both
  failed; `FORCE_MOVING` → 0). A JIT-codegen fix is in progress.
- **BC-EC alloc-putfield / operand-slot-reuse miscompile** and the `org/bouncycastle/`
  JIT ban in `vm/src/jit/skip_list.rs` (blocks math-ec, pqc). In progress.
- The broad in-flight `native-builtins` refactor.

---

## A. JUnit5 jupiter discovery finds 0 tests — HIGH VALUE (unblocks every JUnit5 suite, incl. Commons Math)
**State (corrected):** with a CORRECT *absolute* classpath, CratonVM runs the JUnit5
console launcher to **rc=0 with no crashes** — BufferedWriter, BreakIterator, and the
`ParameterProvider$2.add` NSME are all gone. ServiceLoader finds all 3 engines; method
annotation reflection (`Method.getAnnotations()` → `[Test]`) works. (Earlier
"crashes/blocked" reports were partly a relative-classpath / wrong-cwd repro artifact.)

**The gap:** a programmatic launch that bypasses picocli entirely —
`LauncherFactory.create().execute(request(selectClass("org.apache.commons.math4.transform.TransformUtilsTest")))`
— finds **FOUND=0 on CratonVM vs 4 on HotSpot, silently** (rc=0, no warning/exception).
So the jupiter engine's `isTestMethod`/`isTestClass` predicate returns false on CratonVM.

**Confound to avoid:** an ad-hoc flat classpath can pull TWO `org.junit.jupiter.api.Test`
classes (standalone jar + a separate junit-jupiter-api jar), so `getAnnotation(Test.class)`
and `AnnotationSupport.findAnnotatedMethods(Test.class)` read 0 on BOTH VMs — a probe
artifact, not a CratonVM signal. Ruled out as causes (verified equal to HotSpot):
ServiceLoader, `getAnnotations()`, and `Class.getModifiers()` (see B).

**Next steps:**
1. Build a **single-jupiter-api** classpath (exactly one `Test.class` on the path), so the
   reflection probes become meaningful.
2. Trace jupiter's actual discovery on CratonVM: instrument / step through
   `org.junit.platform.commons.util.ReflectionUtils.findMethods(testClass, predicate, TOP_DOWN)`
   and `AnnotationUtils.findAnnotation(method, Test.class)`. Likely suspects:
   - method enumeration **order/dedup** in `ReflectionUtils.findMethods` (it merges
     declared + inherited and de-dups by signature),
   - annotation **type-identity** comparison (`annotationType() == Test.class`),
   - the **meta-annotation** walk (`@Test` is meta-annotated `@Testable`).
3. Repro probes (programmatic, bypass picocli): `JUnitProbe` (LauncherFactory FOUND/
   SUCCEEDED), `EngineProbe` (ServiceLoader<TestEngine>), `AnnProbe3`
   (modifiers + `AnnotationSupport.findAnnotatedMethods`). Compile against the standalone
   jar; run `-cp "bench;<abs cp>"`. Abs-cp recipe in `reference_junit5_console_launcher`.

## B. `Class.getModifiers()` ACC_SUPER leak — FIX DONE + VERIFIED, ensure it lands cleanly
Verified: `TransformUtilsTest.getModifiers()` returns `0x1` after the fix (was `0x21`),
matching HotSpot. JVMS §4.1: `getModifiers()` must NOT report `ACC_SUPER (0x0020)`. This
breaks any consumer doing `cls.getModifiers() == Modifier.PUBLIC`. It is NOT the cause of
gap A (discovery still 0 after it), but it is a real spec fix.

The fix (in `native-builtins/src/lang_class.rs`, `native_class_get_modifiers`, just before
the final return) — re-apply this hunk if it isn't already in the tree:
```rust
// ACC_SUPER (0x0020) is a VM-internal class flag that Class.getModifiers() MUST NOT
// report (HotSpot strips it; JVMS §4.1). Mask it out of the final value.
let effective_flags = (effective_flags as i32) & !0x0020;
Ok(Some(Value::Int(effective_flags)))
```

## C. picocli `arraylength`-on-`ArrayList` (non-fatal / cosmetic) — NARROWED, not yet pinned
A native calls the `array_length` helper (`vm/src/vm/vm_exec.rs:1409`) on a
`java.util.ArrayList` during picocli's `getTerminalWidth()` (caller frame
`...picocli.CommandLine$Model$UsageMessageSpec$1.run()V pc=94`, the `astore_1` right after
`ProcessBuilder.start()`); the guard returns 0 → terminal width 0 → empty console output
(XML reports still write). RULED OUT this session: it is NOT either ProcessBuilder.start
native — `native_pb_start` (lang_system.rs) is dead (`PB-START-OLD` never prints) and was
hardened anyway (now reads the command List by field name, only `array_length`s a genuine
array); the active `phases_late.rs::register_phase57_process` `start` (`PB-START-ENTRY`)
isn't even called here AND already handles Lists by name. So the `array_length` comes from
some OTHER native on the `getTerminalWidth` path (the reflective
`redirectError`/`Class.forName`/`Method.invoke` dance, or a real-bytecode
`ProcessImpl`/env path).
**Blocker to identifying it:** the guard prints a Rust backtrace to name the source native,
but it is unsymbolizable garbage (a recursive `core::net::socket_addr::impl$6::fmt` — the
`Backtrace::force_capture` symbolizer is itself broken). FIX THAT first (or add a
`#[track_caller]` / explicit native-name tag at the `array_length` helper), then the
offending native is obvious. Cosmetic; gap A is the real Commons-Math blocker.

## D. Bouncy Castle suite — separate real fails from harness artifacts
- `crypto-prng-regression`: now **PASSES** (rc=0, ~59s).
- `util-encoders`: harness shows FAIL but it **passes when run cleanly** ("OK (15 tests)").
  Root cause is the `bc-suite-3way.sh` classpath: MSYS mangles the `;`-separated `-cp`
  so `junit.textui.TestRunner` isn't found (fails identically on HotSpot). Fix the harness
  (use an `@argfile` or proper quoting), then re-measure.
- `pqc-crypto-regression`: was TIMEOUT, now **completes-then-fails**. Capture the actual
  failure on a clean run (real BC assertion vs a wrong intrinsic?).
- `crypto-regression`: OOMs at `-Xmx1g` on HotSpot too — not a fair test at 1g. Bump to
  4g or mark inconclusive.

## E. Comparison-harness render polish (`test-infra/run-vm-comparison.sh`)
- `junit-help`: render mislabels HotSpot/Tornado as FAIL (they print `Usage: junit`, rc=0).
  Fix the `good` detection regex.
- `dacapo-avrora`: DaCapo 9.12-on-JDK25 stderr-digest artifact (expected digest =
  SHA-1 of the empty string; JDK 25 writes to stderr) — fails on HotSpot too. Mark as a
  known non-signal / drop from the suite.
- `commons-math` cratonvm: have the harness run the programmatic `LauncherFactory` path
  (or annotate "JUnit5 discovery = 0 tests, see gap A") instead of the picocli launcher.

## F. Benchmark performance (CratonVM-CPU vs HotSpot) — perf, not correctness
All micro-benchmark checksums are identical across the 3 VMs (correctness perfect).
CratonVM-CPU is ~1.5–2× HotSpot on scalar code (arith ~1.8×, matrix600 ~2× worst,
fib ~tie). Profile `matrix600` / `arith` hot loops if throughput is a goal.

---

## Repro essentials
- VM: `target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25"`.
  HotSpot ref: `C:/Program Files/Java/jdk-25/bin/java.exe`. Tornado JDK:
  `C:/craton/tornadovm/jdk-25.0.3/bin/java.exe`. Maven: `C:/tools/apache-maven-3.9.15`.
- Comparison harness: `bash test-infra/run-vm-comparison.sh` (env `SECTIONS`, `VARIANTS`,
  `BENCHES`, `TIMEOUT`, `HEAP`). CPU-only: `VARIANTS="cratonvm-cpu hotspot tornadovm"`
  for `bench`/`extras`; `"cratonvm hotspot tornadovm"` for `bc`.
- JUnit5 abs cp + probe recipes: memory `reference_junit5_console_launcher`.
- Useful gates: `CRATONVM_DBG_NSME=1` (NoSuchMethodError dispatch dump),
  `--nojit`, `CRATONVM_DBG_FORCE_MOVING=1`, `CRATONVM_DBG_GC_STRESS=<bytes>`,
  `CRATONVM_DBG_CORRUPT_FRAMES=1`, `CRATONVM_DBG_FULLSTACK_SCAN=1`,
  `CRATONVM_JIT_ALLOW_PACKAGES=org/bouncycastle/`.
