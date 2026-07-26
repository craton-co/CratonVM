# CratonVM JIT Compiler — Architecture and Optimization Level

*Last verified against source 2026-07-10 (branch `claude/reverent-engelbart-84796b`,
based on `dev`). The "Round 8–26" material below (through the March 2026
snapshot) is preserved as **historical narrative** — it describes an earlier,
much smaller JIT and its own benchmark numbers, which are no longer
representative of current `dev`. The "Current JIT Architecture" section
replaces it as the source of truth for what the JIT does today.*

---

## Current JIT Architecture (as of 2026-07-10)

The JIT has grown well past the single-pass-only, ~140-opcode, ~7,200-line
compiler the Round 26 snapshot describes. It is now a tiered, two-backend
compiler with a background compilation pipeline, a real deoptimization
framework, and a second (AArch64) target. Every claim below was verified by
reading the current source, not by trusting comments or older docs.

### Module size (verified `wc -l`, 2026-07-10)

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
backend that did not exist at Round 26. Excluding both AArch64 files, the
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
`CRATONVM_JIT_GETFIELD_HELPER` is set. History: a 2026-07-09 hardening routed
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

The master gate `deopt_real_enabled()` (`CRATONVM_DEOPT_REAL`) has been
default-on since 2026-06-22. A further extension, guard-surviving scalar
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
everything under `crypto/{engines,io,modes,paddings}/`, plus — as of a 2026-07-10
change — everything under `org/bouncycastle/math/` (EC + field arithmetic),
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
`CRATONVM_NO_PRECISE_JIT_MAPS`) was re-flipped to default-on 2026-07-07. It
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

A significant addition since Round 26 is a call-site intrinsics layer
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

Checksums stay exact (e.g. `bintrees-18` = 68332206) across all the changes
above; timing has not been captured in a single clean, apples-to-apples
JDK-C2-comparison pass since the Round 26 snapshot. The most recent trustworthy
data point (2026-07-10, checksums cross-checked, same-host interleaved A/B, no
regression vs the prior dev tip) is from merging the SATB clone-free +
allocation-init-cache work on top of this JIT state: `bintrees-18` and
`sieve250k`/`fib44` all matched golden checksums with timing at-or-better than
the pre-merge baseline. A cdb sampling profile of `bintrees-20` around that
point found the dominant cost is now allocation + young-GC throughput (sweep,
free-list scan, old-gen spill for the live tree) rather than JIT codegen —
i.e. further *JIT* optimization has limited headroom left for that workload;
GC throughput is the next lever. Refreshing the JDK-C2 ratio table below
requires an idle benchmark host — the primary Windows dev box currently has a
background process pegging CPU that invalidates timing runs until cleared.

---

## Historical Journey: From 97x Slower to the March 2026 Round 26 Snapshot

```
                    Performance vs OpenJDK -Xint (interpreter mode)
                    ================================================

  Before Opt   ████████████████████████████████████████████████████  97x SLOWER
  Round 3      ████████                                              8x
  Round 7      ██████                                               6.5x
  Round 8 JIT  ██                                                   1.8x
               ▏                                                    ←── OpenJDK -Xint
  Round 10     ◀═══                                                 3.2x FASTER!
  ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─
  (Workload scaled 1000x for stable timing at Rounds 11+)

  Round 14     ◀═══════                                             6.9x FASTER!
  Round 15     ◀═══════  (coverage: +38 bytecodes, all array/float types)
  Round 16     ◀═══════  (SSE: +30 bytecodes, full float/double compute)
  Round 17     ◀═══════  (field access: getfield/putfield, object properties)
  Round 18     ◀═══════  (LICM: hoist invariant aaload out of loops)
  Round 19     ◀═══════  (VM context: checkcast/instanceof/getstatic/putstatic)
  Round 20     ◀══════════════  (compact refs + JIT bug fixes: ~15x faster!)
  Round 21     ◀══════════════  (array bounds check elimination)
  Round 22     ◀══════════════  (JIT invokevirtual/invokespecial)
  Round 23     ◀══════════════  (On-Stack Replacement at hot loops)
  Round 24     ◀═══════════════════  (AVX2 SIMD vectorization)
  Round 25     ◀═══════════════════  (SoA Value layout: 16B → 9B per slot)
  Round 26     ◀═══════════════════  (loop unrolling, speculative BCE, OSR fast-path)
               ▏                                                    ←── OpenJDK -Xint
               ▏  ◀═════                                           ←── OpenJDK C2 (historical R26)
```

*Everything below this point describes the JIT as of the March 2026 Round 26
snapshot and its immediate 2026-07 follow-ups, preserved for historical
context. See "Current JIT Architecture" above for what the JIT actually does
today.*

---

## What Is CratonVM?

A Java Virtual Machine written entirely in Rust:

- **~1,290,000 lines** of Rust code (project-wide, across the 20 workspace
  member crates, measured 2026-07-25; the JIT alone is now ~116,600 lines
  across `jit/src`, `jit/tests`, and `vm/src/jit` — see above)
- Large Rust/Java test corpus with clippy and formatting tracked as release gates
- **~3,100+ native method** registrations (java.lang, java.util, java.io, java.time, ...)
- Full interpreter with 140+ fast-path bytecodes
- Generational garbage collector with write barriers
- Multi-threading with monitors, locks, and barriers
- Lambda/invokedynamic support
- **Two JIT backends**: x86-64 (single-pass + IR-optimizing, ~130 opcodes plus
  ~40 call-site intrinsics) and a newer AArch64 backend
- Historical March 2026 Round 26 snapshot reached 1.50x of JDK C2 on QuickBench.
- Historical 2026-07-02 snapshot (`b80c50b5`), before back-edge OSR flipped
  default-on, was 46.2x slower by default and 8.25x slower with
  `CRATONVM_JIT_OSR=1 CRATONVM_JIT_THRESHOLD=1`.
- Historical 2026-07-08 snapshot (`bfc26c2d`), with OSR default-on since
  2026-07-04, was 3.7x slower by default.
- See "Performance — current status" above for why this ratio has not been
  refreshed since.

---

## The JIT Compiler

### Architecture

```
  Java Source          javac          JVM Bytecode         CratonVM JIT
  ┌──────────┐      ────────►      ┌──────────────┐      ────────►      ┌─────────────┐
  │ int fib( │                     │ iload_0      │                     │ push rbp    │
  │   int n  │                     │ iconst_1     │                     │ mov rbp,rsp │
  │ ){       │                     │ if_icmpgt +5 │                     │ cmp r12d, 1 │
  │   if(n<=1│                     │ iload_0      │                     │ jg .L1      │
  │     ret n│                     │ ireturn      │                     │ movsxd rax  │
  │   ret    │                     │ iload_0      │                     │ jmp .epilog │
  │    fib(  │                     │ iconst_1     │                     │.L1:         │
  │     n-1) │                     │ isub         │                     │ lea ecx,    │
  │   +fib(  │                     │ invokestatic │                     │   [r12-1]   │
  │     n-2) │                     │ iload_0      │                     │ call fib    │
  │ }        │                     │ iconst_2     │                     │ mov r13,rax │
  └──────────┘                     │ isub         │                     │ lea ecx,    │
                                   │ invokestatic │                     │   [r12-2]   │
                                   │ iadd         │                     │ call fib    │
                                   │ ireturn      │                     │ add rax,r13 │
                                   └──────────────┘                     │ ret         │
                                                                        └─────────────┘
```

### Key JIT Optimizations (Rounds 8-25)

| Round | Optimization | Technique |
|-------|-------------|-----------|
| R8 | Basic JIT | Bytecode → x86-64, self-recursive calls |
| R9 | Inline array ops | newarray, iaload/iastore via heap pointer |
| R10 | Register locals | Locals 0-3 in R12-R15 (zero-cost loads) |
| R11 | Magic division | Constant div/rem → multiply-and-shift |
| R12 | Extended regs | 7 callee-saved regs on Windows |
| R13 | Compact arrays | byte[]=1B, int[]=4B, long[]=8B per element |
| R14 | Inline aaload | SHL+MOV for Object[] element access |
| R15 | Inline aastore | Discriminant + pointer write inline |
| R15 | All array types | char[], short[], long[], double[], float[] |
| R15 | Float/double data | fconst, fload/fstore, dconst, dload/dstore |
| R15 | Type narrowing | i2b, i2c, i2s conversions |
| R15 | All return types | freturn, dreturn, void return |
| R16 | SSE arithmetic | fadd/fsub/fmul/fdiv via ADDSS/SUBSS/MULSS/DIVSS |
| R16 | SSE double arith | dadd/dsub/dmul/ddiv via ADDSD/SUBSD/MULSD/DIVSD |
| R16 | Float/double cmp | fcmpl/fcmpg/dcmpl/dcmpg via UCOMISS/UCOMISD |
| R16 | All conversions | i2f/i2d/l2f/l2d/f2i/f2l/f2d/d2i/d2l/d2f via SSE |
| R16 | FP negation | fneg (XOR bit 31), dneg (BTC bit 63) |
| R17 | getfield | Read object fields via jit_getfield helper |
| R17 | putfield | Write fields via type-specific helpers + write barrier |
| R18 | LICM | Hoist invariant aaload out of loops (preheader block) |
| R19 | VM context | SharedVm pointer replaces heap pointer for full VM access |
| R19 | checkcast/instanceof | Class hierarchy check via helper call-outs |
| R19 | Null/ref branches | ifnull, ifnonnull, if_acmpeq, if_acmpne, aconst_null |
| R19 | Static fields | getstatic/putstatic with compile-time CP resolution |
| R20 | Compact ref arrays | Object[]=8B per element (was 16B Value) |
| R20 | SIB scale=3 refs | `MOV RAX,[RAX+RCX*8+32]` for aaload/aastore |
| R20 | Prologue fix | MOV-based callee-saved save/restore (not PUSH/POP) |
| R20 | Fast-path fix | SharedVm ptr (not heap ptr) for JIT invocation |
| R21 | Bounds checks | Safe array access + loop elimination (BCE) |
| R21 | Out-of-line throw | AIOOBE via out-of-line helper at method end |
| R22 | JIT invokevirtual | Virtual/special/interface calls via helper bridge |
| R22 | Cross-method static | Direct CALL to other compiled methods |
| R23 | OSR | On-Stack Replacement at hot loop back-edges |
| R23 | State transfer | Interpreter locals → JIT frame mid-execution |
| R24 | SIMD detection | CPUID check for AVX2 support |
| R24 | AVX2 vectorization | VPMULLD/VPADDD for 8-int parallel arithmetic |
| R24 | Scalar cleanup | Remainder loop for non-aligned lengths |
| R25 | SoA locals | `Vec<u64>` + `Vec<u8>` tags (was `Vec<Value>`) |
| R25 | SoA stack | ValueStack with separate vals/tags arrays |
| R25 | GC SoA scan | Tag-based object ref extraction for GC roots |
| R26 | Loop unrolling | 4x for bodies ≤20 bytecodes, 2x for 21-30 |
| R26 | Speculative BCE | Loop header guard: arr.length ≥ bound, skip per-element checks |
| R26 | Graph-color regalloc | Interference graph + coloring for 7 callee-saved regs |
| R26 | OSR fast-path | OSR-compiled methods reused for normal invocation-level calls |
| R26 | Invocation counting | Profile-guided JIT at 2000-call threshold |

*Post-R26, current dev (see "Current JIT Architecture" above): tiered
background compilation (C1/C2), a second IR-optimizing backend alongside the
single-pass emitter, a real deoptimization framework with guard-surviving
scalar replacement, guarded-inline getfield with heap-region bounds checking,
an AArch64 backend, and roughly 40 call-site intrinsics.*

### Peephole Optimizations

```
  Constant + Arithmetic Fusion       Constant + Compare Fusion
  ─────────────────────────────      ──────────────────────────
  iconst_3                           iconst_0
  imul          ──►  IMUL RAX, 3    if_icmpge   ──►  CMP EAX, 0
                                                      JGE target

  Fused Compare-and-Branch           Magic Number Division
  ─────────────────────────          ────────────────────
  iload_0                            i % 7  ──►  IMUL + SHR + SUB
  iload_1                                        (no IDIV instruction!)
  if_icmplt  ──►  CMP R12d, R13d
                  JL target
```

---

## Benchmark Results

### QuickBench — Scaled Workloads (Historical Round 26)

*Historical measurement from 2026-03-31 on Windows 11, JDK 25.0.1 LTS. This is
not the current benchmark snapshot — see "Performance — current status" above
for why a fresh comparison has not yet been captured.*

```
  Benchmark               JDK C2      CratonVM R26    Ratio     Notes
  ────────────────────     ──────      ───────────    ─────     ─────
  Arithmetic (300M)         889 ms      1676 ms      1.89x     ★ loop unrolling + magic div
  Fibonacci(42) recursive  1876 ms      2457 ms      1.31x     ★ JIT self-calls + regalloc
  Sieve (100K×500 reps)     324 ms       510 ms      1.57x     ★ OSR + speculative BCE
  Matrix 500×500 multiply   351 ms       518 ms      1.48x     ★ LICM + compact refs
  ─────────────────────────────────────────────────────────────────────────────
  TOTAL                    3440 ms      5161 ms      1.50x

  Binary Trees (depth=18)   714 ms     16657 ms     23.3x     ✗ GC allocation bottleneck
  Historical overall: 1.50x on QuickBench vs HotSpot C2 (Binary Trees tracked separately)
```

★ = JIT-compiled to native x86-64 machine code

### Cumulative Performance Journey

```
                    Total Benchmark Time (lower is better)
  ═══════════════════════════════════════════════════════════

  Small workloads (1M arith, fib(28), sieve 100K, matrix 100×100):

  Unoptimized   ████████████████████████████████████████████  5064 ms  (97x)
  R3 interp     ████                                          427 ms   (8x)
  R7 unsafe     ███                                           330 ms  (6.5x)
  R8 JIT        █                                             140 ms  (1.8x)
  R10 regs      ▏                                              20 ms  (0.3x) ← FASTER!
               ────────────────────────────────────────────────────────────────
  JDK -Xint     ▏                                              79 ms   (1.0x)

  Scaled workloads (300M arith, fib(42), sieve×500, matrix 500×500):

  R13 compact   █████████████████████████████████████          4660 ms (2.4x slower)
  R14 full-regs ███████████████████████████████                3978 ms (1.93x slower)
  R20 compact+  █████████████████████████████████████████      9562 ms (1.8x slower)
  R25 SoA+SIMD  ████████████████████                           5210 ms (1.51x)
  R26 unroll    ███████████████████                            5161 ms (1.50x) ← March 2026 snapshot
               ────────────────────────────────────────────────────────────────
  JDK C2        █████████████                                  3440 ms  (1.0x)
  JDK -Xint    █████████████████████████████████████████████ 144543 ms (27.7x)

  R20→R26: BCE + invokevirtual + OSR + SIMD + SoA + loop unrolling + speculative BCE
```

### Per-Benchmark Deep Dive (Round 26)

```
  Fibonacci(42) — 267,914,296 recursive calls
  ═══════════════════════════════════════════

  JDK C2       ███████████████                                  1876 ms
  CratonVM R26  ████████████████████                            2457 ms  ★ JIT self-calls + regalloc
                                                                         ↑ gap: 1.31x

  Arithmetic (300M iterations, mixed ops)
  ═══════════════════════════════════════

  JDK C2       ████████                                          889 ms
  CratonVM R26  ███████████████                                  1676 ms  ★ loop unrolling
                                                                         ↑ gap: 1.89x

  Sieve of Eratosthenes (100K × 500 reps)
  ════════════════════════════════════════

  JDK C2       ████████                                          324 ms
  CratonVM R26  █████████████                                     510 ms  ★ OSR + speculative BCE
                                                                         ↑ gap: 1.57x

  Matrix 500×500 multiply (125M multiply-adds)
  ═════════════════════════════════════════════

  JDK C2       █████████                                         351 ms
  CratonVM R26  █████████████                                     518 ms  ★ LICM + compact refs
                                                                         ↑ gap: 1.48x
```

---

## Rounds 15-18: Float/Double, Fields, LICM

### 70 New Bytecodes (from ~60 to ~130 total)

```
  R8-R14 bytecodes (60):          R15 additions (38):          R16-17 additions (32):
  ─────────────────────           ─────────────────────        ──────────────────────
  iconst, lconst, bipush,        + fconst_0/1/2, dconst_0/1   + fadd, fsub, fmul, fdiv
  sipush, iload/store,           + fload/fstore (indexed+_N)   + dadd, dsub, dmul, ddiv
  lload/store, aload/store,      + dload/dstore (indexed+_N)   + fneg, dneg
  iaload/iastore, aaload,        + laload/lastore (long[], 8B) + fcmpl, fcmpg
  aastore, baload/bastore,       + faload/fastore (float[], 4B)+ dcmpl, dcmpg
  i/l arithmetic, shifts,        + daload/dastore (double[],8B)+ i2f, i2d, l2f, l2d
  bitwise, iinc, i2l, l2i,      + caload/castore (char[], 2B) + f2i, f2l, f2d
  lcmp, branches, goto,         + saload/sastore (short[], 2B)+ d2i, d2l, d2f
  ireturn, lreturn, areturn,    + freturn, dreturn, return     + getfield, putfield (R17)
  invokestatic, newarray,        + i2b, i2c, i2s
  arraylength, multianewarray,
  dup, pop, swap, nop
```

### Inline Array Access — SIB Encoding

```
  byte[]    MOVSX  EAX, BYTE [RAX + RCX*1 + 32]    scale=0  (1B each)
  char[]    MOVZX  EAX, WORD [RAX + RCX*2 + 32]    scale=1  (2B each)
  short[]   MOVSX  EAX, WORD [RAX + RCX*2 + 32]    scale=1  (2B each)
  int[]     MOVSXD RAX, DWORD [RAX + RCX*4 + 32]   scale=2  (4B each)
  float[]   MOVSXD RAX, DWORD [RAX + RCX*4 + 32]   scale=2  (4B, bit-pattern)
  long[]    MOV    RAX, QWORD [RAX + RCX*8 + 32]   scale=3  (8B each)
  double[]  MOV    RAX, QWORD [RAX + RCX*8 + 32]   scale=3  (8B, bit-pattern)
  Object[]  MOV    RAX, QWORD [RAX + RCX*8 + 32]   scale=3  (8B compact refs)
```

### Inline aastore — Compact Reference Write

```
  Before (R14):  CALL jit_aastore          (~15 instructions, function call overhead)

  R15 (Value):   SHL  RCX, 4              ; index *= 16 (SLOT_SIZE)
                 ADD  RCX, RAX            ; RCX = array_base + offset
                 MOV  QWORD [RCX+32], 4   ; write Object discriminant
                 MOV  QWORD [RCX+40], RDX ; write pointer value (16 bytes total)

  R20 (compact): MOV  QWORD [RAX+RCX*8+32], RDX    ; single 4-byte instruction!
                                                      ; raw 8-byte pointer, no Value enum
```

### Round 16: SSE Float/Double Pipeline

```
  Float arithmetic (fadd/fsub/fmul/fdiv):
  ═══════════════════════════════════════
  GPR (i64 bit-pattern)  →  XMM (IEEE 754)  →  SSE compute  →  GPR result

  pop RCX (value2)                  MOVD XMM1, ECX     ; 4 bytes to XMM
  pop RAX (value1)                  MOVD XMM0, EAX     ; 4 bytes to XMM
                                    ADDSS XMM0, XMM1   ; scalar float add
                                    MOVD EAX, XMM0     ; result back to GPR
  push RAX

  Double arithmetic (dadd/dsub/dmul/ddiv):
  ════════════════════════════════════════
  Same pattern with MOVQ (8 bytes) and ADDSD/SUBSD/MULSD/DIVSD

  Comparison (fcmpl/fcmpg/dcmpl/dcmpg):
  ═════════════════════════════════════
  UCOMISS XMM0, XMM1    ; compare, sets CF/ZF/PF
  SETA AL               ; value1 > value2 → 1     ┐
  SETB CL               ; value1 < value2 → 1     ├── extract flags BEFORE ALU
  SETP DL               ; NaN → 1                 ┘
  ... combine for -1/0/1 with NaN bias

  Type conversions — 12 opcodes via SSE:
  ═════════════════════════════════════
  i2f: CVTSI2SS XMM0, EAX           f2i: CVTTSS2SI EAX, XMM0
  i2d: CVTSI2SD XMM0, EAX           d2i: CVTTSD2SI EAX, XMM0
  l2f: CVTSI2SS XMM0, RAX (REX.W)   f2l: CVTTSS2SI RAX, XMM0 (REX.W)
  l2d: CVTSI2SD XMM0, RAX (REX.W)   d2l: CVTTSD2SI RAX, XMM0 (REX.W)
  f2d: CVTSS2SD XMM0, XMM0          d2f: CVTSD2SS XMM0, XMM0
```

### Round 17: Object Field Access (getfield/putfield)

*See "Guarded-inline getfield" and "Compact reference-field layout" above for
the current default-on behavior — the description below is the original R17
mechanism (unconditional helper call), since superseded.*

```
  getfield (0xb4) — read object field:
  ═════════════════════════════════════
  1. Field resolution at JIT compile time (cp_index → field_index + type)
  2. Pop object pointer from JIT stack
  3. CALL jit_getfield(obj_ptr, field_index)
     → reads Value at obj + 32 + field_index × 16
     → extracts i64: Int→sign-extend, Long→direct, Float/Double→IEEE bits

  putfield (0xb5) — write object field:
  ═════════════════════════════════════
  1. Pop value, then object pointer from JIT stack
  2. Call type-specific helper:
     • Primitives: jit_putfield_int/long/float/double(obj, idx, val)
     • References: jit_putfield_object(heap, obj, idx, val) + write barrier
```

### Round 18: Loop-Invariant Code Motion (LICM)

```
  Loop Detection & Invariant Analysis
  ════════════════════════════════════

  1. detect_loops():      find natural loops via backward branches
                          (goto/conditional with negative offset)

  2. find_modified_locals(): build 64-bit bitmask of locals written
                          within loop body (istore/astore/iinc)

  3. match_invariant_aaload(): pattern match:
                          aload X; iload Y; aaload
                          where X and Y are NOT in modified bitmask

  4. find_loop_hoists():  combine for nested loops, sort by span
                          (outermost first for deduplication)


  Before LICM:           After LICM:
  ──────────────         ────────────────────
  loop_header:           preheader:
    aload_0     ←┐        aload_0
    iload 4      │        iload 4
    aaload       │        aaload
    iload 7      │        MOV [spill], RAX    ← hoist to spill slot
    iaload       │      loop_header:
    ...          │        MOV RAX, [spill]    ← load from spill slot
    goto header ─┘        iload 7
                          iaload
                          ...
                          goto header


  Safety: no hoisting if loop contains aastore (0x53),
  which could invalidate the hoisted Object[] element.
```

---

## Technical Implementation

### JIT Module (current, ~105,400 lines — see "Current JIT Architecture" above for the full breakdown)

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

## How We Closed the Gap: R21-R25

```
  Round 21: Array Bounds Check Elimination (BCE)
  Added safety bounds checks to all 16 array opcodes with out-of-line throw.
  Loop analysis detects induction variables + array length bounds.
  Provably safe loops: ONE check before loop, not per-element.

  Round 22: JIT invokevirtual / invokespecial / cross-method invokestatic
  Helper bridge (jit_invoke_method) for JIT -> interpreter dispatch.
  Cross-method invokestatic: direct CALL to other compiled methods.
  Enables JIT for ~60%+ more real-world methods.

  Round 23: On-Stack Replacement (OSR)
  Backward branch hotness counter in Frame (threshold: 10,000).
  Compile + transfer interpreter locals -> JIT frame mid-execution.
  Significant for long-running methods with hot inner loops.

  Round 24: AVX2 SIMD Vectorization
  CPUID check for AVX2. Detects int reduction loop patterns.
  VPMULLD + VPADDD for 8 ints at once. Scalar cleanup for remainder.
  8x throughput on data-parallel arithmetic inner loops.

  Round 25: SoA (Structure of Arrays) Value Layout
  Stack/locals: Vec<Value> (16B/slot) -> Vec<u64> + Vec<u8> (9B/slot).
  44% memory reduction. Tag-based GC root scanning.
  ~12% overall improvement from better cache utilization.
```

---

## Optimization Roadmap

```
  Phase 1 (R15-16)  SSE Float/Double                              COMPLETE
  Phase 2 (R17)     Object Field Access (getfield/putfield)       COMPLETE
  Phase 3 (R18)     LICM (loop-invariant code motion)             COMPLETE
  Phase 4 (R19-20)  VM Context + Compact Refs                     COMPLETE
  Phase 5 (R21-25)  BCE + invokevirtual + OSR + SIMD + SoA        COMPLETE
  Phase 6 (R26)     Loop unrolling + speculative BCE + regalloc    COMPLETE
  -----------------------------------------------------------------------
  March 2026 result: 1.50x vs JDK C2 (Fibonacci 1.31x)            HISTORICAL

  Post-R26 (2026-07, ongoing)  Tiered compilation (C1/C2,
    background pipeline), IR-optimizing backend, real deopt +
    guard-surviving scalar replacement, guarded-inline getfield,
    AArch64 backend, ~40 call-site intrinsics, BC crypto/math
    JIT eligibility, precise JIT stack maps                       ONGOING
  -----------------------------------------------------------------------
  Current focus: allocation + young-GC throughput is now the dominant
    cost on allocation-heavy workloads (e.g. binarytrees) — see
    "Performance — current status" above.                         CURRENT
```

### Timeline

```
  Phase 1    Phase 2    Phase 3    Phase 4         Phase 5     Phase 6
  R15-16     R17        R18        R19-20          R21-25      R26
  ------     ------     --------   --------        --------    --------
  1.93x      fields     LICM       1.8x            1.0x        1.50x
  SSE done   done       done       compact refs    SoA+SIMD    unroll+BCE

  R8   R10   R14   R16   R18   R20   R22   R24   R25   R26
  JIT  regs  regs  SSE   LICM  comp  virt  SIMD  SoA   unroll
  97x  0.3x  1.9x  1.9x  ~2x  1.8x  1.8x  ~1.2x 1.0x  1.50x
```

---

## Key Metrics Summary

| Metric | Value |
|--------|-------|
| Total Rust LoC | **~1,290,000** (20 workspace members, measured 2026-07-25) |
| JIT LoC (jit crate + vm/src/jit) | **~116,600** (`jit/src` 88,720 + `jit/tests` 8,447 + `vm/src/jit` 19,448) |
| JIT backends | x86-64 (single-pass + IR-optimizing), AArch64 |
| JIT bytecodes | ~130 core opcodes + ~40 call-site intrinsics |
| JIT unit/integration tests | ~hundreds in `x64.rs` + differential/IR-vs-singlepass/intrinsic suites in `jit/tests/` |
| Test corpus | Large Rust/Java unit, integration, regression, difftest, and fuzz layers |
| Lint status | `clippy -D warnings` is a release gate, not a baked-in metric |
| Native methods | **~3,100+** |
| Optimization rounds (historical, through March 2026) | **26** |
| Total speedup (small, historical R26) | **253x** (5064ms → 20ms) |
| vs JDK -Xint (historical R26) | **~28x FASTER** |
| vs JDK C2 | Historical March 2026: **1.50x QuickBench (Fibonacci 1.31x)**; historical 2026-07-02 (pre-OSR-flip): **46.2x default / 8.25x OSR+threshold**; historical 2026-07-08: **3.7x default**; current: not yet refreshed post-tiering/IR-backend/deopt work — see "Performance — current status" |
