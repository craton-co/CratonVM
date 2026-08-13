# CratonVM JIT Compiler — architecture and optimization level

This is the source of truth for what the JIT does today. Verify claims by
reading the current source, not by trusting comments or older documents.

---

## Current JIT architecture

The JIT is a tiered, two-backend compiler with a background compilation
pipeline, a real deoptimization framework, and a second (AArch64) target.

### Module size

| Location | Lines | Role |
|---|---|---|
| `jit/src/` (crate `cratonvm-jit`) | **81,903** across 17 files | codegen backends, IR pipeline, tiering, deopt |
| `jit/tests/` | 8,307 | differential / IR-vs-singlepass / intrinsic test suites |
| `vm/src/jit/` (part of `cratonvm-vm`) | **15,197** across 7 files | VM-side glue: JIT-called helpers, skip-list, GC root scanning |
| **Total (jit crate + vm/src/jit)** | **~105,400** | |

`jit/src/x64.rs` alone is 37,327 lines (still the largest single file — the
x86-64 single-pass emitter, including several hundred unit tests). A second,
independent backend now exists: `jit/src/aarch64.rs` (2,293 lines) +
`jit/src/aarch64_backend.rs` (5,734 lines) — CratonVM has a real ARM64 JIT
backend. Excluding both AArch64 files, the
x86-64-only portion of `jit/src` is ~73,900 lines — roughly **10x** the old
"~7,200 line" figure even before counting `vm/src/jit/`.

The IR pipeline itself is substantial: `ir.rs` (3,149, graph builder),
`ir_lower.rs` (3,342, IR→x64 lowering), `ir_optimize.rs` (4,039, optimization
passes), `ir_schedule.rs` (737, scheduling), `escape_analysis.rs` (2,280,
escape analysis / scalar replacement), `scev.rs` (665, scalar evolution),
`null_check_elim.rs` (658), `loop_analysis.rs` (555). Supporting
infrastructure: `tiered.rs` (2,549), `deopt.rs` (2,407), `regalloc.rs` (1,830,
graph-coloring register allocator), `pgo.rs` (1,981, profile-guided
optimization data), `profile.rs` (1,150, interpreter-side profile collection),
`platform.rs` (537, W^X executable-memory allocation).

### Tiered compilation — background pipeline is DEFAULT-ON

`jit/src/tiered.rs` implements a HotSpot-style tiering scheme
(`Interpreter → C1 → C1WithProfiling → FullProfile → C2`). Default policy
(`CompilationPolicy`, `tiered.rs`): C1 threshold 200 invocations, C2 threshold
5,000, `c2_min_invocations` 1,000 — each overridable via
`CRATONVM_TIER_C1_THRESHOLD` / `_C2_THRESHOLD` / `_C2_MIN_INVOCATIONS`.

As of the "wire-tiered-manager Step 7" change, **the background compilation
pipeline is default-on** (`bg_compile()`, `vm/src/runtime/env_cache.rs`;
opt-out `CRATONVM_BG_COMPILE=0`): a hot method's invocation-count trigger
enqueues a `CompilationTask` for a worker thread instead of compiling inline
on the mutator; the mutator keeps interpreting until the worker publishes into
the shared JIT cache. **C1→C2 supersede is also default-on**
(`c2_supersede()`, opt-out `CRATONVM_C2_SUPERSEDE=0`): once a method's C1
(single-pass) body is published, an eligible candidate is enqueued for a
low-priority C2 (IR-optimized) recompile; on publish, cached invoke-site
entries are invalidated so callers re-resolve to the new body. Net effect
under default settings: a hot method is first eagerly compiled by the mutator
via the single-pass backend on its first call (unchanged from before Step 7),
then re-tiered off-thread to the optimizing backend once profiling confirms
its shape.

### Two backends: single-pass (x64.rs) vs IR pipeline

The older single-pass emitter (`x64.rs`) remains the universal fallback and
handles every JIT-eligible method. A separate IR-based backend
(`ir.rs`/`ir_lower.rs`/`ir_optimize.rs`) is used for a bounded but broad
subset of methods by default. Admission gate (`ir_compatible()`, `ir.rs`)
declines (falls back to single-pass) on: `athrow`, any `invokedynamic`, more
than 5 simple invokes / field ops / static-field ops, more than 3 `new` /
`anewarray`, any `multianewarray`, and any `checkcast`/`instanceof` (no IR
lowering exists for these yet).

At the VM layer (`vm/src/runtime/env_cache.rs`, which is what actually governs
the running VM — the per-flag doc comments inside the `jit` crate itself are
stale and describe an earlier, narrower default), the IR path by default
admits: pure-int/ref methods, plus methods using `long`, plus methods using
`float`/`double`, plus `invokestatic` and non-`<init>` `invokespecial` calls
to oop-free-int callees. `invokevirtual`/`invokeinterface` sites remain
single-pass-only by default (`CRATONVM_JIT_IR_CALL_VIRTUAL` opts in). So: the
IR backend is on by default for a fairly broad "simple, call-light,
exception-free, non-virtual-dispatch" method shape, not an experimental
opt-in feature.

### On-Stack Replacement (OSR) — default-ON, live threshold 1000

Master gate `osr_backedge_enabled()` (`CRATONVM_JIT_OSR`) defaults to on. The
threshold that actually gates an OSR attempt is **1,000 back-edges**
(`OSR_THRESHOLD` in `interpreter.rs`, override `CRATONVM_TIER_OSR_BACKEDGE`) —
not the 10,000 figure a dead `CompilationPolicy.osr_threshold` field would
suggest (that field's only consumer, `TieredCompilationManager::on_backedge`,
has zero call sites in the running VM). Since the background pipeline is
default-on, OSR compilation also happens off-thread by default: a hot
back-edge enqueues an OSR request, and the mutator only *enters* a
worker-published OSR artifact; only `CRATONVM_BG_COMPILE=0` restores the older
synchronous inline OSR compile.

A prior blanket rule had permanently denied OSR for any method containing a
primitive `newarray`, added as a workaround for a suspected corruption
(`GOST3412_2015Engine.init_gf256_mul_table`). That workaround has been
default-lifted (`osr_newarray_allowed()`, opt-out `CRATONVM_OSR_NEWARRAY=0`) —
the corruption did not reproduce after later GC-root-coverage fixes
(ThreadLocal value rooting, precise JIT maps, moving-young), and the blanket
deny had been silently costing `sieve250k` ~55x (3.2s → 177s) before it was
lifted.

### Guarded-inline getfield — default-ON, region-bounds-guarded

`guarded_inline_getfield_enabled()` (`x64.rs`) is on unless
`CRATONVM_JIT_GETFIELD_HELPER` is set. History: an earlier hardening routed
every JIT `getfield` through the checked `jit_getfield` helper to close a
stale/garbage-receiver SIGSEGV — at a measured ~4.7x cost on field-heavy
workloads like `bintrees-16`. The current default instead null/alignment-checks
the receiver and validates it against the GC's published `[base, end)` heap
region bounds (the same containment check `is_object_address` uses) before a
raw inline load; anything that fails the check falls back to the checked
helper, preserving its NPE / `i64::MIN`-sentinel semantics exactly. A separate,
still-opt-in `CRATONVM_JIT_INLINE_GETFIELD` raw (unguarded) path exists only
for A/B measurement, not as a production default.

### Compact reference-field layout — default-ON

`compact_ref_fields_enabled()` (`types/src/field_layout.rs`) is on unless
`CRATONVM_COMPACT_REF_FIELDS=0`. Reference instance fields are stored as bare
8-byte pointers instead of the legacy 16-byte tagged `Value` cell; the GC
consults a per-class oop-map (byte offsets of reference fields) built at
class-define time instead of detecting references by cell tag.

Two independent inline `putfield` (reference-field write) paths exist:
- **Legacy 16-byte inline putfield** — opt-in (`CRATONVM_JIT_INLINE_PUTFIELD`,
  default off) and additionally gated on the compact layout being *disabled* —
  under an unmodified default config it is unreachable.
- **Compact inline putfield** — default-on, riding entirely on
  `compact_ref_fields_enabled()` with no separate gate. Emits an 8-byte
  bare-pointer store on the barrier-free fast path (non-null, young-gen
  receiver, null old value, in-bounds index), falling back to the compact-aware
  `jit_putfield_object` helper (full SATB pre-barrier + card write-barrier)
  otherwise.

### Deoptimization and scalar replacement

`jit/src/deopt.rs` implements a real deopt framework: `DeoptReason`
(`NullCheck`, `ClassCheck`, `BoundsCheck`, `DivByZero`, `ReceiverTypeChanged`,
`UncommonTrap`, `OsrExit`, ...), `DeoptAction`
(`Reinterpret`/`RecompileAndReinterpret`/`MakeNotEntrant`/`MakeNotCompilable`),
and `DeoptimizationPoint` (native offset, bci, reason, action, frame state).
`Op::Guard` IR nodes tie a speculative optimization (a bounds-check elision, a
null/type assumption, a loop-header bounds guard, ...) to a specific bytecode
index and a `FrameState` describing how to reconstruct every live interpreter
local/stack slot from register/spill locations. On a guard failure, control
transfers to a deopt trampoline that materializes a precise interpreter frame
at the trapping bci and resumes there — not a whole-method re-run.

The master gate `deopt_real_enabled()` (`CRATONVM_DEOPT_REAL`) is default-on. A further extension, guard-surviving scalar
replacement (`CRATONVM_SCALAR_DEOPT`, default **off**, opt-in), lets an
escape-analysis-eliminated object survive a guard failure by materializing it
on demand from a `FrameValue::VirtualObject` descriptor instead of forcing a
full method re-run whenever a scalar-replaced object is live at a deopt point.
Scalar replacement itself (`jit/src/escape_analysis.rs`) runs as part of the
IR optimizer for eligible allocations and includes monitor/lock elision over
scalar-replaced receivers.

### Self-recursive call inlining — default-ON

`inline_self_guard_enabled()` (`x64.rs`, opt-out
`CRATONVM_JIT_INLINE_SELF_GUARD=0`). A method with direct self-recursive call
sites reserves one frame slot, fills it once in the prologue from a leaf
helper, and each self-call site emits a cheap inline stack-depth compare
instead of calling the full `self_call_stack_guard` helper on every call — the
helper remains as the fallback that actually raises `StackOverflowError`. OSR
trampolines initialize the slot to a sentinel so OSR-entered frames always
take the helper path (they bypass the prologue).

### BouncyCastle JIT eligibility

`vm/src/jit/skip_list.rs` is the single source of truth for JIT eligibility.
Under the default (`Conservative`) policy, `org/bouncycastle/` is blanket-banned
from JIT compilation *except* for an explicit carveout:
`org/bouncycastle/crypto/{BufferedBlockCipher,DefaultBufferedBlockCipher}` and
everything under `crypto/{engines,io,modes,paddings}/`, plus everything under `org/bouncycastle/math/` (EC + field arithmetic),
with narrow forced-interpreted exceptions (`CAST5Engine`/`CAST6Engine` key
schedule, `NISTCTSBlockCipher.processBytes`). The blanket ban traces to a
suspected cross-package JIT arg-marshalling miscompile first seen during BC
provider registration; it was deliberately held in place across several JIT
hardening rounds pending a clean `org.bouncycastle.math.ec.test.AllTests` run
under the allow-override, which now passes (14/14 OK) after later
root-coverage fixes (moving-young GC + precise JIT maps, RRWL/refproc roots,
ThreadLocal value rooting, guarded-inline getfield). The rest of BC
(`asn1/`, `util/`, ...) remains banned.

### Precise JIT stack maps — default-ON

`precise_jit_maps_enabled()` (`x64.rs`, opt-out
`CRATONVM_NO_PRECISE_JIT_MAPS`) is default-on. It
had originally shipped default-off (a ~6x throughput tax on call-heavy code,
"BUG-01") but re-measurement found the tax gone on current `dev` — more
aggressive inlining leaves far fewer real call safepoints in hot
reflection/framework methods. A dependent flag,
`precise_inline_frame_record_enabled()` (also default-on), further optimizes
this by storing the frame pointer inline instead of via a helper call.

### Bytecode and intrinsic coverage

Core coverage is close to the old ~140-opcode set (loads/stores/arrays/
arithmetic/branches/fields/invokes/`newarray`/`multianewarray`) plus
`new`/`anewarray`/`checkcast`/`instanceof`/`tableswitch`/`lookupswitch`/
`athrow`/`monitorenter`/`monitorexit` (with lock elision over scalar-replaced
receivers). `invokedynamic` is no longer a permanent compile-time bail on the
single-pass backend: the call site itself lowers to an uncommon-trap deopt
stub, so the rest of the method still compiles (the IR backend still declines
any method containing `invokedynamic`).

On top of the opcode set sits a call-site intrinsics layer
(`JitIntrinsic`, `jit/src/lib.rs`) — roughly 40 intrinsics, each with a
receiver/type guard that deopts to the normal call path on mismatch:
- `Math`/`StrictMath`: `sqrt`, `floor`, `ceil`, `rint`, `abs`, `fma`, `min`/`max`,
  `multiplyHigh`, `unsignedMultiplyHigh`
- `Integer`/`Long` bit ops: `bitCount`, `numberOfLeadingZeros`,
  `numberOfTrailingZeros`, `reverseBytes`, `highestOneBit`, `lowestOneBit`,
  `reverse`, `compare`, `rotateLeft`, `rotateRight`
- `System.arraycopy` (inline memmove fast path)
- `String`: `length`, `isEmpty`, `charAt`, `hashCode`, `equals`, `compareTo`,
  `indexOf(int)`, `indexOf(String)` (coder-aware for LATIN1/UTF16)
- `Arrays.fill`/`Arrays.equals` (4 element widths each), `Arrays.sort` for
  primitive arrays (insertion sort, inline)
- `CRC32`/`CRC32C.update`

### Summary table

| Feature | Default | Opt-out / opt-in var |
|---|---|---|
| Background compilation pipeline | **ON** | `CRATONVM_BG_COMPILE=0` |
| C1→C2 supersede | **ON** | `CRATONVM_C2_SUPERSEDE=0` |
| IR backend (int/ref/long/FP, non-virtual calls) | **ON** (bounded shape) | see `ir_compatible()` |
| IR backend for virtual/interface calls | off | `CRATONVM_JIT_IR_CALL_VIRTUAL` |
| Back-edge OSR | **ON**, threshold 1000 | `CRATONVM_JIT_OSR=0` |
| OSR for `newarray`-containing methods | **ON** | `CRATONVM_OSR_NEWARRAY=0` |
| Guarded-inline getfield (region-bounds-checked) | **ON** | `CRATONVM_JIT_GETFIELD_HELPER=1` |
| Raw (unguarded) inline getfield | off | `CRATONVM_JIT_INLINE_GETFIELD` |
| Compact reference-field layout | **ON** | `CRATONVM_COMPACT_REF_FIELDS=0` |
| Compact inline putfield | **ON** (rides on layout) | — |
| Legacy 16-byte inline putfield | off (and dead under default layout) | `CRATONVM_JIT_INLINE_PUTFIELD` |
| Deopt framework (`DEOPT_REAL`) | **ON** | `CRATONVM_DEOPT_REAL=0` |
| Guard-surviving scalar replacement | off | `CRATONVM_SCALAR_DEOPT` |
| Self-recursive inline stack guard | **ON** | `CRATONVM_JIT_INLINE_SELF_GUARD=0` |
| BC `crypto/{engines,io,modes,paddings}` + `math/` JIT | **allowed** | — |
| BC blanket ban (`asn1/`, `util/`, ...) | still banned | `CRATONVM_JIT_ALLOW_PACKAGES` |
| Precise JIT stack maps | **ON** | `CRATONVM_NO_PRECISE_JIT_MAPS` |

### Performance — current status

Checksums stay exact (e.g. `bintrees-18` = 68332206) across every change
described above. The current CratonVM-vs-HotSpot ratios live in
[`../BENCHMARK.md`](../BENCHMARK.md); do not restate them here.

A cdb sampling profile of `bintrees-20` finds the dominant cost is allocation
plus young-GC throughput (sweep, free-list scan, old-gen spill for the live
tree) rather than JIT codegen — i.e. further *JIT* optimization has limited
headroom left for that workload, and GC throughput is the next lever.

---

## Technical Implementation

### JIT module (~105,400 lines — see "Current JIT architecture" above for the full breakdown)

| File | Lines | Purpose |
|------|-------|---------|
| `jit/src/x64.rs` | 37,327 | x86-64 single-pass emitter, Compiler, ~hundreds of unit tests |
| `jit/src/lib.rs` | 10,670 | `try_compile`/`try_compile_inner`, intrinsics registry, feature flags |
| `jit/src/aarch64_backend.rs` | 5,734 | AArch64 codegen backend |
| `jit/src/ir_optimize.rs` | 4,039 | IR optimization passes |
| `jit/src/ir_lower.rs` | 3,342 | IR → x64 lowering |
| `jit/src/ir.rs` | 3,149 | IR graph builder, `ir_compatible` admission gate |
| `jit/src/tiered.rs` | 2,549 | tiered compilation manager |
| `jit/src/deopt.rs` | 2,407 | deoptimization framework |
| `jit/src/aarch64.rs` | 2,293 | AArch64 instruction encoding |
| `jit/src/escape_analysis.rs` | 2,280 | escape analysis / scalar replacement |
| `jit/src/pgo.rs` | 1,981 | profile-guided optimization data |
| `jit/src/regalloc.rs` | 1,830 | graph-coloring register allocator |
| `vm/src/jit/helpers.rs` | 7,075 | JIT-called runtime helpers (getfield/putfield/invoke/newarray/...) |
| `vm/src/jit/skip_list.rs` | 3,898 | JIT eligibility policy (see "BouncyCastle JIT eligibility" above) |
| `vm/src/jit/conservative_roots.rs` | 2,411 | GC root scanning for JIT frames |
| *(remaining files, `jit/src` + `vm/src/jit`)* | ~24,900 | `profile.rs`, `ir_schedule.rs`, `scev.rs`, `null_check_elim.rs`, `loop_analysis.rs`, `platform.rs`, `xt_root_scan.rs`, `alloc_class_cache.rs`, `disasm.rs`, `mod.rs` |

### x86-64 Instructions Emitted

| Category | Instructions |
|----------|-------------|
| **Data movement** | MOV reg↔mem, MOV reg←imm32/64, PUSH/POP, LEA |
| **Arithmetic** | ADD, SUB, IMUL (reg and imm), NEG (32-bit and 64-bit) |
| **Magic division** | IMUL+SAR+ADD (constant div/rem without IDIV) |
| **Bitwise** | AND, OR, XOR, SHL, SHR, SAR, BTC |
| **Comparison** | CMP, CMP-imm, Jcc (6 conditions), CMOV, SETcc |
| **Control flow** | CALL rel32, JMP rel32, RET |
| **Extension** | REX.W prefixes, MOVSXD, MOVSX, MOVZX |
| **Array access** | SIB addressing (*1, *2, *4, *8), SHL for *16 |
| **SSE float** | MOVD GPR↔XMM, ADDSS, SUBSS, MULSS, DIVSS, UCOMISS |
| **SSE double** | MOVQ GPR↔XMM, ADDSD, SUBSD, MULSD, DIVSD, UCOMISD |
| **SSE convert** | CVTSI2SS/SD, CVTTSS/SD2SI, CVTSS2SD, CVTSD2SS |
| **Stack frame** | MOV save/restore callee-saved (R12-R15,RBX,RSI,RDI) |

A parallel AArch64 encoder (`jit/src/aarch64.rs`) targets the equivalent
instruction classes for the ARM64 backend.

### JIT-Compiled JVM Bytecodes and Intrinsics

See "Bytecode and intrinsic coverage" under "Current JIT Architecture" above
for the current, verified list — coverage has grown beyond the historical
130-opcode table below with `new`/`anewarray`/`checkcast`/`instanceof`/
`tableswitch`/`lookupswitch`/`athrow`/`monitorenter`/`monitorexit`/partial-
`invokedynamic`, plus ~40 call-site intrinsics (Math, Integer/Long bit ops,
String access/search, Arrays, CRC32).

```
  Constants:    iconst_m1..5, lconst_0/1, fconst_0/1/2, dconst_0/1,
                bipush, sipush
  Loads:        iload, lload, fload, dload, aload,
                iload_0..3, lload_0..3, fload_0..3, dload_0..3, aload_0..3
  Stores:       istore, lstore, fstore, dstore, astore,
                istore_0..3, lstore_0..3, fstore_0..3, dstore_0..3, astore_0..3
  Arrays:       iaload/iastore, laload/lastore, faload/fastore,
                daload/dastore, aaload/aastore, baload/bastore,
                caload/castore, saload/sastore, arraylength
  Int arith:    iadd, ladd, isub, lsub, imul, lmul, idiv, ldiv, irem, lrem
  Float arith:  fadd, dadd, fsub, dsub, fmul, dmul, fdiv, ddiv
  Negation:     ineg, lneg, fneg, dneg
  Shifts:       ishl, lshl, ishr, lshr, iushr, lushr
  Bitwise:      iand, land, ior, lor, ixor, lxor
  Increment:    iinc
  Conversion:   i2l, l2i, i2f, i2d, l2f, l2d, f2i, f2l, f2d, d2i, d2l, d2f,
                i2b, i2c, i2s
  Comparison:   lcmp, fcmpl, fcmpg, dcmpl, dcmpg
  Branches:     ifeq, ifne, iflt, ifge, ifgt, ifle
                if_icmpeq, if_icmpne, if_icmplt, if_icmpge, if_icmpgt, if_icmple
  Jump:         goto
  Return:       ireturn, lreturn, freturn, dreturn, areturn, return
  Fields:       getfield, putfield
  Invoke:       invokestatic, invokevirtual, invokespecial, invokeinterface
  Allocation:   newarray, multianewarray (2D)
  Stack:        dup, pop, swap, nop
```

---

## Key Metrics Summary

| Metric | Value |
|--------|-------|
| Total Rust LoC | **~1,350,000** (22 workspace members; `.rs` files under the root `Cargo.toml` members, excluding `target/`, the non-member `fuzz/` workspace, and any `vendor/` directory — 1,349,978 lines across 702 files) |
| JIT LoC (jit crate + vm/src/jit) | **~131,600** (`jit/src` 100,391 + `jit/tests` 9,674 + `vm/src/jit` 21,549) |
| JIT backends | x86-64 (single-pass + IR-optimizing), AArch64 |
| JIT bytecodes | ~130 core opcodes + ~40 call-site intrinsics |
| JIT unit/integration tests | ~hundreds in `x64.rs` + differential/IR-vs-singlepass/intrinsic suites in `jit/tests/` |
| Test corpus | Large Rust/Java unit, integration, regression, difftest, and fuzz layers |
| Lint status | `clippy -D warnings` is a release gate, not a baked-in metric |
| Native methods | **~3,100+** |
| vs JDK C2 | See [`../BENCHMARK.md`](../BENCHMARK.md) for the current interleaved series |
