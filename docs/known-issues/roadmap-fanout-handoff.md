# Roadmap fan-out — handoff & known issues

Status of the `docs/feature-designs/PARALLEL-HANDOFF.md` roadmap fan-out driven
in this session. Everything below is **on `dev`** and compiles (`cargo check`
green). New behavior is **gated default-OFF** unless noted, so the default code
path is unchanged.

## What landed

| Item | Increments on `dev` | Gate (default) |
|------|---------------------|----------------|
| `jep358-helpful-npe` | extended NPE invoke msg + bci analysis; all null-deref opcodes + LVT names; real `-XX:±ShowCodeDetailsInExceptionMessages` flag | `CRATONVM_HELPFUL_NPE_OPCODES` / `-XX` flag (off) |
| `proxy-real-classfile` | real `$ProxyN` canonical; synthetic `<init>` ctor + Object-method routing; stop-the-silent-degrade (strict) | `CRATONVM_REAL_PROXY` (**on**); `CRATONVM_REAL_PROXY_STRICT` (off) |
| `embedding-api` | Layer 1 JNI Invocation API (`libcratonvm`); Layer 2 flat C API; string read-back | n/a (new crate, additive) |
| `real-cdi-bean-container` | interface static-final init verified + shim retired; real `DefaultApplicationStartup.<clinit>` | `CRATONVM_REAL_SPRING_STARTUP` (off) |
| `wire-tiered-manager` | recommended-tier wired + bg compile thread; off-thread `compile_fn` + C1; **GC-STW safety fix** | `CRATONVM_BG_COMPILE` (off) |
| `activate-ir-optimizer` | DSE + widened escape analysis; SCEV LICM + write-only DSE; **full loop unrolling** | `CRATONVM_JIT_LICM` (off); `CRATONVM_JIT_UNROLL` (off) |
| `keystore-mldsa-mlkem` | ML-DSA Signature routing (done by a concurrent session) | n/a |

Validated live against real HotSpot JDK 25 (see *Live-run tooling* below):
items 1, 4 (nested-`<clinit>`), 5 (bg-compile, no deadlock), 6 (LICM/DSE +
unroll) all produce checksums identical to HotSpot with their gate on.

## Known issues / important findings

### 1. The JIT loop-opt passes only recognise `Op::Region`, but real javac loops use `Op::Merge` headers — so LICM does NOT fire on real loops
`loop_regions` / `loop_body` (`jit/src/ir_optimize.rs`) match `Op::Region`.
The bytecode→IR builder, however, encodes a javac loop header as an **`Op::Merge`
with a back-edge**, and wraps the exit `If`'s projections in **single-input
`Op::Merge`** pass-throughs. Consequence:
- **`CRATONVM_JIT_LICM` never fires on real javac loops** (it finds zero
  `Op::Region` headers). Its unit tests pass only because they hand-build
  `Op::Region` loops.
- **Loop unrolling (`CRATONVM_JIT_UNROLL`) was adapted** to the real shape: it
  runs `collapse_trivial_merges` then accepts a `Region` *or* back-edge `Merge`
  header (entry vs back-edge identified by control-reachability,
  `forward_control_closure`). This is the working reference for the fix.

**Fix**: apply the same Merge-header handling (`collapse_trivial_merges` +
reachability-based back-edge ID) to `loop_regions`/`loop_body` so LICM (and any
future loop pass) sees real loops. Until then, treat LICM as a no-op on
production code.

### 2. `CRATONVM_BG_COMPILE` worker can run class loading / `<clinit>` off an unregistered, GC-neutral thread
The background JIT compile worker is a GC-neutral daemon (unregistered, holds no
managed `ObjectRef`, never polls a safepoint — mirrors `G1-MarkComplete`), and
the increment-3 fix bounds its VM-lock scopes so it never holds
`class_manager`/`jit_cache`/`flight_recorder` across a blocking wait (so a
mutator wanting `class_manager.write()` can always reach its safepoint and STW
can complete). **Residual**: `try_jit_compile_callee_slow` →
`resolve_field_ref` → `load_class_concurrent` can take `class_manager.write()`,
run a class `<clinit>` (arbitrary bytecode) and allocate (GC points) **on the
worker**. This is safe today only because the feature is gated OFF; before any
default flip, the worker must become a safepoint participant for the
class-loading path, or class loading must be excluded from off-thread compiles.

### 3. Items deferred — need live suites not runnable in this session
- **`real-cdi-bean-container`**: flipping `CRATONVM_REAL_SPRING_STARTUP` to
  default and deleting the startup-metrics natives requires the **Spring Boot
  battery** to prove the real `<clinit>` path holds end-to-end. The next shared
  enabler (generated-class load/execute for Quarkus ArC) needs a minimal repro.
- **`wire-tiered-manager` C1/C2 split**: `try_compile` is tier-agnostic (picks
  IR-vs-single-pass by method shape + env flags). A real split must thread the
  recommended tier through `try_jit_compile_callee` → `try_compile` and is
  perf-sensitive — needs a JIT benchmark to validate, not just `cargo check`.
- **`activate-ir-optimizer` follow-ups**: partial unrolling (unroll-by-factor +
  remainder loop) for large/non-constant trips; unrolling loops with internal
  branches (requires cloning control, not just data); and fixing issue #1 so
  LICM fires.

## Live-run tooling (left in place)
- **Binary**: `C:\craton\cratonvm-rm-run.exe` — debug build of `cratonvm-cli`
  from the `rm/iropt-unroll` worktree (uniquely named so it does not clash with
  the main checkout's `cratonvm.exe`).
- **Run a class**:
  `cratonvm-rm-run.exe --java-home "C:\Program Files\Eclipse Adoptium\jdk-25.0.2.10-hotspot" -c <classdir> <MainClass>`
- **Probes**: `C:\craton\rm-probes\` — `NpeProbe` (JEP-358 messages),
  `LoopProbe` (LICM/DSE/bg-compile checksum), `ClinitProbe` (nested
  `<clinit>`/interface static-final), `UnrollProbe`/`UnrollProbe2` (unroll across
  strides/ops/trips). Each is run against both `cratonvm-rm-run.exe` and the JDK
  25 `java.exe` and the checksums compared.
- **Build/test scripts** (MSVC env + libffi includes, Admin paths):
  `C:\craton\_build_run.bat` (debug binary → unique name),
  `C:\craton\_check_integ.bat` (`cargo check` affected crates + `cratonvm-cli`),
  `C:\craton\_test_jit.bat` (`cargo test -p cratonvm-jit`).
- **Diagnostics**: `CRATONVM_DBG_UNROLL=1` logs unroll candidates / bail reasons
  / successful unrolls; `CRATONVM_DBG_DUMP_JIT=LIST` lists JIT-compiled methods.

## Worktrees (parked under C:\craton\)
`CratonVM-rm-{integ,npe,proxy,keystore,embed,cdi,tiered,iropt}` — per-item
worktrees off `dev`. `rm/integ` (now `rm/iropt-unroll`) holds the integration +
the live-run binary's `target/`. Remove with `git worktree remove` when done.
