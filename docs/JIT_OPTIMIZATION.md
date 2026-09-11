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

### Register residency in the optimizing tier — built, verified, still opt-in

`ir_lower` keeps **every** value in a frame word. Its GP tier is fixed
(RAX/RCX/RDX for values, R10/R11 for safepoint and shadow-stack work, R8/R9 for
call arguments), `frame_word_off` returns `Err` for `ValueLoc::Reg`, and the
linear-scan allocator that exists and self-verifies
(`regalloc::allocate_linear_scan` plus `verify_allocation`) was wired only as a
**write-through read cache over XMM2–XMM7**, behind
`CRATONVM_JIT_IR_LINEAR_SCAN`, default off. Its own doc comment stated the
consequence: *"this wiring is still FP-only. An `int` loop counter gets nothing
out of it."*

The single-pass backend, meanwhile, colours Java locals into callee-saved GPRs
(`LOCAL_REGS` = RBX/R12–R15, plus RSI/RDI on Windows) by default. So the
baseline tier keeps loop counters and accumulators in registers and the
optimizing tier that supersedes it does not.

**That inversion is measured, in one binary.** `CRATONVM_JIT_IR_LONG=0` declines
any method using `long`, which routes a `long`-accumulating kernel to the
single-pass backend and changes nothing else. Five one-line kernels, 20.5M
iterations each, identical checksums on every arm:

| kernel (inner loop body) | C2 / IR | C1 / single-pass | C1 advantage |
|---|---:|---:|---:|
| `s += a[i]` over `int[]` | 84–90 ms | 25–28 ms | **3.2x** |
| `N n = a[i]; if (n != null)` | 91–103 ms | 31–34 ms | **3.0x** |
| `s += a[i].v` | 93–102 ms | 34–37 ms | **2.7x** |
| `a[i].v = i` | 165–178 ms | 119–129 ms | 1.4x |
| `a[i].next = a[i]` | 347–361 ms | 284–304 ms | ~1.2x |

(Linux/EPYC, load 5–12, three rounds. An earlier Windows run at load ~0.6 put
the first row at 1.6x; the direction is the same and the size is host-dependent.)
The last two rows are the control that makes the rest readable: they are
dominated by out-of-line barrier work, so register residency cannot move them,
and it does not. `jit/src/x64/single_pass_only.rs` treats this inversion as a
finite, enumerable list of single-pass specialisations to veto on. It is not
finite.

**What landed.** `regalloc::xmm_roles::IR_GP_LINEAR_SCAN` = RBX, R12–R15 — a
general-purpose file beside the XMM one, on the same write-through contract.

Every part of the register choice is forced. They must be **callee-saved on the
target ABI**, which this wiring needs because it has no reload machinery: a
value's register must survive a call by the calling convention rather than by
analysis, and that rules out every caller-saved register. They are untouched by
this emitter's own tiers. The prologue saves them and every exit restores them
(`IR_GP_PROLOGUE_SAVED`), on the same footing as the XMM save area and just as
dynamically — a method that promotes nothing emits no save.

> **Corrected 2026-09-10.** This paragraph used to read "callee-saved on
> **both** ABIs […] and that rules out even the otherwise-obvious System V
> candidates RSI/RDI" — a System V fact stated as an ABI-independent one.
> **Win64 makes RSI and RDI callee-saved**, and the single-pass backend has
> been colouring locals into them all along (`x64::LOCAL_REGS` is `[u8; 7]` on
> Windows and `[u8; 5]` elsewhere). The IR file now widens to seven there
> behind `CRATONVM_JIT_IR_GP_WIDE`, which is **default OFF because it was
> measured slower**, not because it is unsoaked:
> `docs/internal/performance/c2-the-gp-register-file-is-not-the-binding-constraint-20260910.md`.
> The reason it is slower is the next paragraph but one — write-through means a
> wider file buys publishes, not fewer stores.

**The safepoint obligation is discharged by type, not by structure.** A GC root
walk reads a frame it did not stop, through RBP, and `OopMapEntry` names frame
slots only, so no reference may be register-resident at a safepoint. The XMM
file discharged that by having no register a `Ref` could occupy; the GP file has
to refuse the type, which `plan_register_residency`'s bank match does. Everything
else is unchanged by construction: the home word is written at every definition,
so `emit_safepoint_map`, `build_deopt_points` and `emit_phi_copies` read exactly
what they always did.

**The default did not move, and the census is why.** With
`CRATONVM_DBG_IR_LINEAR_SCAN=1`:

* on `BinTrees.itemCheck` the file works — `resident=7 (fp=0 gp=7) demoted=0`,
  `phi=0`, 13 candidates lost to splits and 2 to type;
* on all five probe kernels above it never runs at all: *"refused: liveness and
  colourer disagree about which values want a home"*, 5 of 5.

That refusal was a **pre-existing** gate, not something the GP file introduced.
`regalloc::ir_op_defines_value` (through `wants_loc`) and `ir_lower`'s
`op_defines_result_slot` (through `node_color`) are two enumerations of one
question — the second and third of the three `the_three_ir_op_enumerations`
names — and any disagreement declines the whole method. It was declining the XMM
cache the same way and nobody could see it, because the flag printed only on
success. Both halves of that are fixed: every refusal now carries a reason, the
enumeration disagreement names the offending node and op, and a successful plan
reports its per-cause skip census (`split_or_spilled`, `wrong_bank_or_type`,
`no_home`, `phi`).

**The disagreement itself is fixed too, and it was exactly two ops.**
`op_defines_result_slot` lists `Op::ArrayLength` and `Op::NewArray`;
`ir_op_defines_value` did not — while its own doc comment asserted lockstep and
named those two as *"absent there and absent here"*. The comment written to
prevent the drift was the drift. Every counted loop spelled
`for (i = 0; i < a.length; i++)` contains an `arraylength`, which is why the
five array-touching kernels declined and `BinTrees.itemCheck`, which touches no
array, was the one that promoted. Nothing was ever miscompiled — the agreement
check did its job and declined — but the optimization was unavailable wherever
arrays are, which is most places.

The lockstep claim is now enforced rather than asserted:
`the_two_value_defining_enumerations_agree` parses both function bodies out of
the two source files and compares the sets, so adding an arm to one list and
forgetting the other fails a test instead of surfacing as an unexplained
refusal months later.

So the capability is built, tested, safe, and no longer structurally blocked.
The flip is a separate decision that still wants a **measurement on a quiet
host**, which is what this work could not supply: the box ran at load 30–352
throughout, with the fat-LTO link of the verifying binary SIGKILLed by the OOM
killer at load 352. **Do not flip it on the strength of the table above; that
table is the problem statement, not a result.**

Off is exactly the pre-change emission: no register handed out, no save area
reserved, every read from its home word.

Two couplings are worth knowing. Residency and the **level-2 selector** are
mutually exclusive — `isel`'s encoder is anchored byte-for-byte against the
per-opcode arms under the assumption that the frame-homed allocation *is* this
backend's allocation, which residency makes false — so a MIR mode turns
residency off. And the IR tier still publishes no OSR entry table; the assertion
that would catch an OSR trampoline entering past a prologue that saves registers
now covers the GP band too.

**Known limit, named rather than guessed at:** a loop counter and a loop
accumulator are `Op::Phi` at the loop header, and phis are excluded — the
allocator refuses them, and their homes are written by `emit_phi_copies` at each
incoming edge rather than by a definition arm, so there is no site that could
publish one into a register. Reaching loop-carried values therefore needs the
allocator to admit phis and `emit_phi_copies` to publish; the `phi=` field of
the skip census is there to say how much that is worth on a given workload
before anyone builds it.

### Reference stores: barrier gates instead of a region table

Under the default collector, **every reference `putfield` in compiled code was
an out-of-line call**, and the inline fast path guarding it was dead code that
still cost about twenty-five instructions.

The fast path was gated on `region_bounds_are_live(region_bounds_addr)` — the
*contents* of the process-global `JIT_REGION_BOUNDS` table. ZGC (the default
since 2026-08-10) and G1 both deliberately never publish into it; `zgc.rs` says
so outright: filling it "would re-enable an inline reference STORE fast path
this collector must not have". Worse, the emitter's admission test was
`helpers.region_bounds_addr != 0` — the *address* of a static, hence a constant
`true` — so the whole sequence was emitted (null test, alignment test, six
containment compares that could never pass, a compactness test, an old-gen test)
and then fell through to `jit_putfield_object` anyway.

The prediction that follows is falsifiable and was checked:
`CRATONVM_NO_JIT_INLINE_PUTFIELD=1` must measure exactly zero. On BinTrees d=16
it did — 3546/3541 ms on against 3525/3786 ms off, checksum `14985902` on all
four runs. **A kill switch that cannot change a number is not a lever.**

**What landed is not a re-run at the same question.** The old guard asked "is
the receiver in a published young region", which is a *generational* question G1
and ZGC do not answer. The collector now publishes three bytes
(`gc::gen_heap::JIT_REF_STORE_GATES`, reached through
`JitRuntimeHelpers::ref_store_pre_gate` / `_post_gate` / `_post_young_floor`),
and each names a **prefix of a barrier helper's own control flow**:

| inline test | the helper's own first act |
|---|---|
| `pre_active == 0` | `satb_pre_barrier` loads `mark_active` and returns |
| `flags_byte < young_floor` | `note_ref_store_slow` compares `gc_age` to the promotion age and returns |
| `post_active == 0` | `note_ref_store` loads `has_old_objects` and returns |

So a skipped call is one that would have returned having done nothing. On any
other answer the sequence calls the collector's **own** `write_barrier` — no
remembered-set contract moves into the emitter, which is the mistake the
previous inline store path made and the reason `inline_card_mark_available` is
hard-`false`.

Two properties make reading these inline safe. Each gate may be conservative but
never permissive: publishers raise a mirror *before* the state it mirrors and
lower it *after*, so it can only ever say "there may be work" when there is
none. And the SATB flag is armed only inside the mark-start pause
(`start_concurrent_mark` takes a `StopTheWorldToken`), so no mutator can sit
between its inline test and its store while the flag flips. The young floor is
pinned at `gc_age == 0` — the bound that needs no ordering argument at all,
since the promotion age is clamped to at least 1 — and that is the case that
matters, because in allocation-heavy code the receiver of a reference store is
overwhelmingly an object allocated moments earlier.

The gated path also drops the condition that the field's **old value be null**,
which is what used to send every re-assignment of an already-set reference to
the helper. With `pre_active` read directly, the old value stops mattering.

`CRATONVM_JIT_GATED_REF_STORE=0` restores the helper path. A collector that
publishes no plan (all three addresses zero) gets the previous emission byte for
byte, which is what G1 and Generational get today.
`CRATONVM_DBG=jit-method-stats` prints `compiled reference stores: gated=N
declined=M` — both numbers always, because a zero on the left alone cannot
distinguish "no plan published" from "this workload compiles no reference
stores". On BinTrees d=16 under the default collector it reads `gated=2
declined=0`, which is the engagement evidence the switch it replaces could not
produce.

**The throughput result is NEUTRAL, and it is stated here rather than left to
be inferred from the mechanism.** Wall clock was unusable — the host ran at load
20–63 with four other sessions' VMs on it, and a BinTrees arm swung 1600–5000
ms — so the arms were priced in **CPU time**, which is what this host's own
methodology calls for. Eight pairs, alternated with the order flipped on
alternate pairs, `-Xmx4g`, BinTrees d=16:

| arm | user CPU (s), 8 runs | median |
|---|---|---:|
| gated ON | 2.16 2.17 2.16 2.20 2.16 2.20 2.13 2.17 | 2.165 |
| gated OFF | 2.14 2.19 2.13 2.12 2.21 2.21 2.14 2.17 | 2.165 |

Identical. (System CPU ranged 0.83–2.11 on both arms — GC and page-fault noise,
not attributable to either.)

Two things explain that without contradicting the change. The census reads
`gated=2 declined=0`: only two compiled sites in this workload take the
sequence at all, so the sample is small. And BinTrees builds a tree that
survives, so `has_old_objects` arms early and the surviving path still calls
`write_barrier` — the young-receiver floor is what would elide it, and it
covers only `gc_age == 0`.

So what is established is engagement, correctness and the emitted sequence — a
call plus six compares that could never pass, replaced by three byte tests —
and what is **not** established is a throughput win on any workload measured so
far. It is kept on because the removed compares are provably dead code and the
off arm is a supported configuration, not because a number says so. A
call-denser workload on a quiet host is the measurement that would settle it.

### Call sites: argument staging moved to the cold path

Both `emit_direct_cross_call` and `emit_inline_cache_call` opened by copying
every outgoing argument into a contiguous staging region, then loaded the same
frame slots again to marshal them into ABI registers — 3N memory operations per
call where N does. The only reader of that region is the callee-deopt service,
reached when a callee returns the deopt sentinel and otherwise never.

The staging now happens on each reader's own cold side: inside
`emit_inline_callee_deopt_service` past its `JNE .done`, and immediately before
the shared hashed/vtable stub in the megamorphic region rather than ahead of the
monomorphic guard. The resolving slow path already re-staged for itself, which
is what made the copy at the top redundant even before this. The values are read
out of the same frame slots in all three places, and nothing between the marshal
and any of them writes those slots.

**Known residual, not fixed here.** Because `needs_context` is checked as a
runtime property of the cached entry, the argument marshalling is emitted twice
per cache entry — ten copies at a site with one MIC and a four-entry PIC. That
is a code-*size* cost rather than a per-execution one (each execution runs
exactly one copy), and the clean fix is a uniform entry ABI, which changes how
every compiled method receives its arguments. `needs_context` is an output of
optimization and it moves the ABI; that is not a change to stack on top of a
register-file change in the same pass.

### Displacement widths

Both backends hard-coded the disp32 ModRM form at sites where a disp8 is legal,
each with a smallest-form encoder sitting next to the site that did not call it.
`ir_lower`'s frame accessors (`load_reg_from_frame`, `lea_reg_from_frame`,
`fp_load`, `fp_store`, `emit_xmm_frame_move`) now share one
`emit_rbp_modrm_disp`, which is where the RBP-has-no-`mod=00` rule lives — three
bytes on the instruction class that dominates every IR body. The guarded
receiver check reads six table words at displacements 0..40 through RDX, every
one of them a disp8, and paid disp32 on each: 18 bytes per unproven-receiver
field access.

### The operand-stack register cache, and why it is still pure-kernel-only

`push_from_rax` parks a pushed value in a scratch register instead of storing it
to the frame. It is gated on `pure_kernel` — no invokes, no MIC/PIC or indy
sites, no field or static-field ops, no allocation, no typechecks, no inline
sites, no speculative BCE guards — so one `getfield` anywhere in a method turns
it off for the whole method and it never engages on application code.

The comment at that gate blamed "the broad R8/R9 experiment regressed call-heavy
methods because each call flushed live scratch values", which reads as a cost
argument and is not one: a flush emits the store the frame push would have
emitted anyway, only later.

**The real blocker is a register collision.** `SCRATCH_REGS` is `[R8, R9]` and
`ARG_REGS` is `[RCX, RDX, R8, R9]` on Win64, `[RDI, RSI, RDX, RCX, R8, R9]` on
System V — R8 and R9 are argument registers on both. Every helper call
marshalling three arguments writes R8; four writes R9. Only sites that call
`flush_scratch_registers` first are safe, and the emitter has far more
`emit_call_absolute` sites than flush sites. Under `pure_kernel` none of them is
reachable, which is why the collision has never mattered. Turning the cache on
broadly without that audit produces wrong code, measurably: with it default-on,
`test_compile_fib` returned 20 for `fib(10)` and
`test_getfield_putfield_roundtrip` returned garbage.

Two things did change. `CRATONVM_JIT_OPERAND_CACHE=1` exists so the two arms can
be measured in one binary, which the previous shape could not be. And a real
defect underneath was fixed unconditionally: `StackSlot::Scratch` now carries
the home word its push reserved and `flush_scratch_registers` stores into that
instead of reserving another, so a straight-line stretch with several calls no
longer grows the spill region once per call until `spill-range-exhausted` fails
the compile.

### Allocation in the optimizing tier — the bump landed; the gate is now a policy question

This section used to say the optimizing tier lowered every `new` through
`emit_new_object_stub` — three register loads and a `CALL` — while the
single-pass backend had `emit_inline_tlab_new`, so an escaping allocation
compiled *worse* after escape analysis had run on it. That is fixed:
`runtime_lowering::emit_inline_tlab_new_ir` gives this tier the cursor load,
the bump, the limit compare and the inline header writes, with the old stub as
its slow path. It lives beside the stub because that module exists to be the
one place both front ends share allocation, dispatch and monitor contracts.

Three things in it are load-bearing and none of them is a copy of the
single-pass sequence:

* **The size comes from `class_layout`, not from arithmetic.**
  `jit_post_tlab_init` derives the object's shape and total size from
  `class_layout(class_id)` itself, so a caller that sizes the allocation as
  `HEADER_SIZE + num_fields * SLOT_SIZE` while the class carries a registered
  compact layout hands the helper a size mismatch and corrupts the heap. The
  snapshot is taken from the same two functions the helper and the single-pass
  emitter read, so all three agree by construction rather than by inspection.
* **A layout can be replaced between compile and execution**, which is what the
  guard emitted first is for: it compares the live field count against the one
  this compile baked and diverts to the helper on a mismatch, before any state
  exists to unwind.
* **Every header write lands before the cursor commits**, and the mark word is
  written unconditionally. Publishing the cursor first exposes an object whose
  header is still whatever the TLAB slot held — `class_id = 0` to the GC
  walker, which then mis-decodes it and steps into its neighbour. The mark word
  stopped being padding when the 24 → 16 byte shrink folded `kind`,
  `element_type`, `gc_age` and `gc_flags` into it; skipping it is the
  2026-08-07 Spring Boot regression (`read_slot: corrupt Value cell` in 178 of
  184 classes).

Measured: the site census reads `inline-bump=2 stub-only=0` with the path on
and `0 / 2` with `CRATONVM_JIT_IR_INLINE_TLAB=0`, the Binary Trees checksum is
`674478` on every arm and matches HotSpot, and the regression suite is 85/85
with `CRATONVM_JIT_C2_ALLOC_UPGRADE=1` forcing the new path.

`CRATONVM_JIT_C2_ALLOC_UPGRADE` is still opt-in, and `IR_MAX_ALLOCATIONS` is
still 16. That is now a *population* question rather than a codegen one —
flipping it moves every allocation-bearing method to a different tier, which is
a change that wants its own measurement — but the reason the gate could not be
opened at all is gone.

### The receiver null check nobody could prove away

`getfield` on `this` re-tested `this` for null on every execution, including
every iteration of a loop, for a value the JVM guarantees at the call site.
Two independent gaps produced that, and both had to be closed:

**The dataflow could not represent the fact.** `null_check_elim::analyze` seeded
entry IN to `0` — nothing proven on entry — so a local became known non-null
only by being dereferenced. That is enough for straight-line code and useless
for a loop: the IN mask at a loop header is the meet of the entry path and the
backedge, the backedge carries "local 0 non-null" because the body's own
`getfield` proved it, the entry path carries nothing, and the intersection is
empty. The fact died at the header on every iteration.

**The getfield emitter never asked.** `emit_trusted_oop_receiver_check` emitted
`TEST`/`JZ` unconditionally, while the array sites next door had consulted
`is_local_nonnull` since round 11. The proof existed and had no consumer.

`CRATONVM_JIT_THIS_NONNULL` seeds bit 0 when local 0 holds a receiver;
`CRATONVM_JIT_RECEIVER_NULL_ELIM` lets the two `getfield` arms consult the
result. They are separate switches because the blast radii differ — the seed
widens a fact three consumers already read (inline array null-check elision,
`ifnull`/`ifnonnull` branch elision, and the new one), while the consumer flag
only adds the third — and one switch for both would have made them
indistinguishable in a bisect.

Instance-ness is **derived, and the derivation refuses rather than guesses**.
`compile_with_param_slots` takes no `is_static`, but `method_key` already
carries the descriptor and `num_params` is the argument count with `this`
included, so the answer is which of `declared` / `declared + 1` the count
equals. "Neither" answers `None`. Note the width convention: `count_param_slots`
counts `J` and `D` as **one** argument each, unlike `compute_param_jvm_slots`,
which counts them as two slots — reading the wrong one misclassifies every
method with a `long` or `double` parameter.

Scoped to `getfield` and deliberately to nothing else. `putfield`'s preceding
push is the stored **value**, not the receiver — the exact shape of the Tomcat
`MessageBytes.setString` miscompile that `opcode_dereferences_receiver` already
documents — and `checkcast` does not throw on a null receiver at all, so its
`JZ` targets a legal null path; eliding it would let a null fall into the
`KIND_TAGS` byte compare and fault.

#### Measured: it engages, and it does not show up in the clock

Probe: `for (i = 0; i < 2000; i++) s += this.x;`, called 40,000 times — 80M
executions of the elided check. Debug binary, Azure `vm1`, a shared host that
was also running two fat-LTO release builds and an H2 suite.

| Arm | census | checksum |
|---|---|---|
| both on | `elided=2 emitted=0` | 240000000 |
| `CRATONVM_JIT_RECEIVER_NULL_ELIM=0` | `elided=0 emitted=2` | 240000000 |
| `CRATONVM_JIT_THIS_NONNULL=0` | `elided=0 emitted=2` | 240000000 |

Both halves are load-bearing and independently switchable — turning off either
one returns the count to zero — and the answer matches HotSpot on every arm.

Throughput, five interleaved reps per arm, CPU time (`%U + %S`; wall clock is
meaningless on that host): medians **3.30 s on and 3.30 s off**, ranges
3.01–3.33 and 3.13–3.39. **No detectable difference.** The loop does run
compiled — the `--nojit` arm was still going after two minutes against 3.3
seconds — so this is a measurement of compiled code, not of the interpreter.

That is the expected result and it is worth stating rather than filing away:
`TEST r,r; JZ rel32` is nine bytes and two well-predicted µops, and an
out-of-order core hides them behind the load they guard. What the elision buys
is **code size**, paid entirely at compile time, plus the fact that a check
that is not emitted cannot be got wrong.

Eliding by proof is strictly better than faulting, wherever the proof exists.
Where it does not, there is the implicit null check.

### The implicit null check — the receiver dereference is the check

`CRATONVM_JIT_IMPLICIT_NULL_CHECK=1`, **default OFF**.

Where the dataflow proves nothing, the compact `getfield` arm can drop
`TEST RAX, RAX; JZ slow` anyway and let the receiver dereference that follows
it fault. The signal handler translates that fault back into the arm's own slow
path, which calls the helper that raises the `NullPointerException`. The load
that would have been guarded *is* the guard.

This is the HotSpot mechanism, and it is the one item of the JIT audit that did
not land with the rest of its round. The reason was never the signal handler —
it was lifetime, and it is worth writing down what each of the three hazards
actually needed.

**Signal safety.** The lookup runs inside the handler, so it cannot use the
mutex `lookup_jit_method_name` uses. That function is only ever reached while
the process is already dying, which is what makes a `try_lock` acceptable
there; here the process is expected to *survive*, and a handler that blocks on
a lock its own interrupted thread holds deadlocks. The table is a fixed array
of atomics and the reader does nothing but loads — no allocation, no lock, and
no call into anything that takes one.

**Lifetime — the hazard that actually blocked it.** A `CompiledMethod`'s buffer
is unmapped on drop and, in that function's own words, "the address is then
reusable by the next `alloc_executable`". An entry that outlived its buffer
would eventually match a PC belonging to *different* code, and the handler
would resume execution at a stale address inside a live method. That is not a
crash; it is silent, arbitrary control flow. Two things close it: `Drop` calls
`implicit_null::unregister_range` beside the `unregister_jit_method_name` that
exists for exactly the same reason, and **slots are never reused** — retiring
stores `0` and leaks the slot, because reuse would let a reader that has
already matched `fault_pc` read a `recover_pc` that a concurrent
re-registration had since overwritten. Exhaustion *declines*: the site keeps
its explicit check and `declined` counts it, so the feature turns itself off
rather than turning unsound.

**Mis-recovery.** A genuine backend bug also faults inside compiled code, and
silently resuming from one would convert a diagnosable crash into corrupted
state. Recovery requires all of: a memory-access fault; an `si_code` saying
`si_addr` is an address at all rather than a union member left over from a
`kill -SEGV`; a faulting address inside the **null page**; and an **exact**
registered PC, not merely one inside some compiled method's range. The last two
are what separate "a null receiver reached a load we chose not to guard" from
"compiled code dereferenced garbage" — a wild pointer does not land in the
first page.

#### Fail-closed, twice, because the elision is far from the thing it depends on

The compiler does not trust its own source. `bind_implicit_null_recovery`
decodes the bytes at the site it declined to guard and requires
`MOV r32, [RAX + disp32]` with `disp32` inside the same null-page constant the
handler screens on — so the two agree by construction rather than by two people
remembering the same number. A second backstop fails any compile that reaches
the end with a site still unbound. Both discard the artifact and return the
method to the interpreter.

That is more machinery than the elision itself, and deliberately so: the
elision happens in one function and the property it depends on — that the next
instruction dereferences the receiver, and that the slow path is reached —
lives several hundred lines away in the arm that called it. An edit that broke
the coupling would not produce a red test, it would produce a crash on a null
receiver in production.

#### Both arms opt in now, and the second one has bought nothing yet

The legacy-cell `getfield` arm passed `false` at first, because it emits its
`GC_FLAGS` read only under `compact_ref_fields_enabled()`. It can now make the
guarantee exactly: `raw_mode` is already false in that branch, so the guard's
`!raw_mode && compact` reduces to `compact`, and the opt-in is the *same*
expression rather than a second one that has to be kept in step. The arm binds
its recovery address where its guarded slow path begins, and that slow path
reloads the receiver from its frame slot, so a recovered fault needs no
register repair.

**It changed no number.** On the H2 workload the census reads
`implicit=285 (compact-arm=285 legacy-arm=0)`; on a purpose-built megamorphic
probe reading a public field off a JDK class, `compact-arm=8 legacy-arm=0`. The
legacy arm is 13 sites against the compact arm's 1,705 on H2, and all 13 had
receivers the dataflow already proved.

So the widening rests on "it could carry sites no proof reaches", not on a
measurement that it does. That is recorded rather than smoothed over, and the
census is split by arm precisely so the claim is falsifiable: **if `legacy-arm`
stays 0 across real workloads, this widening is dead and should be withdrawn.**

It is not the same as unreachable code — the arm does fire, 13 times on H2.
What is unproven is that it ever fires with a receiver no proof covers, and
that is a property of workloads rather than of the code, which is why it gets a
counter instead of an argument.

#### Measured

An 800,000-call probe whose receiver is a *parameter* (so the dataflow proves
nothing and the implicit path is the one taken), with five null calls in the
middle:

| Arm | census | answer |
|---|---|---|
| default (off) | `implicit=0 emitted=1`, `registered=0 recovered=0` | `sum=5600000 caught=5` |
| `=1` | `implicit=1 emitted=0`, `registered=1 retired=1 recovered=5` | `sum=5600000 caught=5` |

`recovered=5` is the whole feature in one number: five hardware faults, five
exact-PC matches, five `RIP` redirects, five `NullPointerException`s. The
400,000 iterations *after* the faults still sum correctly, so recovery does not
leave the frame damaged, and `retired=1` shows the entry withdrawn when the
artifact dropped. HotSpot returns the same two numbers.

Note the second row needs `CRATONVM_C2_SUPERSEDE=0` to be reached at all: with
the default policy the method tiers up before the null calls, the C1 artifact
is dropped, and the optimizing tier's own explicit check handles them. That is
worth knowing before reading a `recovered=0` as a broken feature — it is more
often a measurement of which tier owned the method.

**Throughput is unchanged.** A 120-million-call probe reading a field off a
parameter, five interleaved reps of CPU time, `CRATONVM_C2_SUPERSEDE=0` so the
arm under test is the one that runs: medians **5.68 s on and 5.55 s off**,
ranges 5.22-5.78 and 5.12-5.87. Read that as no detectable difference rather
than as a regression -- the fast path with the flag on is the fast path with it
off minus two instructions, so it cannot actually be slower, and the overlap is
the host.

Which is exactly what the elision A/B above predicted: an implicit check
removes the same `TEST`/`JZ` pair the proof-based elision removes, and that
pair did not move the clock either. **The reason to have this is not speed.**
It is that the sites where no proof exists are precisely the ones the elision
cannot reach, and this is the only thing that covers them -- and that having it
built, measured and switchable is worth more than an argument about whether it
would have helped.

#### The soak, 2026-09-02, and the default

It was off pending a soak. The soak ran, it is clean, and it also produced the
number that argues against flipping the default. Both halves are recorded
because the second one is the useful one.

**Correctness.** Roughly an hour of continuous execution plus two full
regression-suite passes, all with `CRATONVM_JIT_IMPLICIT_NULL_CHECK=1`:

| Arm | census | answer |
|---|---|---|
| 5M iterations, default tiering, `--Xmx 256m` | `registered=8 retired=8 recovered=0` | `sum=187500000 npes=19532` ✓ |
| 3M iterations, `CRATONVM_C2_SUPERSEDE=0` | `registered=8 retired=8` **`recovered=11663`** | `sum=112500000 npes=11719` ✓ |
| 5M iterations, flag OFF (control) | — | `sum=187500000 npes=19532` ✓ |
| regression suite × 2 | — | **87 passed, 0 failed** each |

The middle row is the one that exercises the mechanism: **11,663 of 11,719 null
dereferences were hardware faults translated into `NullPointerException`s**,
under GC pressure, with the checksum matching HotSpot exactly. The 56 that were
not recovered are the ones taken before the method compiled. Every arm exited
`rc=0`. CPU time was 962 s with the flag on against 1001 s off — read as
identical on a shared host, not as a win.

**Reach — and the correction that matters.** This section first estimated the
reach from short suite vectors and synthetic probes, and concluded it was "a
real set, and a small one". **That was wrong, and it was wrong for a reason
worth keeping:** the probes were too small to compile much, so they measured
the JIT's warm-up threshold rather than the feature's reach.

* `RMapGcStress`, `RJitGc`, `RStringOps` read `elided=0 implicit=0 emitted=0`
  with `CALL sites emitted by arm:` **empty**. That empty field is the tell,
  and it was in the output all along: the arm was not declining, the vectors
  compile no `getfield` in this tier *at all*. A census of a workload that
  compiles nothing measures nothing.
* The soak probe had to be built megamorphic to provoke 8 sites, which said
  more about the probe than about the feature.

Pointed at a **real application** — the H2 engine, 60,000 batched inserts and
20 sorted full scans over a 3-column table — the same census reads:

| Arm | elided | implicit | emitted |
|---|---|---|---|
| default (all on) | 1425 | 288 | **0** |
| `CRATONVM_JIT_IMPLICIT_NULL_CHECK=0` | 1425 | 0 | 288 |
| `CRATONVM_JIT_THIS_NONNULL=0` | 1000 | 713 | 4 |
| all three off (the old behaviour) | 0 | 0 | **1715** |

Every arm returns the identical answer, matching HotSpot.

So the real numbers are: **1,715 receiver null checks on this workload before
any of this work, and 0 after it.** The `this` seed accounts for 425 of the
elisions on its own (1425 → 1000 when it is switched off). The implicit check
covers 288 sites that no proof reaches — and the two are complementary rather
than redundant: with the seed off, 713 sites fall through to the implicit path
instead, and only 4 end up with neither.

`recovered=0` on this run, because correct code does not dereference null. That
is the expected steady state: the implicit check costs nothing until a null
arrives, and then it costs a fault instead of a branch.

**The default is ON**, since 2026-09-02. Opt out with
`CRATONVM_JIT_IMPLICIT_NULL_CHECK=0`.

The engineering recommendation at the end of the soak was to leave it off, and
it is worth recording that it was overruled deliberately rather than forgotten.
The case for off was never correctness — the soak settles that — it was that
none of the three things a default usually rests on were present: the
throughput effect is unmeasurable, the reach looked like a handful of sites,
and the failure mode is the only *silent* one in this backend. **The middle
term was wrong** — measured on the H2 engine rather than on probes, this work
removes 1,715 receiver null checks and leaves zero, 288 of them reachable only
by the implicit path (see the reach table above). The recommendation to leave
it off was made on synthetic evidence and does not survive the real
measurement; the decision to default it on does. The case for on is that
the mechanism is the one thing covering the sites the proof-based elision
cannot reach, it has soaked clean across three full suite passes and ~12,000
translated faults, and a feature that is only ever exercised behind an opt-in
flag is a feature that decays.

Both readings are defensible. What matters more than which one won is that the
**kill switch stays**, and that anyone debugging an unexplained crash in
compiled code knows to reach for it first: `=0` restores
`emit_trusted_oop_receiver_check` at both arms unconditionally, registers
nothing, and returns a fault in compiled code to the crash reporter exactly as
before this existed. Same binary, one run, no rebuild. That is the property
that makes a silent failure mode survivable, and it is worth more here than it
is anywhere else in this file.

#### The two things the soak left untested, and what closed them

The soak above ran single-threaded and saw 8 `CompiledMethod` drops per run.
That left the lock-free table untested under **concurrent** faults, and the
lifetime path barely exercised. Both were closed before the default moved,
with `CRATONVM_JIT_THRESHOLD=1` (so every method compiles immediately, which is
what actually produces drop churn) and 128 reader classes behind an interface:

| Arm | census | answer |
|---|---|---|
| 8 threads, no supersede, ON | `registered=23 retired=23` **`recovered=7433`** | `sum=61988608 npes=12504` ✓ |
| 8 threads, same, OFF (control) | `emitted=23 registered=0` | `sum=61988608 npes=12504` ✓ |
| 8 threads, supersede ON, 200k iters | `registered=23 retired=23 recovered=1` | `sum=123985408 npes=25000` ✓ |

**7,433 faults recovered across eight threads at once**, with the checksum
matching HotSpot exactly on every arm and `rc=0` throughout. That is the
lock-free table doing concurrent reads against concurrent registration and
retirement, which is the shape nothing else had exercised.

`registered` equalled `retired` in every arm, in every run, at every scale —
8/8, 15/15, 23/23. The table does not leak entries, which is the accounting
half of the lifetime argument.

**The correctness half rests on an invariant worth naming**, because it is
easy to break and nothing else would notice. `CompiledMethod::drop` retires
`[entry, entry + buffer.pos())`, and registration keys sites off
`cm.entry + fault_off`. Those two agree only because the method entry *is* the
buffer base — `driver.rs` says so in as many words (`let entry_offset = 0; //
prologue starts at offset 0`), and the OSR-trampoline purge in that same `Drop`
already depends on it. Give the prologue a non-zero offset and every site below
the new entry silently stops being retired, which is precisely the stale-entry
hazard the whole design exists to prevent.

**What is still not stressed**, for whoever revisits this: there is no
production eviction path to drive drops harder than tier-up does.
`jit_code_cache_cap_reached` *refuses new compiles* rather than evicting, and
`CachedMethods::evict_least_used` is called from tests only. So drops come from
supersede and deopt, and 23 per process is what a compile-everything workload
produces. Hammering address reuse beyond that needs a deopt storm.

### The 2026-09-02 eight-finding pass — what moved, and what did not

An audit of where the JIT loses to HotSpot produced eight findings; all eight
are addressed above. What follows is the measurement, and the residuals that
survive it, stated rather than left to be inferred.

**Method.** ONE binary, every new default flipped off against every one on,
arms alternated with the order reversed on alternate reps, `-Xmx4g`,
`bench/CratonBench.java` one phase per fresh process. Windows workstation, not
the Azure bench host, so these are NOT comparable with `BENCHMARK.md`'s
HotSpot ratios and are not written there. The statistic is the MINIMUM of each
arm, which is the least contaminated one.

**The result is NEUTRAL on every CratonBench row, and that is the finding.**

| phase | all-off | all-on | |
|---|---:|---:|---|
| HashMap (10M put/get) | 9,115 ms | 8,749 ms | neutral |
| Binary Trees (d=18) | 13,346 ms | 13,168 ms | neutral |
| Matrix 1280² | 3,289 ms | 3,191 ms | neutral |
| Arithmetic (2B ops) | 5,768 ms | 5,804 ms | neutral |
| String/Regex (100K) | 239 ms | 251 ms | neutral |
| Fibonacci(44), Sieve | — | — | inside the noise band |

Checksums identical on every row of every arm.

**An earlier draft of this table claimed 1.62x on HashMap and 1.40x on Binary
Trees. Those numbers were real and they were not this branch's.** They came
from a VM-thread TLAB on ZGC that `feat/zgc-jit-tlab-20260902` landed in
parallel — and that feature ships OPT-IN (`CRATONVM_ZGC_JIT_TLAB=1`), because
its own author measured it slower in the general case. Enabling it in BOTH
arms attributes this branch correctly: HashMap 4,828 ms with these switches
off against 4,866 ms with them on. Neutral. The lesson is the one this
document already states about control arms — an arm that differs in two
features measures neither.

**What IS established, and it is not a throughput number.** The emitted
sequences are shorter, verified by disassembly rather than by a clock:
`BinTreesClassic.itemCheck` is 2,348 bytes against 2,437 with the switches
off, `fib`'s optimizing-tier body no longer materialises its constants into
frame words or lowers `n > 1` through a stored boolean, and a receiver is
proved once per block instead of once per field read. Those removals are real
and permanent; what this host cannot do is resolve them above a spread that
reaches 45% inside a single arm. A quiet Linux bench host is where a 3-5%
codegen change becomes measurable, and that measurement has not been taken.

Correctness, which was measured: regression suite 88/88 with every switch on,
`cargo test -p cratonvm-jit --lib` 2,178 passed, `jit-api` 56, `types` and
`gc` green.

**One finding is only half closed, and this says which half.** Finding 4 was
"allocation and reference stores in the optimizing tier are helper calls". The
reference stores are fixed here (`emit_gated_compact_ref_store`). Compiled
allocation under the default collector was fixed in parallel by
`feat/zgc-jit-tlab-20260902`, which reached `dev` first and is the
implementation that ships: `VmHeap::refill_tlab` had answered `None` on the
Zgc arm, so a thread TLAB was always empty and the inline bump both tiers emit
was dead code. This branch had built the same thing and it was dropped in
favour of theirs on the merge — including its answer to the registration
problem below, which they express as `cratonvm_types::
jit_tlab_registration_required()` rather than as a helper-ABI slot. The optimizing tier's OWN bump
(`emit_inline_tlab_new_ir`) stays opt-in: turning it on makes
`regression-suite` vector `RJitMapTierDiff` SIGSEGV 4 runs in 10, inside VM
code, on a reference read back as `0x2800`. What was ruled out: the header
writes (read out of a disassembly, they match the single-pass sequence field
for field) and relocation (4/4 with `CRATONVM_ZGC_RELOCATE=0`). One real
defect was found and fixed on the way — the reserved tail was freed twice,
once by `Tlab::retire`'s hook and once by `tlab_retire_locked`, because ZGC's
own `ZArenaTlab` contains a `Tlab` — and it is not this one.
`CRATONVM_JIT_C2_ALLOC_UPGRADE` stays opt-in with it, because with that bump
off a promoted allocation lowers through the stub's CALL again, which is the
downgrade that gate was shut for.

**Residuals, in the order they are worth taking.**

1. **The operand-stack register cache stays pure-kernel-only, and this pass
   recommends AGAINST widening it.** Its blocker is unchanged — `SCRATCH_REGS`
   is `[R8, R9]` and both are argument registers on both ABIs — and the fix
   would be a dynamic pool of callee-saved registers the local allocator did
   not hand out. But two things now argue the payoff does not justify it: the
   RELOAD half of the round-trip is already elided by `slot_mirror` (see the
   array-load page's step 7), so what remains is one STORE per push; and the
   adjacent change — reserving the home word at push time — shipped a
   nondeterministic heap corruption on 2026-09-02 and was reverted the same
   day. A one-store win is not worth a third visit to that code.
2. **Every compiled call still republishes RBP and pushes/reloads the shadow
   stack.** Those buy precise roots, not nothing, and removing them is a GC
   trade rather than a codegen one.
3. **Nothing compares a C2 body against the C1 body it replaces.**
   *(Partly closed 2026-09-10 — see the note at the end of this item.)* The
   policy question is unchanged and deliberately still open — the obvious
   static metrics both misjudge the good cases, since a bigger body is usually
   inlining or unrolling and more call sites can be a callee's own calls after
   its frame was inlined away. What this pass adds is the DATA: with
   `CRATONVM_DBG=jitc`, a supersede prints `c1=<bytes> c2=<bytes>`. What it
   also does is remove the causes that made a C2 body worse — the tier now has
   an inline TLAB bump, gated inline reference stores, and a register file.
   **What that instrument then said is below.**
   **2026-09-10, what is now closed of item 3.** The acceptance gate no longer
   judges only by *what the tier did*. `ir_evidence::CompileRecord` carries the
   per-execution cost a compile introduced beside its transform bitset, and
   `is_worth_publishing` refuses a body whose priced cost went UP however much
   it transformed. Evidence is now necessary, not sufficient.

   The prices are the two this crate already reasons in — a blind
   `jit_invoke_dispatch` resolves by name at ~175 ns against a direct `CALL`'s
   ~4 — and the trade they settle is splicing: a spliced frame saves a call, a
   call the splice strands without a bindable target costs a resolution on every
   execution. What is NOT modelled is site execution frequency, so a resolution
   on a cold branch is charged like one in a loop; that errs toward refusing,
   which is the safe direction, and it is the first thing to fix if the gate is
   ever measured refusing better bodies.

   This is still not a general C1-vs-C2 comparison. It prices one specific trade
   because that trade has measured constants; the rest of item 3 stands.
   `[c2-supersede] refused as a cost regression: bodies=N est_ns_per_execution_declined=M`
   is the reading. It came out of a case where a transform's presence was the
   evidence that published a 3x regression — `Objects.checkIndex` spliced into
   `ArrayList.get` set `Inlined`, the stranded call was native-shadowed, and the
   published body ran its probe in 897 ms against the single-pass 338. See
   `internal/performance/c2-splice-checkcast-and-instanceof-20260909.md`.

4. **`Node` is still 48 bytes against HotSpot's 24.** Unchanged, structural,
   and a GC item: see
   `known-issues/perf/perf-bintrees-9x-gap-characterised.md`.

### What the supersede diagnostic said, and the optimisation it refuted

The `c1=`/`c2=` line above was built to gather data, and the first reading off
it looked like a finding. CratonBench, nine supersedes: seven republished a
body of **exactly the same size**, an eighth (`fib`) had `c1=?`, and only
`itemCheck` changed size (1335 → 2355). Since every publish calls
`bump_jit_supersede_epoch()`, and that epoch is a **process-wide** counter every
`Jit` invoke-cache entry in every thread is measured against
(`CachedInvokeTarget::is_stale`), seven of nine looked like pure waste.

Three things came out of chasing it, two of them negative.

**The `c1=?` was not a broken lookup.** `fib` is the narrow scalar
self-recursion shape, and `promote_scalar_selfrec_to_ir` sends that straight to
the optimizing tier on its first background compile — deliberately, because
compiling it as C1 first strands recursive frames in the slower body. There was
no C1 body to find. The defect was that the diagnostic printed `?` for both "no
predecessor" and "lookup failed". It now classifies (`SupersedeOutcome`) and
prints `outcome=first-publish|unchanged|changed` with `epoch_bumped=`.

**Byte equality never fires, and cannot.** The seven same-size bodies differ in
0.116%–1.06% of their bytes (6 of 5193 for `sieve`, 379 of 35686 for
`Pattern.clazz`). They are the same code: a C2 task whose IR pipeline bails —
`[ir] ir_lower::lower_inner returned None for Pattern.clazz(Z)…` — falls back
to the single-pass backend and recompiles the same bytecode. But each compile
allocates fresh `JitInvokeInfo` boxes and embeds their addresses as absolute
immediates (`emit_mov_imm64(ARG_REGS[1], info as *const _ as i64)`), so the
bodies are equal modulo relocations and unequal as bytes. Detecting "unchanged"
would mean building a relocation table for a case that should not be created in
the first place.

**And the cost it was going to save is not there** — though not for the reason
first written here. `epoch_stale_evictions()` counts the invoke-cache entries
the epoch actually throws away. Over a whole CratonBench run: **9**. Over the
regex workload: **0**.

**That measurement does not generalize, and this document claimed it did.** On
an H2 test class (`org.h2.test.db.TestAlter`, 612 compilations) the same counter
reads **2,640** — roughly 290x the CratonBench figure, because the cost scales
with live call sites and CratonBench has almost none. The original wording,
"measured worthless: 9 IC evictions/run", was a micro-benchmark number presented
as a property of the mechanism.

The *conclusion* survives, on different evidence. On that same H2 run the
supersede census reads `first_publish=0 unchanged=0 changed=75`: every publish
replaced a genuinely different body, so every bump was owed, and
`CRATONVM_JIT_SUPERSEDE_EPOCH_SKIP_USELESS` would have skipped **none** of them.
The switch stays off because there is nothing for it to skip on a real workload,
not because skipping would be cheap.

So the suppression is implemented, correct, and **off by default**
(`CRATONVM_JIT_SUPERSEDE_EPOCH_SKIP_USELESS=1`). Two of the three outcomes
provably cannot invalidate anything — `first-publish` has no predecessor, and
the interpreter's negative "no compiled body" memo is driven by
`jit_cache_generation`, which `JitCache::put` bumps on *every* publication — but
safe and worthless is not a reason to move a default.

The residual worth having is the one this uncovered by accident: **a C2 task
that declines after entering the IR pipeline still recompiles via single-pass,
republishes an equivalent body, and pays an invalidation.** The compile is the
expensive part, not the epoch — timed at 36 ms across 6 such tasks in one
CratonBench run, against 2 ms for the 3 that produced an IR body. That residual
is now fixed; see below.

### The deferred-`new` retry was spent blind

Naming the decline routes (`[ir] IrBuilder::build refused at ir.rs:N`,
`[ir] ir_lower::lower_inner refused (<reason>)`) moved the diagnosis twice.

The residual above was written as "bails in `ir_lower`". It mostly does not.
Five of the six fall-throughs bail one stage earlier, in `IrBuilder::build`, at
the `0xbb` arm — a `new` whose class had no `new_info` row. That is the
documented `JitNewSite::Deferred` path: the resolver never runs a user
`ClassLoader.loadClass` from inside a compile, so a `new` of a not-yet-loaded
class defers, and the builder refuses the whole method.

That refusal is transient by design, and there is a one-shot memo
(`note_deferred_new_bail` / `take_deferred_new_retry`) to give such a method one
more optimizing attempt once the class loads. **The grant never checked whether
it had.** It flipped its `0` to `1` on the next supersede attempt regardless, so
on CratonBench each of the five `java/util/regex/Pattern` methods bailed
*twice* — once for real, once on a retry spent while the class was still
unloaded — and then had no retry left for the moment it did load. The doc
comment even described the failure ("a class that is still not loaded on the
retry bails again") without treating it as one.

The memo now records the deferred sites as `(holder_class_id, cp_idx)` and the
grant asks the resolver whether they resolve *now*; if not it holds the retry
rather than burning it. Measured on CratonBench:

| | before | after |
|---|---:|---:|
| C2 tasks that fell through to single-pass | 6 (36 ms) | **1 (6 ms)** |
| C2 tasks that produced an IR body | 3 (2 ms) | 3 (2 ms) |
| supersede publishes with a changed body | 8 | **3** |
| deferred-`new` retries held / spent | 0 / 6 | **5 / 1** |

The gate discriminates rather than refusing everything — it held five and
granted one — and `lowered=3` is unchanged, so no method lost its optimized
body. All seven CratonBench checksums are identical.

**This is not a throughput claim.** The saving is ~30 ms of *background compile*
CPU per run; the benchmark wall times moved by more than that in both
directions, which is run-to-run noise on this host, not a result.
`CRATONVM_JIT_DEFERRED_NEW_RETRY_BLIND=1` restores the blind grant.

### Holding a retry nobody offers again is the same as spending it

Holding was only half of it. A method that bailed on an unloaded class already
has a body, so nothing ever compiles it again — and the retry door is only ever
walked by the method being compiled at that moment. The first sweep was placed
on the compile door and re-offered *nothing*: `re_offered=0` against `held=21`
on CratonBench, and on a fixture built to load the class after the bail it held
four times and never came back.

The event that can change the answer is a class definition, and the place to
observe it is `ClassManagerWriteGuard::drop` — after the write lock is released,
beside `drain_pending_class_hooks`, which is there for the same reason. The
sweep now runs from there across every live VM, and costs one relaxed load
(`held_deferred_new_count`) when nothing is held.

`bench/DeferredNewReoffer.java` is the fixture that separates the two: the `new`
sits on a branch warmup never takes, so the class is still unloaded when the hot
method is compiled, and a later `touch()` loads it.

| | `BLIND=1` (blind grant) | held + re-offered |
|---|---|---|
| after the IR build bails | retry spent immediately | retry **held** |
| second compile | single-pass again, 2492 bytes | — |
| when the class loads | nothing left to offer | **re-offered** |
| final body | single-pass, 2492 bytes | **IR, 1495 bytes** |

Same checksum on both arms. This one *is* a capability change rather than
avoided waste: `make` reaches the optimizing tier, which under the blind grant
it could not. The size drop is the emitted body, not a timing.

#### Once per collector, because one run per arm is not a measurement

The collector is a variable this fixture has no business depending on, which is
the reason to check rather than assume. Five runs per collector, and the same
fixture under `CRATONVM_JIT_DEFERRED_NEW_RETRY_BLIND=1` as the control:

| collector | armed | re-offered | reached an IR body | control: IR body |
|---|---|---|---|---|
| ZGC (default) | 5/5 | 5/5 | 5/5 | 0/3 |
| G1 | 5/5 | 5/5 | 5/5 | 0/2 |
| Generational | 4/5 | 4/5 | 4/4 | 0/1 |
| Serial | 4/5 | 4/5 | 4/4 | 0/1 |
| Parallel | 5/5 | 5/5 | 5/5 | 0/1 |

Every one of the 25 runs produced checksum `-353614574`. Every re-offer that
happened produced an IR body (1495 or 1502 bytes against single-pass 2492);
under the blind grant, **no** armed memo on **any** collector ever reached one.
So the mechanism is collector-independent, and the control attributes the
difference to the grant rather than to anything else that moved.

**The fixture is timing-sensitive, and a `0` from it is not a regression.**
Arming requires `make` to be compiled by the C2 door *before* the C1 door
reaches it, and which door gets there first varies run to run: a single G1 run
during this sweep armed 0 times, and five consecutive runs immediately after
armed 5/5. Read this fixture over at least five runs. `make` contains a `new`,
so once the C1 door has it, `c2_upgrade_would_engage` refuses it without
`CRATONVM_JIT_C2_ALLOC_UPGRADE=1` — which is also how to force the arming path
deterministically when bisecting.

One measurement trap worth recording, because it cost a wrong conclusion here
first: probing for the IR body by grepping the exact literal `len=1495` reported
Generational at 2/5 when the true figure was 4/4. The re-offered body is 1495 or
1502 bytes depending on inlining, and an exact-size probe reads a body that got
7 bytes bigger as no body at all.

### Why a method falls through: the refusals could not be counted

`bailout.rs`'s own module doc names the gap: of the three ways the compiler says
"I cannot compile this", `Option::None` from `IrBuilder::build` and
`ir_lower::lower_inner` "carries no reason at all, so the per-method compiler
report the review asks for (admitted/bailout counts by reason) cannot be
produced." It could not, and nothing said so out loud — the per-compilation
record has carried an empty `bailouts` array since it was added.

Measured before touching anything: over CratonBench, **all 105** compilations
reported `bailouts:[]`, including the 17 that fell through to single-pass. The
process-wide category counters were moving the whole time, which is what made
the hole hard to see — the totals looked alive while every per-method row was
blank.

Two halves were missing, and both are the same one-line split `verify_or_bail`
already documents ("attribution, not duplication"): `record_bailout` owns the
process-wide counters, `metrics::note_current_bailout` attaches the same bailout
to *this* method. `ir_lower::refuse` did the first and not the second;
`ir::ir_build_bail` did neither.

With both wired, on `org.h2.test.db.TestAlter` (612 compilations, 50
fall-throughs) **50 of 50 now name a reason**, where 6 did before:

| reason | phase | count |
|---|---|---|
| `unsupported_shape` | build | 38 |
| `unsupported_opcode` | build | 6 — five `0x53` (`aastore`), one `0x5c` (`dup2`) |
| `unallocated_value` | lower | 4 |
| `code_buffer_exhausted` | lower | 2 |

Read with `CRATONVM_JIT_METRICS=1 CRATONVM_JIT_METRICS_OUT=<path>`, one JSON
object per compilation.

#### And ranked by site: one refusal is three quarters of them

`unsupported_shape` was 38 of 50 with the site living only in a debug line
nothing aggregates — 38 identical rows saying "the builder refused", naming
nothing to fix. The site now rides in the bailout's context, which turns that
into a work list. On `org.h2.test.db.TestAlter`, 44 build refusals:

| refusal | count | share |
|---|---:|---:|
| `ir.rs:7150` — an invoke pc with no `invoke_info` entry | 33 | **75%** |
| opcode `0x53` (`aastore`) | 5 | 11% |
| `ir.rs:6888` — `new` with no `new_info` (the deferred-class path) | 3 | 6% |
| `ir.rs:7094` — `invokespecial`, neither lowering applies | 2 | 4% |
| opcode `0x5c` (`dup2`) | 1 | 2% |

Three quarters of everything the IR builder turns away on a real workload is a
single line: `self.invoke_info.get(&pc)` answering `None`. Its neighbour at
7094 documents why that concentrates so hard — "one non-emittable invoke
elsewhere in the method discarded the whole map" — so a single unlowerable call
site refuses **every** call site in the method, and with it the method. That is
the next thing to fix in this tier, and it is one map rather than a list of
opcodes.

By contrast the missing opcodes (`aastore`, `dup2`) are 6 of 44 together: real,
but not where the methods are going.

#### What the census then said: a block-placement defect, not a sizing one

The investigation started from the hypothesis that large methods fail on code
buffer capacity. That is real but rare — 2 of 50. The dominant lowering refusal
is `unallocated_value`: **a value emitted after its own use**.

    StringUTF16.compress   n21 (Call)    at position 15, used by n34 (Return) at 12
    Pattern.range          n51 (Cmp(Ne)) at position 51, used by n57 (If)     at 21

`verify_data_locations` models emission as blocks in index order, so a use at 21
and a def at 51 means the definition's *block* is laid out after its user's.
`ir_schedule`'s own module doc states the opposite as invariant 1 — the layout
"keeps every definition before every use, so no live range inverts" — so this is
a violated invariant, not a missing feature. **Fixed 2026-09-04; see below**,
where the per-method effect is also stated more carefully than it was here: it
costs `String.equals` and `StringLatin1.equals` one of their two compilations
each, not the optimizing tier outright.

Not caused by the 2026-09-02 switches: `CRATONVM_JIT_IR_FUSED_BRANCH=0` and
`CRATONVM_JIT_IR_LINEAR_SCAN=0` each still produce exactly 3 on CratonBench.
Left open — correcting global code motion is a change to every compiled method,
and it wants its own branch and its own per-collector sweep.

### The block order was creation order, and nothing made it a reverse postorder

`ir_lower` emits `schedule.blocks` front to back, so block *index* order is
emission order. Block indices are assigned in **creation** order — whatever
order Step 1 happened to walk control nodes in — and nothing turned that into a
reverse postorder. A block could therefore be emitted before a block that
dominates it, and a value read before the instruction that defines it.

`verify_data_locations` catches exactly that and refuses the compile, so it was
never wrong code. It was lost compiles, silently, and until the bailout
attribution above it could not even be counted.

The machinery to fix it was already present and switched off. `layout_blocks`
produces a DFS layout whose stated property (invariant 2 of `ir_schedule`'s
module doc) is "for every edge `u → v` reachable from the entry, `pos(u) <
pos(v)` unless the edge is retreating" — and a dominator is a DFS ancestor, so
an RPO layout places it first and def-before-use follows. But it runs only under
`ScheduleOptions::layout_hot_paths`, which is `false` in the production
pipeline: `schedule()` is `schedule_with_options(graph, &ScheduleOptions::default())`.

So when hot-path layout is off, the blocks are now still laid out — just without
the frequency priority. `layout_blocks_rpo` is `dfs_layout(blocks, None)` behind
the same `validate_order`, and a validation failure keeps creation order and
compiles anyway, exactly as the hot-path arm already did.

| | `CRATONVM_JIT_IR_RPO_LAYOUT=0` | default |
|---|---|---|
| CratonBench: `unallocated_value` | 3 | **0** |
| CratonBench: fell through | 17 | **14** |
| H2 `TestAlter`: `unallocated_value` | 4 | **0** |
| H2 `TestAlter`: `code_buffer_exhausted` | 2 | **0** |
| H2 `TestAlter`: fell through | 50 | **44** |

Three H2 methods gain an optimizing-tier body they previously never got —
`FutureTask.awaitDone`, `MVMap.<init>`, `TransactionStore.getEntryId` — and
`String.equals` / `StringLatin1.equals` go from 1-of-2 compilations wasted to
2-of-2 lowered. The `code_buffer_exhausted` pair disappearing was not predicted:
a better block order emits less code, and those two methods were the ones on the
edge of their estimate.

**No throughput claim.** Six interleaved CratonBench rounds alternate in sign
(on faster, off faster, on faster) while the absolute totals drift ~50% across
rounds, which is this host under load, not a result. Checksums are identical in
every run.

Validated per collector, because reordering blocks moves oop-map and safepoint
positions in every compiled method: regression-suite 90/90 on ZGC, G1 and
Generational; `cratonvm-jit` 2225 passed.

### Summary table

| Feature | Default | Opt-out / opt-in var |
|---|---|---|
| Background compilation pipeline | **ON** | `CRATONVM_BG_COMPILE=0` |
| C1→C2 supersede | **ON** | `CRATONVM_C2_SUPERSEDE=0` |
| IR backend (int/ref/long/FP, non-virtual calls) | **ON** (bounded shape) | see `ir_compatible()` |
| IR backend for virtual/interface calls | **ON** | `CRATONVM_JIT_IR_CALL_VIRTUAL=0` |
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
| IR-tier register residency (GP + FP files) | **ON** since 2026-09-02, phis included | `CRATONVM_JIT_IR_LINEAR_SCAN=0`, `CRATONVM_JIT_IR_PHI_RESIDENCY=0` |
| IR-tier constants as immediates | **ON** | `CRATONVM_JIT_IR_CONST_IMM=0` |
| Skip the supersede-epoch bump when it cannot invalidate anything | off (nothing to skip: H2 shows 75/75 publishes genuinely changed) | `CRATONVM_JIT_SUPERSEDE_EPOCH_SKIP_USELESS` |
| Deferred-`new` retry held until the class resolves | **ON** | `CRATONVM_JIT_DEFERRED_NEW_RETRY_BLIND=1` |
| Reverse-postorder block layout (def before use) | **ON** | `CRATONVM_JIT_IR_RPO_LAYOUT=0` |
| IR-tier fused compare-and-branch, trampoline-free branches | **ON** | `CRATONVM_JIT_IR_FUSED_BRANCH=0` |
| IR-tier fused compare reads its operands in place (register, frame slot or folded immediate) | **ON** | `CRATONVM_JIT_IR_CMP_IN_PLACE=0` |
| IR-tier `x + k` / `x - k` as one `LEA` | **ON** | `CRATONVM_JIT_IR_ADD_LEA=0` |
| IR-tier receiver-guard CSE (once per receiver per block) | **ON** | `CRATONVM_JIT_IR_RECEIVER_GUARD_CSE=0` |
| IR-tier gated inline reference stores | **ON** where a collector publishes a plan | `CRATONVM_JIT_IR_GATED_REF_STORE=0` |
| IR-tier inline TLAB bump for `Op::New` | off — the sequence has a defect `RJitMapTierDiff` reproduces 4/10; see `ir_inline_tlab_enabled` | `CRATONVM_JIT_IR_INLINE_TLAB=1` |
| Thread pointer fetched from a TLS mirror (both tiers) | **ON** where the probe succeeds | `CRATONVM_JIT_TLS_THREAD_FETCH=0` |
| One post-call sentinel compare (both tiers) | **ON** | `CRATONVM_JIT_MERGED_CALL_SENTINEL=0` |
| ZGC VM-thread TLAB (the chunk the inline bump bumps) | off — `feat/zgc-jit-tlab-20260902`'s, kept opt-in because it measured slower there | `CRATONVM_ZGC_JIT_TLAB=1` |
| Receiver-type + call-site profile recording | **ON** | `CRATONVM_TIER_PGO_RECEIVERS=0` (branch/back-edge recording stays behind `CRATONVM_TIER_PGO`) |
| Guarded virtual inlining on receiver profiles | **ON** | `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE=0` |
| Gated inline reference stores | **ON** where a collector publishes a plan | `CRATONVM_JIT_GATED_REF_STORE=0` |
| Operand-stack register cache beyond pure kernels | off (see the section above for the ARG_REGS collision) | `CRATONVM_JIT_OPERAND_CACHE=1` |
| Optimizing tier for allocation-bearing methods | off — the tier's own bump is off, so a promoted allocation would lower through the stub's CALL again | `CRATONVM_JIT_C2_ALLOC_UPGRADE=1` |
| `this` seeded non-null at method entry | **ON** | `CRATONVM_JIT_THIS_NONNULL=0` |
| `getfield` receiver null-check elision | **ON** | `CRATONVM_JIT_RECEIVER_NULL_ELIM=0` |
| Implicit null check (fault + signal translation) | **ON** — 288 sites on H2 that no proof reaches; kill switch is the first move on any unexplained compiled-code crash | `CRATONVM_JIT_IMPLICIT_NULL_CHECK=0` |

### The tiering inversion — status, 2026-09-03

The audit that opened this work led with: *the optimizing tier compiles slower
code than the baseline tier, on every loop measured.* Where that stands, with
the measurement rather than an argument.

**Method**, because the first two attempts at this were wrong and the method is
what fixed them: release binary; arms interleaved run-by-run rather than in
blocks; and **a second arm of each configuration**, identical to the first, so
the spread between a config and itself is the noise floor. Fifteen reps, CPU
time, medians. `CRATONVM_C2_SUPERSEDE=0` pins the baseline tier,
`CRATONVM_JIT_FORCE_C2=1` the optimizing one, and
`compiles: c1=N c2=M` in the method-stats line witnesses that the arms really
differ.

| loop shape | baseline | optimizing | verdict |
|---|---|---|---|
| int arithmetic | 2.42 | 2.60 / 2.25 | within noise (control spread 14%) |
| **field read** | **0.86 / 0.83** | **1.39 / 1.41** | **optimizing ~1.65x SLOWER** |
| long arithmetic | 1.77 | 1.65 / 1.81 | within noise (10%) |
| double arithmetic | 8.58 | 8.61 / 8.61 | identical (0.3%, ±1% ranges) |
| array sum | 0.65 | 0.67 / 0.64 | within noise (5%) |

So **"every loop" is no longer true — one shape of five is.** The field-read
loop reproduces cleanly: both control pairs agree (3.5% and 1.4%) while the
groups differ by 65%, and the medians are separated by far more than either
spread.

**Re-measured 2026-09-10: still there, at 1.594x.** The table above is a
2026-09-03 snapshot and seven register flags went default-ON after it, so the
number was retaken rather than carried forward — `probes/FieldLoop.java` `sum`,
`tools/tier-ab/tier-ab.sh`, 503 ms baseline against 812 ms optimizing over a
2.6% floor. The register work that landed in between did not close it, and
widening the GP file does not either. See
`docs/internal/performance/c2-the-gp-register-file-is-not-the-binding-constraint-20260910.md`.

Two things it is **not**. The optimizing tier emits *less* code for that method
(1,030 bytes against 1,579), so it is not bloat; and `getfield helper calls`
is 0 at runtime in both arms, so it is not an out-of-line call per iteration.
What has not been examined is the instruction-level shape of the loop body at
each tier. That is where the next person should start, with
`CRATONVM_DBG=jit-disasm CRATONVM_DBG_JIT_DISASM=TierProbe.fieldloop`.

#### The disassembly, which settles it: neither candidate was the cause

Both tiers' `fieldloop` bodies, extracted between the backedge and its target
(`CRATONVM_DBG=jit-disasm CRATONVM_DBG_JIT_DISASM=TierProbe.fieldloop`).

**The baseline tier** unrolls 4x and keeps every loop-carried value in a
callee-saved register — `i` in `r12d`, `n` in `r14d`, `sum` in `r13`, `this`
in `r15`:

```text
219: cmp r12d,r14d          ; counter and bound, both registers
226: mov rax,r15            ; receiver, from a register
229: mov ecx,[rax+0Fh]      ; GC_FLAGS — and the implicit null check
239: movsxd rax,[rax+10h]   ; the field
284: add eax,ecx            ; sum, in a register
290: add r12d,1
```

Note what is absent: no `TEST RAX, RAX; JZ`. The receiver null check is
already gone here, elided by the `this` seed, and `mov ecx,[rax+0Fh]` is the
implicit check that replaced it.

**The optimizing tier** does not unroll, and round-trips *every* loop-carried
value through the frame on *every* iteration:

```text
1aa: mov rax,[rbp-58h]      ; receiver reloaded from the frame
1ae: test rax,rax           ; the null check
20d: mov [rbp-88h],rax      ; spill the loaded field
217: mov rcx,[rbp-88h]      ; reload it
220: mov [rbp-90h],rax      ; spill sum
227: mov rbx,[rbp-78h]      ; reload sum
235: mov [rbp-98h],rax      ; spill counter
23c: mov r12,[rbp-80h]      ; reload counter
243: mov rcx,[rbp-60h]      ; reload bound
```

That is roughly eight extra memory operations per iteration against a body
whose real work is one load and one add. **The null check is a rounding error
beside it**, which is why removing it moved nothing useful — and why the
tiering inversion on this shape was never a null-check problem.

**The allocator is not absent — it is losing.** With
`CRATONVM_DBG_IR_LINEAR_SCAN=1` on this method:

```text
[ir-ls] nodes=23 positions=17 peak_live=11 scan_promoted=9
        resident=3 (fp=0 gp=3) demoted=0 splits=4 scan_spills=1 scan_reloads=1
```

It runs, and it holds **three** of nine promoted candidates — against a peak of
eleven live values and a file of five GP registers (`IR_LOWER_LS_GPRS`).

**Where the other six go was, until 2026-09-03, misreported.** The consumer's
skip census read `split_or_spilled=5` beside the allocator's `splits=4`, and
the two together said "four candidates lost to live-range splits". They were
not. Printing the segment shapes showed four of the five refusals had **empty
segment lists** — nodes the scan produced no interval for, which is every
control and memory node in the graph, `Start` and `Proj` included — and one
genuinely spilled. **There was no split value on this method to reclaim.**

That miscount cost a day: split residency was designed, built, tested and
measured against it, and its engagement counter read zero, which is how the
mislabel was found. The census now separates `no_alloc`, `spilled` and
`split_or_spilled`, so the next reader gets three numbers that mean three
different things.

So the named causes of the residual inversion are, in order:

1. **The optimizing tier does not unroll.** The baseline's 4x unroll amortises
   the counter compare, the backedge and the safepoint poll over four
   iterations; the optimizing tier pays all three every iteration.
2. **The loop's values are ENTRY PARAMETERS, and entry parameters cannot be
   promoted at all.** Traced 2026-09-03, and it is the end of the chain.

`single_use` was the largest bucket (13) and looked like the answer: it refuses
any value read fewer than twice, counting **static** graph edges. Printing the
shape first — the lesson from the split miscount immediately above — gave:

```text
single_use n3 op=Param(0) static_uses=1 loop_weight=10   <- the receiver
single_use n4 op=Param(1) static_uses=1 loop_weight=10   <- the loop bound
```

One static use, ten loop-weighted. The rule compares a static count while the
definition and the uses sit at different loop depths: `this` and `n` are
defined once at method entry and read every iteration. `CRATONVM_JIT_IR_LS_LOOP_WEIGHT=1`
generalises the test to `uses_frequency >= 2 x definition_frequency`, which
reduces exactly to `use_count >= 2` at depth 0.

**It admits them past that gate and residency does not move** — `resident=3`
either way; they land in `no_alloc`/`spilled` instead. The policy was never the
binding constraint, because the allocator had already declined them.

**`MachineModel::pin_entry_params` pins every `Param` to its incoming ABI
register**, and `allocate_linear_scan` skips a pinned value outright
(`regalloc.rs`, `live.pinned[id]`). The ABI registers are caller-saved and are
not in `IR_LOWER_LS_GPRS` (RBX, R12–R15), so an entry parameter can never be
promoted into the callee-saved file the loop needs. It reaches the loop through
its frame slot, every iteration, by construction.

The baseline tier does the one thing this tier does not — it copies parameters
into callee-saved registers in the prologue:

```text
39: mov r15,rsi        ; this -> r15
3c: mov r14,rdx        ; n    -> r14
```

The prologue copy was built (`CRATONVM_JIT_IR_PARAM_COPY=1`, default OFF). It
works: `resident=3` becomes `resident=4`, `param_copies=1`, the loop bound
gets a callee-saved register. **It measures zero** — 2.07 s against 2.07/2.08
for the two control arms, which agree with each other to 0.5%, so that is a
real zero and not one hidden by noise.

#### Four candidates, four zeros, and what that finally says

| candidate | engaged? | effect |
|---|---|---|
| split residency | no (`split_recovered=0`) | census mislabel; nothing to reclaim |
| implicit null check port | yes (`elided=2`) | ~20% *worse*, then noise |
| loop-weighted use count | yes (params left `single_use`) | none — allocator had already declined them |
| parameter prologue copy | yes (`resident` 3→4) | none |

Every one of them was a register-residency or null-check argument, and none of
them moved a loop that is 1.6x slower at this tier. **The cost is not where any
of that reasoning says it is**, and the disassembly said so from the start if
the instruction counts are read per ITERATION rather than per body:

* baseline: 178 instructions covering **four** iterations — about **44 per
  iteration**, because the tier unrolls 4x;
* optimizing: 95 instructions for **one** — about **95 per iteration**.

A ratio of roughly 2.2x against a measured 1.6x, which is the only account so
far that is the right size. The counter compare, the backedge and the
safepoint poll are each paid once per iteration here and once per four
iterations there, and no amount of register residency changes that.

#### Unrolling tested: worth a fifth of the gap, not the gap

Tested the cheap way — by removing the advantage from the FAST arm rather than
building it into the slow one. `CRATONVM_DISABLE_UNROLL=1` turns off the
baseline's 4x unroll (both its call sites are in `x64/`), so if unrolling
explains the inversion the baseline should collapse toward the optimizing tier.

| arm | median |
|---|---|
| baseline, unrolled | 0.73 |
| baseline, same config (control) | 0.66 |
| **baseline, `CRATONVM_DISABLE_UNROLL=1`** | **0.84** |
| optimizing tier | **1.61** |

Unrolling is worth about **20%** — real, above the ~10% control spread. And it
is nowhere near the whole gap: the un-unrolled baseline is 0.84 against 1.61,
**still 1.9x apart**. So the section above overreached in calling instruction
count per iteration "the only account of the right size"; it is *an* account,
of about a fifth of it.

#### What that leaves, and the reconciliation the four zeros needed

The remaining 1.9x is per-iteration work that has nothing to do with unrolling,
and the disassembly names it: the optimizing tier round-trips **every**
loop-carried value through the frame, roughly eight memory operations against a
body whose real work is one load and one add.

That also explains why four successive fixes measured zero without any of them
being wrong. **Each addressed ONE value.** Removing one of eight memory
operations is ~12% of the loop's memory traffic and a few percent of its time —
at or under the measurement floor on this host. The four zeros are not evidence
that frame traffic is innocent; they are evidence that **it cannot be fixed one
value at a time.**

So the target is the class, not a member of it: the optimizing tier needs
loop-carried values to stay in registers *as a group*, which means the
write-through publish (a store at every definition, a load at every publish)
and the per-value residency policy both have to give way to something that
treats a loop's live set as one decision. That is a larger change than any of
the four, and it is the first one whose expected effect is above the noise
floor rather than under it.

#### Confirmed: register residency is 1.57x of it

Tested the same way, by taking the advantage away from the fast arm.
`CRATONVM_JIT_LOCAL_REGS=0` truncates the baseline's local-colouring pool to
empty, so every Java local lives in the frame — the optimizing tier's situation,
imposed on the tier that normally wins.

| arm | median | vs baseline |
|---|---|---|
| baseline | 1.33 | — |
| baseline, same config (control) | 1.27 | — |
| **baseline, `CRATONVM_JIT_LOCAL_REGS=0`** | **2.04** | **1.57x** |
| baseline, no locals **and** no unroll | 2.28 | 1.75x |
| optimizing tier | 3.09 | 2.38x |

(A busier host than the unrolling table above, so the absolute numbers are
larger; only the within-run ratios are being read, and the control pair agrees
to 5%.)

**Spilling the loop-carried values alone costs the baseline 1.57x** — the
single largest factor found, and it moves the baseline most of the way to the
optimizing tier without touching anything else. Adding the unroll loss brings
it to 1.75x of a 2.38x gap: about **two thirds of the inversion**, with
register residency the dominant share and unrolling roughly 1.12x on top.

#### The residual 1.36x: latency, not volume

Chased, and it is not what the rest of this section assumed. Both tiers were
disassembled at MATCHED settings — baseline with `CRATONVM_JIT_LOCAL_REGS=0`
and `CRATONVM_DISABLE_UNROLL=1`, so both spill and neither unrolls — and
counted:

| | baseline (matched) | optimizing |
|---|---|---|
| loop-body instructions | 108 | **95** |
| distinct frame slots touched | 24 | **15** |
| memory `mov`s in the body | 41 | **32** |
| time | 2.23 | **3.01** |

**The optimizing tier does less of everything and takes 1.36x longer.** So the
residual is not instruction count, not memory-operation count, and not slot
count — every volume measure points the wrong way. The
"instructions per iteration" framing earlier in this section explains the
unrolling fifth and nothing beyond it.

`CRATONVM_JIT_KERNEL_REG_LOCALS=0`, which makes the baseline's operand-stack
scratch cache inert, moved the matched arm not at all (2.23 against 2.23), so
that is not it either.

**It is a dependency chain.** A probe with four INDEPENDENT accumulators
(`a+=this.fx; b+=this.fx; c+=this.fx; d+=this.fx;`) instead of one shrinks the
gap from **1.36x to 1.18x**, with the two control arms landing on 1.41 and
1.41. Independent work overlaps a stall; it cannot overlap extra instructions.
That is the signature of a latency bottleneck, and the disassembly shows the
mechanism: the optimizing tier's body is a chain of store-then-load pairs on
the same slot two instructions apart —

```text
1f5: mov [rbp-88h],rax      ; store the loaded field
1fc: mov rax,[rbp-78h]
200: mov rcx,[rbp-88h]      ; reload it, two instructions later
207: add eax,ecx
209: mov [rbp-90h],rax      ; and the accumulator goes back to memory
```

— with the accumulator itself crossing the back edge through the frame, so
every iteration waits on the previous one's store.

**That pair has since been removed for one shape, and the removal is where this
tier's remaining frame traffic was finally counted.** A second carry slot lets a
consumer take BOTH of its single-use operands in registers instead of one, which
deletes exactly the store-and-reload above; widening the rule that says which
arm a carried value may cross — from an OP-level allowlist to the NODE-level
question the arm actually asks — took the probe set from 1 such carry to 11 and
is worth 1.009x against a 0.1% floor on a kernel that has the shape. The census
that made possible is the part to read before reaching for this paragraph again:
**82% of the candidate sites fail on operand POSITION**, not on anything the
emitter decides. Full write-up in
`c2-one-carry-slot-is-the-frame-traffic-ceiling-FIXED-20260910.md`.

The exact stall could not be named: this host is a VM without PMU passthrough
(`perf stat` reports `<not supported>` for cycles and instructions), so
store-forwarding latency is the likely mechanism rather than the measured one.

**What this changes.** It strengthens the register-residency conclusion rather
than competing with it: keeping a loop's live set in registers removes the
memory round trip *and* the chain that round trip creates. And it explains the
four zeros a second way — shortening a serial chain by one link out of several
does not speed it up. Both readings say the same thing: **the live set has to
move as a group, or not at all.**

#### Moving it as a group is an architecture change, not another heuristic

Attempted, and this is where the incremental route ends. A fifth change — a
register-to-register publish, taking the register at a resident definition
whenever `last_home_store` proves one already holds the value, position-checked
so any intervening emission falls back to the load — produced **byte-identical
code** on this method: same 1,030 bytes, same 95 loop instructions, same 22
frame loads. The precondition never held. It was withdrawn rather than landed.

That failure is the informative one, because of what the census says alongside
it. Residency IS working: `resident=3 (gp=3)`, and `rbx`, `r12` and `r13`
appear eleven times in the loop body. Three values are genuinely being read out
of registers — **and the body still has 22 frame loads and is still a
store-then-load chain.**

The reason is structural. `lower_data_node` gives every node a frame slot
(`alloc_slot(id)`) and writes it; the residency file is a **read cache layered
on top of that**. So a value costs its store whether or not it is resident, the
store-then-load pair survives residency, and no policy change on the cache can
remove a store the model emits unconditionally.

**So "move the live set as a group" means making the home slot OPTIONAL** — a
value that lives in a register for its whole range and is named by no deopt
frame and no safepoint map should not have a home at all. That is a change to
the lowering model, with the deopt and oop-map obligations to discharge for
every value that loses its slot, and it is the first item on this list that
cannot be tried behind a flag in an afternoon.

The five zeros are the case for doing it properly rather than continuing:
split residency, the null-check port, the loop-weighted use count, the
parameter prologue copy and the direct publish each addressed a symptom of the
frame-slot-first model, and the model absorbed all five.

That change is designed, sized and not built:
[`feature-designs/ir-optional-home-slot.md`](feature-designs/ir-optional-home-slot.md).
It records the three obligations already verified (deopt and safepoints are
covered by `pinned`; references are excluded for oop-map reasons; the read side
is 84 cached against 13 direct), the coupling that sets its shape (dropping the
store needs a register-to-register publish, which needs the arms to say where
their result is, and 50 of them say it only by writing memory), and a
fail-closed route for the one hazard — `slot_of` on a homeless value fails the
compile rather than reading a stale word, so the first run names the sites to
convert instead of a whitelist being guessed.

**So the recommendation stands and now has a number behind it.** Getting a
loop's live set into registers *as a group* is worth about 1.57x on this shape.
That is an order of magnitude above the measurement floor that swallowed all
four single-value fixes, which is exactly why it is the one worth building.

The loop-weight rule is kept, default OFF, because it is a correct
generalisation that will matter once the parameters can be promoted at all —
and because its own measurement is on record as not moving this workload.

What it is **not**: splits, register pressure at the file's edge, code size
(the optimizing tier emits *less* code here), or the null check.

Neither is the null check, and neither is code size — the optimizing tier emits
*less* code for this method (1,030 bytes against 1,579).

**The named candidate was tried, and it made things worse.** Porting the
receiver null-check elision to the optimizing tier is built and switchable
(`CRATONVM_JIT_IR_THIS_NONNULL=1`, default OFF). It is correct and it engages
— `seeded=9 elided=2 emitted=0` against `0/0/2` with it off, same answer —
and on the very loop above it is **~20% SLOWER with the check removed**:
medians 1.78/1.93 on against 1.49/1.61 off, two replicate pairs, within-config
spread 8%, same direction both times.

At `reps=1`, where the loop barely runs, the arms are 0.19 against 0.18, so
the per-block seed costs ~0.01s and the 0.3s is in the emitted code, not the
compile.

Deleting two instructions cannot slow a loop by 20% on its own. What that
result actually says is that this loop body is dominated by something
layout- or branch-structure-sensitive, and removing a never-taken forward
`JZ` moved it. **That is now the most promising lead for the residual
inversion**, and it is why the switch is kept rather than the change reverted:
it is the smallest known perturbation that moves this loop by 20%, which makes
it the cheapest handle on whatever the real cause is.

**2026-09-11 — RETIRED: the anomaly no longer reproduces.** Re-measured on the
current tree with `tools/tier-ab/flag-ab.sh` (7 rounds, interleaved, same-config
control, checksum `1200150000` on every run), `CRATONVM_JIT_IR_THIS_NONNULL` on
`probes/FieldLoop.java` `sum` is **0.981x — UNMEASURABLE inside a 4.6% floor**.
It is not 20% slower; it is not measurably anything. Whatever arrangement
produced the 1.78/1.93-against-1.49/1.61 medians is gone, most likely with the
phi-copy change in
`docs/internal/performance/c2-the-phi-copy-staging-register-20260911.md` §5.

So this paragraph's standing recommendation — keep the switch because it is the
cheapest handle on the residual inversion — no longer holds: there is no longer
an effect for it to be a handle on. Keep the switch on its own merits (it is
correct and it elides real checks), not as a lead.

The layout theory it invited was tested and did not survive either.
`docs/internal/performance/c2-the-loop-body-is-mostly-code-it-never-runs-20260911.md`
counts this loop at **412 bytes spanned, ~122 executed**, the rest cold code
emitted inline; `CRATONVM_JIT_IR_POLL_OUTLINE` removes the largest of those
blocks (229 bytes) and one taken branch per iteration, and it measures
**0.999x — UNMEASURABLE**. A well-predicted branch over cold bytes costs
approximately nothing, because fetch follows the predicted target rather than
the linear address. What actually moved this loop was removing WORK: see the
same document's §3.

One structural asymmetry is worth naming as a candidate: the receiver
null-check elision described above — the `this` seed and
`CRATONVM_JIT_RECEIVER_NULL_ELIM` — is **single-pass only**. Both arms it
touches are in `x64/bytecode_walk.rs`; the optimizing tier still emits
`TEST RAX, RAX; JZ` at every `getfield`. On a loop whose entire body is one
field read, that is not obviously small.

**A note on the apparatus, because it cost two wrong answers.** The first run
compared blocked arms with no same-config control and reported the optimizing
tier slower on three shapes; the second showed the same configuration
disagreeing with itself by 13–22%, which was larger than every effect claimed.
A timing arm on this host without a same-config control is not a measurement.
The `C1/C1B` and `C2/CTRL` pairs above exist for that reason and should be kept
in any re-run.

#### The census refuted the design, and named the site that was worth fixing

Built the instrument before the feature, and it is the reason there is no sixth
zero to report. `plan_register_residency` now counts, over the values it
actually gave a register to, how many could lose their home word:

```text
[ir-ls] resident=3 (fp=0 gp=3)
[ir-ls] home: droppable=0 blocked_deopt=1 blocked_phi=2 safepoints=16
```

> **The census in that code block no longer exists, 2026-09-10.** It kept
> asking the question it was written for — "promoted, named by no safepoint at
> all, and not a phi" — while `ir-reg-authoritative`, `ir-drop-home` and
> `ir-drop-phi-home` widened the rule the emission uses to "named by no
> REACHABLE frame state". By 2026-09-10 it printed `droppable=0` on a compile
> of `FieldLoop.sum` that dropped **three** homes and skipped five stores. It
> is replaced by `[ir-ls] homes: dropped_values=` (the outcome) and
> `[ir-ls] homes kept: switch/deopt/type/op` (the per-cause remainder), the
> second computed from `home_dropped` itself under an accounting identity so
> it cannot fall behind again. Read the block below as the 2026-09-04 record
> it is.

**Zero of three**, on the loop this whole section is about. The design's safety
argument — that `pinned` covers every deopt-named value, so a promoted value is
named by no frame state — is true in `regalloc.rs` and false where it is used:
`plan_register_residency` calls `release_deopt_pins` deliberately and pays for
it by keeping every home the colourer planned. A 50-arm refactor of the
lowering arms would have had nothing to act on.

`blocked_phi=2` is the useful half. **The loop-carried values ARE the phis**,
and a phi's home is written by `emit_copy_op`, which was memory to memory:
`load rax, [src]`, `store [dst], rax`, and then `emit_phi_copies` reloaded the
word it had just written to publish the phi's register. Then, because a phi
appears in its header block's node list like any other value, the generic
publish site reloaded it **again, once per iteration** — which is the other
half of the loop-carried chain, `mov [rbp-78h],rax` on the back edge and `mov
rbx,[rbp-78h]` at the top of the next iteration waiting on it.

Two changes at that one site (`CRATONVM_JIT_IR_PHI_COPY_REGS=1`,
`CRATONVM_JIT_IR_SKIP_REPUBLISH=1`, both default OFF). The disassembly confirms
both fire: `FieldLoop.sum` goes 1030 → 1026 → 1018 bytes as they are turned on,
the preheader's two publishes become `mov rbx,rax` / `mov r12,rax`, and **both
loop-body reloads disappear** — the phis are read straight out of `rbx` and
`r12`.

**And it measures zero.** Interleaved arms, user CPU time, a second arm of the
control configuration as the floor:

| probe | floor (ctl vs ctl2) | on vs ctl | P(on < control) |
|---|---|---|---|
| `FieldLoop.sum` (2 loop-carried) | 0.86% | +0.86% | 0.529 |
| `FieldLoop.sumWide` (5 loop-carried) | 0.00% | +1.27% | 0.516 |

`P(on < control)` is over all 15x30 arm pairs; 0.50 is no effect. Two
instructions out of a thirty-two instruction body, in a loop with enough
independent work to overlap them, is below what this host can resolve.

**Half of it is inert on this shape, and the counter says which half.**
`[ir-ls] phi copies: reg_reads=0 reg_publishes=4` — four publishes (two phis
times two edges) and not one register read, because the sources of those copies
are the `Add` results, which are single-use and therefore never promoted. The
read half waits on a shape where a phi's incoming value is itself resident.

Correctness is established rather than assumed: `probes/PhiSwapLoop.java`
(two-cycle, three-cycle, mixed GP/FP) matches HotSpot with the flags on and
off, the 2,489 `cratonvm-jit` tests pass with both flags on, and the regression
suite is 90/90 in both arms.

**The CratonBench arms were vacuous, and the check that caught it is worth
copying.** `sieve`, `matrix` and `arithmetic` were run the same way and came
back at 0.407, 0.475 and 0.549 — until `CRATONVM_DBG=ir-linear-scan` was read
on each of them:

```text
fib:    ir-ls=3
sieve:  ir-ls=0
```

**The IR tier plans no residency at all on those kernels**, so both arms ran
identical machine code and the three numbers describe nothing. `osr_entered=504`
with `osr: admitted=2` says where the time actually goes. That reach question —
how much of a real workload the optimizing tier's body reaches in the first
place — is a prerequisite for any further measurement in this section, and it
had not been asked.

**What is sequenced next, and why it is now sequenced rather than assumed.**
`FrameValue::Register`, `RegisterLong` and `RegisterRef` already exist and are
tested, so deopt metadata CAN name a register — but the IR tier's
`emit_deopt_stub` passes only `rbp` to `ir_deopt_entry` and reserves no
`SavedRegisters` region, so nothing would fill one. Dropping the home of a
deopt-named value needs that region reserved and the callee-saved file spilled
into it first. `blocked_deopt` is the counter that says what that would buy
(retired 2026-09-10; `[ir-ls] homes kept: deopt=` is its successor).

#### The register image was built, and the home is gone for the values it covers

The sequenced step, taken. `ir_deopt_entry` used to say "the IR lowerer keeps
every live value in a frame slot, so no register file is needed; a
register-allocating backend would spill GPRs/XMMs in the trampoline and pass
them here instead" — and that sentence was the whole reason a resident value
could never lose its home. Four switches, all default OFF:

| switch | what it does |
|---|---|
| `CRATONVM_JIT_IR_DEOPT_REGS=1` | reserve 256 bytes, spill 16 GPRs + 16 XMMs at the stub, pass a `*const SavedRegisters` |
| `CRATONVM_JIT_IR_DROP_PHI_HOME=1` | stop writing the home word of a value a deopt frame can name in its register |
| `CRATONVM_JIT_IR_PHI_COPY_REGS=1` | (already present) edge copies move register to register |
| `CRATONVM_JIT_IR_SKIP_REPUBLISH=1` | (already present) an already-live register is not re-published |

**The naming rule is exclusive ownership, not residency**, and the distinction
is the whole safety argument. A value owns its register over its LIVE RANGE,
and `plan_register_residency` releases the deopt pins — so a bytecode local can
still be named by a frame state long after its last IR use, by which time the
allocator may have handed the register to something else. A value is therefore
nameable only when no other value anywhere in the method holds the same
register: then it holds from the definition to the end of the frame, and no
mapping between `graph.safepoints` and allocator positions is needed at all.
`slot_of` on a dropped home fails the compile; `home_read_refusals` counts it.

**It engages, and the loop loses its last frame traffic.** On `FieldLoop.sum`:

```text
[ir-ls] deopt regs: nameable=2 frame_slots_named_by_register=7 regs_base=464
[ir-ls] homes:      dropped_values=1 stores_skipped=2 read_refusals=0
```

Seven frame-state slots now describe a register, one loop-carried value has no
frame word at all, and no reader refused. In the disassembly the counter is
`rbx` throughout — no store on the back edge, no reload at the top, nothing.

**A bug the executable test found, which is the reason to write that kind of
test.** `deopt_regs_base` was computed arithmetically whether or not the region
had been reserved, so with the flag OFF it came out non-zero — which the stub
reads as "there is a region here" — and 32 spill stores went over the argument
staging area and past it. `test_guard_deopt_reconstructs_live_frame` SIGSEGV'd.
This is the mirror image of the hazard `deopt_spill_region_reserved` already
records for the single-pass backend, where an unreserved region left the base at
0 and the stores walked UP over the saved RBP and the return address.

**And it measures nothing.** Interleaved, user CPU time, 15 rounds, with the
control repeated:

| arm | median | P(arm < control) |
|---|---|---|
| control | 0.79 | — |
| control again | 0.85 | — |
| phi copies only | 0.93 | 0.411 |
| all four | 0.95 | 0.424 |

The two controls differ by 7.6%, so the floor swallows everything; `P(all <
phi)` — the home-drop on its own, against the arm it depends on — is 0.516,
which is chance. The host was at load 5-9 throughout. The honest reading is
that this is not resolvable here, not that it is a regression.

**Two limits worth stating rather than discovering later.** With the flag on,
every IR frame grows by 256 bytes and every deopt stub by 32 stores, which is
why it is off. And no Java workload built for this reaches an IR-tier deopt at
all — `deopts=0` everywhere, and the probe written to force one
(`probes/DeoptRegLoop.java`, a null receiver inside a loop) was never offered to
the IR backend. The register-naming path is proven by
`a_deopt_frame_reads_a_register_the_stub_spilled`, which emits a body that puts
a sentinel in RBX and in no frame word, jumps to the stub as a guard does, runs
it, and asserts the reconstructed local is that sentinel. That is a stronger
proof than a workload would have been, and it is currently the only one.

#### How much of a real workload the optimizing tier reaches: the census

**This is the prerequisite question, and it had never been asked.** Every
measurement in this section — the 2.38x inversion, the four zeros, the five
zeros, the register image — concerns the body the optimizing tier emits. None of
them asked how often it emits one.

`CRATONVM_DBG_IR_COMPILES=1` already answers it: the pipeline has four stages
that can decline a method (`ir_compatible`, the admission conjunction,
`IrBuilder::build`, `ir_lower`), and each reports which one it was. Run over
CratonBench, one phase per process, with `CRATONVM_JIT=force-c2`:

| phase | offered to the gate | admitted | reached lowering | why refused |
|---|---|---|---|---|
| arithmetic | **0** | 0 | 0 | never offered at all |
| hashmap | **0** | 0 | 0 | never offered at all |
| sieve | 2 | **0** | 0 | a bulk byte-array zero fill (`REP STOSB`) |
| matrix | 1 | **0** | 0 | one `multianewarray` in the method |
| fib | 1 | 1 | 1 | — |
| bintrees | 4 | 4 | 4 | — |
| stringregex | 91 | 65 | 52 | 22 `invokedynamic`, 12 String pin, 4 over the invoke cap, 1 precise frames |

**On four of the seven kernels the optimizing tier lowers nothing at all**, and
they are the loop-dominated four. That is the explanation for every zero this
section records against a real workload, and it is a better explanation than any
of the per-optimization ones: an A/B of an IR-tier switch on `sieve`, `matrix`,
`arithmetic` or `hashmap` compares a binary against itself.

Three distinct causes, and they want different answers:

* **Never offered.** `arithmetic` and `hashmap` produce no `[ir]` line whatever
  — the admission gate is not consulted once. Their hot code enters through OSR,
  and `compile_osr_artifact` "reaches `x64::compile_with_param_slots` directly":
  **the OSR door has never gone through the optimizing tier.** No flag changes
  that.
* **One instruction disqualifies the whole method.** `matmul` allocates its
  result with a single `multianewarray` at the top and its hot triple loop is
  refused along with it; `sieve` zero-fills a byte array once and pays the same.
  Both refusals are of the form "the single-pass backend has an intrinsic here
  and the IR tier has none", which is a fair trade when the intrinsic is hot and
  the wrong one when it runs once per call against a loop that runs millions of
  times. `CRATONVM_JIT_IR_OVER_INTRINSIC=1` already makes exactly this trade for
  CALL-SITE intrinsics; neither of these two is covered by it.
* **Attrition through the funnel.** Where the tier does work, it still loses most
  of what it takes: `stringregex` goes 91 → 65 → 52 → **17 bodies**, and the
  largest single loss at the builder is `new-site DEFERRED` — a class not yet
  loaded when the compile ran, which `take_deferred_new_retry` grants exactly one
  retry for.

**What this changes about the work in this section.** The tier-inversion
programme has been optimising a body that, on the kernels used to motivate it,
is never emitted. Before another switch is added to `ir_lower`, the reach
number is the one to move — and of the three causes, the OSR door is the largest
and the only one no flag can reach.

**Method note.** Read `[ir] admission` counts before believing a per-phase A/B,
and do not read `compiles: c2=N` as "N optimizing-tier compiles": on `sieve` it
says `c2=3` while the IR backend lowered nothing, because that counter is fed by
the tier manager's nomination and not by the backend that ran.

#### The OSR door: the go/no-go, and the blocker that is actually in the way

The reach census named the OSR door as the largest cause and the only one no
flag can reach. The first question is whether there is anything at the far end
of a route through it, and `ir_compatible_sized` is pure and `scan` is already
in hand at that door, so the question costs nothing to ask. Per CratonBench
phase, over the methods actually compiled there:

| phase | OSR compiles | pass `ir_compatible` |
|---|---|---|
| arithmetic | 1 | **1** |
| hashmap | 1 | **1** |
| sieve | 2 | **2** |
| matrix | 1 | 0 (`multianewarray`) |
| bintrees | 1 | **1** |
| stringregex | 1 | **1** |

**Six of seven**, and they include `arithmetic` and `hashmap` — the two phases
where the optimizing tier is offered nothing whatsoever today. Read it as an
UPPER BOUND: `ir_compatible` is the first of four gates, and `sieve` in
particular would still be refused further down for its bulk byte-array zero
fill. But the route is not empty, which is what a go/no-go needed.

**The design that fits this codebase.** Do NOT teach `osr_trampoline` the IR
frame. It takes some twenty layout parameters (`osr_local_assignments`,
`osr_callee_saved_base`, `osr_xmm_saved_base`, …) because it builds the
single-pass frame from OUTSIDE, and that arrangement depends on a property the
optimizing tier does not have: a fixed per-method local→home map. IR locals are
SSA values on colour-assigned slots that differ from bci to bci.

Instead, have `ir_lower` emit its own OSR entry stub per eligible bci. It
already holds everything needed and the trampoline holds none of it:

* the frame — it built it, bookkeeping slots and save bands included;
* where local `i` lives at bci `b` — the safepoint snapshot at `b` names the
  node, and `node_slot` / `gp_reg_of` name its location. This is the same
  information `build_deopt_points` already reads, used in the opposite
  direction;
* the native offset of `b` — `bci_native`.

The VM side then needs only "is there an entry for this bci" and a
three-argument call, instead of twenty layout fields. Entry bcis must have an
EMPTY operand stack (a javac loop header does), and a `Ref` local must be
seeded exactly where that bci's oop map says it lives.

**The blocker, named precisely, because it is not the frame.** The optimizing
tier's inputs — the invoke plans, `checkcast_info`, the `new`-site resolutions,
the inline sites — are assembled inside `try_compile_inner`, and the OSR door
does not go through it. `compile_osr_artifact` assembles a DIFFERENT set for the
single-pass backend and calls `x64::compile_with_param_slots` directly. So the
work is not "emit an entry stub"; it is "give this door the optimizing tier's
input pipeline", and the honest way to do that is to route OSR through the
common funnel rather than beside it.

That refactor pays for itself twice. The same divergence has already produced
three bugs this file records — the permanent bail-list, the bisect levers and
the code-cache cap were each hand-copied to this door after a bug, and the
`compile_gate::admit` token exists to stop a fourth. A door that goes through
the funnel cannot drift from it again.

**Not started, deliberately.** Entering a compiled body part-way with a
hand-seeded frame is the failure mode that produces a plausible wrong number
rather than a crash, and this section already records what happens when that
class of work is begun without the instrument first. The census above is the
instrument; the go is now on the record with a number behind it.

#### The OSR route, measured: it works, it is correct, and it is 1.56x SLOWER

The reach census said the optimizing tier never sees a loop entered by a back
edge; the wiring made it possible; this is what it bought.
`probes/OsrTierBench.java` is the shape the census named — a kernel entered
ONCE, calling nothing, so a back edge is its only route to compiled code — and
it is admissible under `ir_osr_sentinel_free`. Interleaved, seven rounds, the
probe's own in-kernel `ms` so process startup is not in the number:

| arm | median ms |
|---|---|
| control | 352 |
| control again | 355 |
| `CRATONVM_JIT_FORCE_C2=1` alone | 359 |
| **door ON** | **551** |

The two controls differ by 0.9%, and `FORCE_C2` alone lands inside that — so the
confound is excluded and the door owns the difference. **1.56x slower, slowest
in all seven rounds.** The answer is `ck=25500075088100865`, which is HotSpot's,
so this is a speed result and not a correctness one.

**This is the tier inversion again, and that is the point.** It is the same
~1.5x this section has measured all along, now reproduced on a DIFFERENT probe
through a DIFFERENT door — which is the independent confirmation the original
number never had. The optimizing tier's loop body is worse than the single-pass
tier's, and giving it more loops to compile makes things worse in proportion.

So `CRATONVM_JIT_OSR_OPTIMIZING` stays off, and the reach work's payoff is
GATED on the body, not on the route: the 1.57x register-residency share and the
1.36x latency residual are what stand between this route and a win. The route
is infrastructure that pays nothing until they are fixed — which is worth
knowing now rather than after they are.

#### And the phi work finally has a positive number

The register-to-register phi copies, the suppressed re-publish, the register
image and the dropped home all measured ZERO when they landed. They were
measured on `FieldLoop`, where the optimizing tier was not running the hot loop
— so the arms compared a body that barely mattered. This probe is the first
workload where that tier owns a loop through OSR, and the same four switches on
top of the door read:

| arm | median ms |
|---|---|
| door ON (control) | 558 |
| door ON again (control) | 575 |
| **door ON + the four switches** | **526** |

**`all` beat its paired `on` run in 11 of 12 rounds**, `P(all < control) = 0.743`
over all pairs, against a 3.1% control-vs-control floor on a busier host. About
**6%**, and the paired count is the statistic to read — under no effect it is a
fair coin, and 11 of 12 is not.

That does not rescue the 1.56x. It does say the work was sound and the earlier
zeros were a measurement problem, not a design problem: **a change to the
optimizing tier's loop body cannot be measured on a workload where that body is
not what runs.** Every zero this section records against those switches was
taken on one.

#### Taking on the 1.57x: the census says ONE value of four is in a register

With a workload where the optimizing tier finally owns a loop
(`probes/OsrTierBench.java` through the OSR door), the residency census is
readable for the first time on the population that matters. Its kernel has four
live values — `n`, `sum`, `acc`, `i` — and five registers to put them in:

```text
[ir-ls] peak_live=15 scan_promoted=19 resident=1 (gp=1) splits=12
[ir-ls] skipped: split_or_spilled=3 const=7 single_use=20 no_alloc=3
```

**The allocator promotes nineteen values and the residency file accepts one.**
`split_or_spilled=3` is the whole loop-carried set minus the one that survived.

**Why the scan evicts exactly the wrong values.** `ls_pick_victim` maximises
`next_use_distance × SCALE / frequency_weight`, which is the classic rule and is
right when a reload is paid ONCE. A loop-carried value's next use is across the
back edge, so its distance is large; and it typically has FEWER uses than a
temporary in the same body, so its weight is smaller. Both terms point the same
way, and the value whose eviction costs a store and a reload on *every
iteration* scores as the best victim available.

Loop-depth weighting cannot separate the two, and that is worth stating plainly
because the weighting is already there: a loop-carried phi and a temporary in
the same loop body sit at the SAME depth. What distinguishes them is not where
they are but how long they live — across the back edge, or not.

**The fix, and what it bought.** `LiveModel::carried` marks a value whose range
spans a back-edge position, and `ls_carry_relief` divides such a value's
distance before scoring, which keeps the ordering among carried values while
moving all of them behind the uncarried ones. It works, deterministically:
`resident` 1 → 2 and `split_or_spilled` 3 → 2, saturating by a relief of 64.

**It is default OFF, because it is not shown to pay.** The timing arm was
attempted and is not usable: the host was at load 22 on 8 cores, and this file
has recorded twice already what a contended host does to an arm. A zero taken
there is not a zero. That measurement is owed.

**And the census names what actually stands in the way.** Two of the four
carried values are STILL split, and `plan_register_residency` refuses any split
value outright — the file has no reload machinery, so "one segment, one
register, whole range" is the admission. With four carried values and five
registers there is no reason to split any of them; the scan splits them because
it allocates them in competition with eleven transients under `peak_live=15`.

So the shape of the remaining work is not a better heuristic. It is **reserving
the carried set** — assigning those values registers before the scan runs and
letting everything else compete for what is left, which is what the single-pass
tier does by colouring locals into callee-saved registers and is why it wins by
1.57x. The heuristic fix above moves one value; reserving moves the set, and
this section's own conclusion has been "the live set has to move as a group, or
not at all" since the four zeros.

#### Reserving the carried set: the whole live set is in registers now

The blocker the previous census named, taken on. `plan_register_residency`
assigns a register to every LOOP-CARRIED value out of the ones the scan left
free, before the parameter copy and after the scan — the same door the parameter
copy already uses, and the same safety argument: **a register in this file that
the accepted set does not name is written by nothing**, because emission is
driven by `gp_reg_of` alone.

The point is not that the scan decided badly. It is that it was asked the wrong
question: it allocated four values that are needed on every iteration in
competition with eleven transients under `peak_live=15`, so it split them, and
the file refuses a split value because it has no reload machinery. Reserving
asks instead — these few are needed every iteration, give each one a register
and let everything else have the rest.

On `probes/OsrTierBench.java`, with `CRATONVM_JIT_IR_RESERVE_CARRIED=1`:

| | resident | carried_reserved |
|---|---|---|
| off | **1** (gp=1) | 0 |
| on | **5** (gp=5) | 4 |

All five registers in use, and each of the kernel's four loop-carried values has
one. That is "the live set moves as a group" actually happening, after a
section-length run of changes that each moved one value.

**Correctness holds**: `ck=25500075088100865`, HotSpot's answer, with the
reservation on — and the same under `--nojit`.

**The timing is owed, again, and for the same reason.** The control-vs-control
floor on the day was **6.3%** at host load 12–40 on 8 cores; the arms
(`base` 691, `reserve` 752, `reserve`+the phi stack 677) all sit inside it. This
file has now recorded three separate days where a contended host denied an arm,
and the rule it keeps proving is that a number taken there is not a number.
Default OFF until it is measured on a quiet one.

**What the measurement should expect to see, when it happens.** Reserving
removes the RELOADS of the carried set; the write-through home STORES remain
unless `CRATONVM_JIT_IR_DROP_PHI_HOME` is also on, which is why the two want
measuring together. The prediction the 1.57x implies is that the pair, not
either alone, is what closes it.

#### The measurement a contended host cannot deny: the loop body itself

Three days of timing arms have been refused by host load. The emitted code is
not: it is a deterministic function of the compile, so counting it settles what
a stopwatch could not. Doing that first required fixing an instrument.

**The OSR door's optimizing artifact was invisible to `CRATONVM_DBG_JIT_DISASM`.**
The only `osr` dump comes from inside `compile_osr_artifact`, which the door
SKIPS when it takes an optimizing body — and the background tier worker calls
that function anyway, so a dump appears, is labelled `osr`, and is the
single-pass body. It is byte-identical with the door on and off, which reads as
"the door changes nothing" while `osr_entered_optimizing=1` and a 1.56x timing
gap say it changes everything. The door now dumps what it actually enters, under
`osr-optimizing`.

With that, `OsrTierBench.kernel`'s loop body, counted:

| arm | loop insns | frame ops | loads | stores |
|---|---|---|---|---|
| door only | 138 | 52 | 22 | 19 |
| **+ reserve the carried set** | 152 | **60** | 26 | 23 |
| + reserve + the phi/home-drop stack | 146 | 50 | 20 | 19 |

**Reserving the carried set ALONE makes the loop worse**, and the mechanism is
the one this section named long ago: residency is a **write-through read cache
over a frame-slot-first model**. Promoting a value adds a PUBLISH at its
definition and does not remove its home STORE, so promoting four more values
buys four more memory operations per iteration and removes reads only where a
read already went through `gp_load_value`. `resident=1 → 5` is a real census
movement and a regression in emitted traffic.

That is a direct correction to the expectation set when the reservation landed,
which predicted the pair would close the 1.57x. The pair is better than
reserving alone — 50 against 60 — but it is only 52 → 50 against doing neither,
on a kernel with four live values whose loop still performs **fifty** frame
operations. The gap is not going to be closed by promoting more values into a
cache that cannot remove the stores underneath them.

So both switches stay OFF, and the next move is not another promotion policy.
It is the one the design page named and the census keeps re-deriving: a value
that lives in a register for its whole range should have **no home slot and no
store**. `CRATONVM_JIT_IR_DROP_PHI_HOME` does that for phis, at one site; the
other forty-nine store sites are what the loop's remaining fifty frame
operations are made of.

#### The forty-nine store sites, and what removing them did not buy

`CRATONVM_JIT_IR_DROP_PHI_HOME` dropped one frame store, at the one site
(`emit_copy_op`) where this backend knew both that RAX held a value and *which*
value it was. Every other definition writes its home through `store_rax`, which
knew neither. Both facts were available and neither was being passed:
`lower_data_node_tracked` now records the definition being lowered, and
`publish_def_at_store` recognises the store whose offset is that definition's
own home — at which point RAX provably holds it, because the only thing a home
word is ever written with is its own value.

That makes two per-definition frame operations reachable without editing fifty
arms:

* `CRATONVM_JIT_IR_PUBLISH_AT_DEF` publishes the register from RAX **at the
  store**, instead of the generic publish site reloading the word the arm just
  wrote — the register-to-register publish that site's own comment has called
  cheaper since it landed;
* `CRATONVM_JIT_IR_DROP_HOME` then drops the store itself for any value the
  deopt register image can name, extending the phi case to the arithmetic arms.

`op_home_is_one_store_rax` is an audit of the arms whose lowering writes its
home exactly once through `store_rax` with RAX holding the value. Arms with a
home write on one path and not another (`Op::Load`, `Op::CheckCast`,
`Op::Call`), arms that reach the home another way (`Op::Const`'s immediate
store, `Op::Param`'s `gp_store_value`, every FP `fp_store_value`), and the
comparisons — whose home write is conditional on a fusion decision made in
`lower_terminator` — are all absent. Getting that list wrong is **not silent**:
`lower_data_node_tracked` refuses the compile when a dropped-home value reaches
the end of its own lowering unpublished, and `value_home_droppable` additionally
requires `ir_phi_copy_regs_enabled`, because `gather_phi_copies` is the one
reader exempt from `slot_of`'s fail-closed refusal.

**First, an instrument correction, because it reverses a published verdict.**
The loop-body counts in the section above were taken over the range between a
back edge's target and the jump that takes it — choosing the OUTERMOST such
pair. On an artifact that carries OSR entry stubs that is the wrong range: a
stub is emitted AFTER the body and ends by jumping to the loop header, which
reads as a back edge spanning the loop, the epilogue and the stub. The numbers
in that table therefore counted the stub's local-zeroing and the epilogue's
callee-saved restores. Taking the INNERMOST back edge instead:

| arm | loop insns | frame ops | loads | stores |
|---|---|---|---|---|
| door only | 58 | 31 | 17 | 14 |
| + the phi/register stack | 57 | 29 | 15 | 14 |
| + `DROP_PHI_HOME` | 56 | 28 | 15 | 13 |
| + `DROP_HOME` as well | 56 | 28 | 15 | 13 |
| + reserve the carried set (no drops) | 60 | **25** | 11 | 14 |
| **reserve + publish-at-def + drop home** | 56 | **19** | 9 | 10 |

**Reserving the carried set alone is not a regression.** Over the real loop it
takes 31 frame operations to 25 — it removes six RELOADS, exactly what it was
built to do — and the earlier "52 → 60" was the entry stub being counted, which
grows with the number of reserved registers because there are more seeds to
copy. That verdict is withdrawn.

The rest behaves as the design predicts and the census confirms it engaged:
`dropped_values=4 stores_skipped=6 read_refusals=0 def_publishes=2
def_stores_skipped=2` with `resident=5 (gp=5)`. Reserving removes the reloads,
write-through leaves all fourteen stores, and dropping the home removes four of
them and two more loads. **31 → 19 frame operations, a 39% cut**, with the
instruction count also down (58 → 56), and `ck=5100017428506113` — HotSpot's
answer — identical across all six arms.

**And it is worth about 6%.** Four interleaved arms at `n=200,000,000`, nine
rounds, host load 11–17 on 8 cores, with the single-pass arm run twice as its
own control:

| arm | mean ms |
|---|---|
| single-pass OSR | 398 |
| single-pass OSR (control) | 408 |
| optimizing door, switches off | 675 |
| **optimizing door, full stack** | **630** |

The control-vs-control floor is **2.4%**; the full stack beats the door arm by
6.7% and does so in 7 of 9 rounds. Real, and far outside the floor.

**It does not close the gap, and that is the finding.** The tier inversion goes
from 1.68x to 1.56x. Removing 39% of the loop's frame traffic bought 6.7% —
which refutes, with a number, the assumption this whole line of work has been
built on: that the optimizing tier's loops are slow *because* they go through
frame words.

**What the control says instead.** The single-pass body for the same kernel,
counted the same way, is unrolled two ways and runs **~30 instructions per
iteration with ZERO frame operations in the hot path** — `i`, `acc`, `sum` and
`n` live in `r12`, `r13`, `r14`, `r15` from entry to exit, and the only
`[rbp-...]` traffic in its loop is the safepoint poll's spill on the slow side
of a `je`. Against that, the optimizing body is 56 instructions and 19 frame
operations.

So the remaining distance is not one more promotion policy either. Reading what
those nineteen operations are makes the next target concrete:

```text
mov [rbp-0C0h],rax      ; store a value
mov rax,[rbp-0C0h]      ; ...and read the same word straight back
```

They are SINGLE-USE INTERMEDIATES — the result of an `Op::Add` or `Op::And`
that feeds exactly one consumer — written to a frame word and reloaded on the
next instruction. `plan_register_residency` skips every one of them by policy
(`single_use=20` in its census), and it is right to: they do not want a
register. They want not to be spilled at all, which is a question about the
shape of `lower_data_node`'s value model rather than about who gets a register.
That, and the 2:1 instruction count against a tier that unrolls, is what the
1.56x is made of.

Both switches stay OFF pending that.

#### The single-use intermediates, and the frame states that pin them

The previous section ended by naming what the optimizing tier's loop still
spends its frame traffic on: pairs of the shape

```text
mov [rbp-0C0h],rax      ; store the result of an Add
mov rax,[rbp-0C0h]      ; ...and read the same word straight back
```

`plan_register_residency` skips every one of these (`single_use` in its census)
and is right to — their live range is one instruction, so they do not want a
register, they want not to be written to memory. `CRATONVM_JIT_IR_CARRY_SINGLE_USE`
is `fused_cmp`'s move generalised to them: when a value has exactly one use and
its consumer is the very next node in the same block, it stays in the register
the arm computed it in. RAX when the consumer reads it first, RCX when it reads
it second — one register move that still removes a memory access.

Neither half of the contract is trusted. The read refuses unless it is the
planned consumer asking for the planned register, and — for an RAX carry —
unless `buf.pos()` proves nothing was emitted in between, which is a proof
rather than an audit of what the arms do. A carry that outlives its consumer,
or reaches a block boundary or a terminator, refuses too. Both allowlists have
source-scanning tests that check them against the arms they name.

**The first cut planned ZERO carries, and why is the more useful half of this
section.** The screen was `deopt_named`, and the IR graph says every candidate
fails it:

```text
20: Mul : Int <- [13, 19]  bci=Some(16)
safepoint[12] bci=17 locals=[3, 12, -, 13, 14] stack=[20]
safepoint[15] bci=22 locals=[3, 12, -, 13, 14] stack=[20, 22]
safepoint[16] bci=23 locals=[3, 12, -, 13, 14] stack=[23]
```

`graph.safepoints` records the **full operand stack at every bci**, so an
intermediate is named by a frame state from its definition until its consumer
pops it. That is the same wall the residency file hit — its `blocked_deopt`
census, today `[ir-ls] homes kept: deopt=` — reached from a different
direction.

And it is worth reading beside what the door reports for this very method:
`sentinel_free=true`, which is `deopt_stub_patches.is_empty() &&
call_exc_patches.is_empty()` — **this body emits no deopt stub at all**.
Thirty-three frame states, not one of them reachable from inside the code, and
they are what pins every intermediate to memory.

So the change splits in two, and the split is the point. Eliding the LOAD asks
nothing of the frame: the home word is still written, every frame state still
resolves through it, and a read of a word RAX already holds becomes no
instruction. Dropping the STORE keeps the `deopt_named` screen. On this kernel
that is `planned=4 ... stores_dropped=0 still_deopt_named=4`.

| arm | loop insns | frame ops | loads | stores |
|---|---|---|---|---|
| door only | 58 | 31 | 17 | 14 |
| **+ carry alone** | 57 | 27 | 13 | 14 |
| + the residency stack | 56 | 19 | 9 | 10 |
| **+ both** | 55 | **15** | **5** | 10 |

Four carries, four loads gone, `refused=0`, and `ck=5100017428506113` — HotSpot's
answer — in all four arms.

**And this one is unambiguous on the clock.** Five interleaved arms at
`n=200,000,000`, nine rounds, host load ~10–12 on 8 cores, single-pass run twice
as its own control:

| arm | mean ms |
|---|---|
| single-pass OSR | 385.7 |
| single-pass OSR (control) | 385.4 |
| optimizing door, switches off | 657.4 |
| **+ carry alone** | **592.4** |
| **+ carry + the residency stack** | **557.9** |

The control-vs-control floor is **0.08%** — the quietest measurement this file
has recorded. The carry alone is **9.9%** and beats the door arm in **9 of 9**
rounds; with the residency stack it is **15.1%**, also 9 of 9. The tier
inversion goes **1.70x to 1.45x**.

That also revises the previous section's conclusion, in the direction of the
evidence rather than away from it. Frame stores alone did not explain the gap —
39% of them bought 6.7%. But frame *round trips on the critical path* do: these
four loads are each one instruction's distance from the store that fed them, so
every one is a store-forwarding stall in the middle of a serial recurrence, and
removing four of them is worth more than removing twelve stores that nothing
was waiting on.

**What the remaining ten stores are, and the exact rule that would remove
them.** They are home writes for values named only by frame states no deopt can
reach. The precise condition is not "no safepoint names it" but *no safepoint
that names it sits where a deopt can actually be taken* — which is the union of

* **trap bcis**: `Op::Guard`, the `Op::Div`/`Op::Rem` zero guard, calls,
  allocations, array and field access, `checkcast`, the monitor ops;
* **safepoint-poll bcis**: the back-edge terminators, where the runtime can
  transfer a frame out from under the compiled body.

For `OsrTierBench.kernel` the first set is empty and the second is the single
`If` at bci 10, whose stack is `[14, 3]` — a phi and a parameter, neither of
them an intermediate. All four carried values would become droppable.

It is a separate change because it removes a backstop rather than adding one.
`build_deopt_points` currently builds a point for **every** safepoint with a
native anchor, so a dropped home is caught there today by `frame_value_of`
refusing the compile. Making these values droppable means also not building the
unreachable points — at which point the trap/poll classification above is
load-bearing rather than backstopped, and it deserves its own pass.

#### The loop was computing its own return value, every iteration

Reading the whole emitted loop rather than only its frame operations found
something bigger than either of the previous two sections was chasing:

```text
27  mov rax,rbx          ; sum
28  mov ecx,0F4243h      ; 1000003
29  imul rax,rcx         ; sum * 1000003L
30  mov [rbp-0E8h],rax
31  mov rax,[rbp-80h]
32  movsxd rax,eax       ; (long) acc
33  mov [rbp-0F0h],rax
34  mov rcx,rax
35  mov rax,[rbp-0E8h]
36  add rax,rcx
37  mov [rbp-0F8h],rax
```

That is `return sum * 1000003L + acc` — the method's **exit expression** —
computed and discarded on every one of two hundred million iterations. Eleven
of the loop's fifty-five instructions and three of its fifteen frame
operations.

**`ir_schedule`'s own header says a data node is "placed as late as possible
(to minimize register pressure)". It is not.** `find_best_block` picks the
deepest block that all of a node's INPUTS dominate, which is schedule-EARLY. For
a value whose inputs are a loop phi and a constant, that is inside the loop —
whatever its uses do.

`CRATONVM_JIT_IR_SINK_LATE` moves a pure node to the shallowest loop nesting on
the dominator path between where its inputs put it and where its uses need it,
and **only when the depth strictly decreases**. The classic schedule-late also
prefers the latest block at equal depth, to shorten live ranges; that is a
different trade with a different risk, and leaving it out keeps this pass's
effect attributable to the one thing it claims.

The obligation it discharges is the safepoint one. A frame state resolves a
value it names from that value's HOME WORD, and the home is written wherever
the node is emitted — so a moved node must still dominate every block that can
anchor a safepoint naming it. That is checked against the final placement, and a
violation reverts the **whole method** rather than the offending node: reverting
one node can break another's dominance, so undoing the lot is the only revert
that is obviously correct.

| arm | loop insns | frame ops | loads | stores |
|---|---|---|---|---|
| door only | 58 | 31 | 17 | 14 |
| **+ sink alone** | 50 | 25 | 14 | 11 |
| + carry + the residency stack | 55 | 15 | 5 | 10 |
| **+ all three** | **44** | **10** | **3** | **7** |

`moved=3 reverted_for_safepoints=0`, and `ck=5100017428506113` — HotSpot's
answer — in all four arms.

**Priced in CPU time, which is what a contended host leaves usable.** Two
wall-clock runs at load 23–36 produced control-vs-control floors of 3.7% and
10.0% — unusable for a magnitude, though the paired counts held (the sink arm
beat its door arm in 17 of 18 rounds). User CPU over the same five interleaved
arms, nine rounds:

| arm | user CPU (s) |
|---|---|
| single-pass OSR | 0.564 |
| single-pass OSR (control) | 0.568 |
| optimizing door, switches off | 0.812 |
| **+ sink alone** | **0.743** |
| **+ sink + carry + the residency stack** | **0.621** |

The floor is **0.60%**, and every comparison separates in **9 of 9** rounds:
the sink alone is **8.5%**, all three together **23.5%**. In CPU time the tier
inversion is **1.435x → 1.097x** — the optimizing tier is now within ten per
cent of the single-pass body it was 1.7x behind when this arc started. (The
wall-clock figures earlier in this file are wall clock; on this host CPU time is
the instrument that survives the load, and it puts the door arm at 1.435x rather
than 1.70x. Different instruments, not a correction.)

**Engagement outside the probe is small, and that is the reason it stays OFF.**
Across forty regression-suite classes and 112 optimizing compiles, exactly
**one** compile sank anything, with zero reverts. The pass is correct and free
when it does not fire, but a default-on codegen change wants evidence broader
than one constructed kernel, and this is a targeted fix for a specific shape:
a loop whose method computes something after it.

#### The constant operands, and the end of the tier inversion

Every binary arithmetic arm in `ir_lower` reads its second operand through
`gp_load_value(RCX, ..)` and then works register-to-register. When that operand
is a constant, `ir_const_imm` turns the read into `mov ecx, imm` — so the
emitted code carries a whole instruction per constant operand that x86 has an
addressing form for:

```text
mov ecx,1Fh  / imul eax,ecx        becomes   imul eax,eax,1Fh
mov ecx,1    / add eax,ecx         becomes   add eax,1
mov ecx,0FFh / and rax,rcx         becomes   and rax,0FFh
```

`CRATONVM_JIT_IR_ALU_IMM` folds it. Nine arms, three encoders: the accumulator
short form for ADD/SUB/AND/OR/XOR (one column of the opcode map, so one helper),
`IMUL r, r/m, imm32`, and `C1 /digit ib` for the shifts.

**Unlike everything else in this arc it is not shaped like a loop.** The
residency work, the carry and the sink each need a particular structure to fire
— a loop-carried value, an adjacent single-use consumer, work that outlives its
loop. This is every `x + 1`, `x & 0xFF` and `x * 31` in every compiled method.

Two details are load-bearing. The shift count is masked **here**, to 5 bits for
`ishl`/`ishr`/`iushr` and 6 for the `l` forms: x86 masks a shift count the same
way, which is exactly what makes the existing `CL` form correct without a mask,
and an immediate form that inherited that assumption silently would be a
coincidence rather than a reason. And a `long` constant outside `i32` falls back
to the register form, because every immediate form here SIGN-EXTENDS its
`imm32` — `0xFFFFFFFFL` is the case that separates a correct fallback from a
truncating one. `probes/AluImmProbe.java` checks both against HotSpot, along
with negative shift counts and the two's-complement edges; it agrees in all five
arms, `--nojit` included.

Only the SECOND operand folds, never the first, even for the commutative ops.
`gp_load_value(RAX, node.inputs[0])` being unconditional is what
`op_reads_rax_then_rcx` and the carry's RAX contract rest on — and it is what
keeps this change and the RCX carry disjoint by construction, since a carry to
RCX is planned only when `inputs[1]` is the carried node, which is never a
constant.

| arm | loop insns | frame ops | loads | stores |
|---|---|---|---|---|
| door only | 58 | 31 | 17 | 14 |
| + the fold alone | 53 | 31 | 17 | 14 |
| + everything else | 44 | 10 | 3 | 7 |
| **+ everything** | **40** | **10** | **3** | **7** |

#### The loop control itself, 2026-09-11

The fold above reaches every `x op k` in a method, and the residue it left
behind was the three instructions at the bottom of every counted loop. Two
changes finish it. Both are default-ON with a kill switch, both strictly remove
instructions **and** bytes wherever they fire, and neither resolves on this
build host's clock — which is said here rather than dressed up.

**A fused compare reads its operands where they already are.** A compare whose
only consumer is the `If` defines no value, writes no home and publishes no
register; the only thing that outlives it is the flags, and those are the same
whichever registers or addresses the comparison names. So it names them:

```text
mov rax,rbx / mov rcx,r12        / cmp eax,ecx    becomes   cmp ebx,r12d
mov rax,rbx / mov rcx,[rbp-60h]  / cmp eax,ecx    becomes   cmp ebx,[rbp-60h]
mov rax,rbx / mov ecx,64h        / cmp eax,ecx    becomes   cmp ebx,64h
mov rax,[rbp-60h] / mov ecx,64h  / cmp eax,ecx    becomes   cmp [rbp-60h],64h
```

Four forms, `pick_cmp_form` in that order of preference. The two immediate forms
are the common ones — `i < 100` is the shape of most Java loops — and the two
frame forms are not fallbacks: `peak_live` routinely exceeds the five-register
GP file, and a loop bound is exactly the long-lived value that loses its
register.

Three guards, each of which fails closed rather than wrong. `carry_names`
declines either operand a carry is holding, because a carried value has to be
read through `gp_load_value` or the carry strands. The frame forms go through
`slot_of_checked`, so a dropped home declines the form rather than latching a
bailout on a path with a perfectly good fallback. And the immediate forms gate
on `alu_imm32`, which is the same gate the arithmetic folds use: it declines a
constant too wide for `i32` (every immediate form here sign-extends, so such a
constant has no immediate encoding at all) and it is off under a MIR mode, where
a tiled node is emitted by the selector and a fold here would leave the
byte-equality lane comparing two different programs.

The 32-bit frame forms read four bytes where the `MOV` they replace read eight.
That is the same comparison: the slot holds the `int` in its low word, and the
`CMP EAX, ECX` being replaced only ever looked at those four bytes either.

**`x + k` and `x - k` become one `LEA`.** `LEA` is the only three-operand
integer instruction on this machine, so it is the only way to read a source and
write a different destination without routing through the accumulator:

```text
mov rax,rbx / add eax,1 / mov r14,rax     becomes   lea r14d,[rbx+1]
mov rax,rbx / add eax,1                   becomes   lea eax,[rbx+1]
```

Two forms, and **the weaker one is the common case**, which is the thing to know
about this change. Whether the first is reachable turns on whether the result
got a register of its own, and a loop-carried increment does not: measured on
`CmpImm.wide`, `def_publishes=0` against `phi copies: reg_publishes=10` — the
loop-carried values are published by the phi copies on the back edge, so `i + 1`
writes a home word and is given no register. A first version that required one
engaged **nowhere** on that probe. `PollReach.hotLoop` is where the direct form
does fire (`add_lea=1+0`), and it is worth two instructions there rather than
one.

Unlike the compare this DEFINES a value, so the direct form owes everything a
definition owes, and `every_droppable_op_writes_its_home_once_through_store_rax`
had to grow an exception for it — the one home write in a claimed arm that does
not go through `store_rax`. The exception is named in that test and proved in
`the_lea_add_form_publishes_what_it_does_not_store`, which reads the direct
form's source and requires that it publish the register and say so BEFORE it
decides whether to skip the home store. The accumulator form needs none of that:
it leaves RAX holding exactly what `mov rax,x; add eax,k` would have, and
`store_rax` finishes unchanged.

`x - k` is `x + (-k)` through the same encoder, except at `Integer.MIN_VALUE`,
whose negation is not an `int`. One constant in the language, and it declines
rather than wrapping into a silent `+ MIN`.

**Measured — instructions and bytes yes, time no.** Release binary, the two
flags as the A/B, first optimizing-tier compile of each method:

| probe | both off | cmp only | lea only | **both on** | forms that fired |
|---|---:|---:|---:|---:|---|
| `CmpImm.wide` | 186 / 923 | 184 / 919 | 185 / 918 | **183 / 914** | `cmp_imm=1+0 add_lea=0+1` |
| `CmpImm.down` | 186 / 919 | 184 / 912 | 185 / 914 | **183 / 907** | `cmp_imm=1+0 add_lea=0+1` |
| `LoopCtl.spin` | 189 / 933 | 187 / 927 | 188 / 928 | **186 / 922** | `cmp_in_place=0+1 add_lea=0+1` |
| `PollReach.hotLoop` | 200 / 1190 | 198 / 1185 | 198 / 1183 | **196 / 1178** | `cmp_in_place=1+0 add_lea=1+0` |
| `PollReach.wideLoop` | 268 / 1637 | 266 / 1631 | 267 / 1632 | **265 / 1626** | `cmp_in_place=0+1 add_lea=0+1` |

instructions / bytes. The four arms are additive to the instruction, which is
what says the two levers are disjoint. Bytes fall in every arm — the point worth
contrasting with the operand-pairing pass, which bought its one instruction for
three extra bytes and was rejected.

All four compare forms engage somewhere: the register-immediate form on
`CmpImm.wide`, the frame-immediate form on `CmpImmProbe` (`cmp_imm=0+2` on one
compile) and on `CmpImm.tight` (`1+1`), the register-register form on
`PollReach.hotLoop`, the register-frame form on `LoopCtl.spin`.

**The timing is a null on this host and no speedup is claimed.** Three arms
interleaved ABCCBA, A and C the SAME build, so the A-C spread is the floor:

| probe | rounds | floor (A vs C) | effect (B→A) |
|---|---:|---:|---:|
| `CmpImm.wide` | 12 | 1.43% median / 1.18% min | −2.62% median / +3.05% min |
| `LoopCtl.spin` | 12 | 5.69% median / 1.55% min | +3.19% median / +3.49% min |
| `CmpImm.wide` | 20 | **14.63%** median / 0.07% min | +9.35% median / **−5.28%** min |
| `LoopCtl.spin` | 20 | 3.11% median / 2.74% min | +22.22% median / +0.40% min |

Host load ran 40-111 on 8 cores across those runs, and it shows: two identical
builds come out 14.63% apart, the measured "effect" ranges from −5.28% to
+22.22%, and the median and the minimum disagree about its SIGN. **That is a
null, not a small win**, and adding rounds made it worse rather than better
because the load rose faster than the averaging helped. A quieter box is what
this needs; the instruction and byte counts above need nothing, being exact.

**One mechanism worth writing down, because it is the only argument AGAINST the
`LEA`.** `mov rax,rbx` is eliminated at rename on every current x86-64, so the
instruction the accumulator form removes was very likely already free. On Intel
`LEA` also issues on fewer ports than `ADD` (1 and 5, against 0/1/5/6), so in a
port-1/5-bound loop it could in principle cost a cycle it does not spend. **Not
on this host** — an AMD EPYC 9V45 (Zen 5), where the simple base-plus-
displacement form runs on all four ALUs — but the flag is not host-specific
and the next machine may be. So: the `LEA` removes an instruction and five bytes
but probably not a uop. It ships ON for the decode and I-cache saving, which is
not in doubt, and `CRATONVM_JIT_IR_ADD_LEA=0` is the way back. The compare has
no such counter-argument — `mov ecx,imm` is a real uop that no renamer
removes.

**Verified.** 2366 `cratonvm-jit` unit tests and 2645 `cratonvm-vm` unit tests in
debug, so `debug_assert` is live; 145 `ir_vs_singlepass` differential tests. The
regression suite **92/92 with the new defaults and 92/92 with both kill
switches** — both directions, because a switch nobody exercises is not a switch.
`probes/CmpImmProbe.java` agrees with HotSpot to the checksum under the
defaults, under each kill switch, under both, under `CRATONVM_JIT_IR_ALU_IMM=0`,
under `CRATONVM_JIT_IR_LINEAR_SCAN=0` and under `--nojit`; it covers the
`imm8`/`imm32` boundary in both signs, negative bounds, a `long` constant outside
`i32` that no immediate can express, `Integer.MIN_VALUE` as a bound and as an
addend, a first operand forced out of its register, and a reference against
`null`.

`alu immediates folded: 5`, and `ck=5100017428506113` in every arm.

**And this is where the arc ends.** User CPU, five interleaved arms, nine
rounds, single-pass run twice as its own control:

| arm | user CPU (s) |
|---|---|
| single-pass OSR | 0.586 |
| single-pass OSR (control) | 0.583 |
| optimizing door, switches off | 0.916 |
| + the fold alone | 0.837 |
| + everything except the fold | 0.663 |
| **+ everything** | **0.579** |

The floor is **0.39%**. The fold alone is 8.6% (8 of 9 rounds) and 12.7% on top
of the rest (9 of 9). Within this run the tier inversion goes **1.566x to
0.990x** — the optimizing tier now **matches** the single-pass body on this
kernel, where it started 1.7x behind.

Parity, not a win: 0.990 is inside the spread between the two single-pass arms,
so the honest statement is that the gap this whole section set out to explain is
gone rather than reversed. Compare within a run and not across runs — the door
arm alone measured 0.812 s earlier the same day and 0.916 s here, which is more
drift than several of the effects being measured.

**What it took, in order of size:** taking work out of the loop that was never
loop work (the sink, 11 instructions), not spilling values whose live range is
one instruction (the carry), folding constant operands (this, 5), and only then
the register residency this section spent three days on. The frame traffic fell
from 31 operations to 10, and the instruction count from 58 to 40 — and the
first of those was worth less than the second, which is the opposite of the
premise the work started from.

**Everything above is still default OFF.** Eleven switches, every one measured
positive on this kernel, none of them defaulted on — because the engagement
censuses say what a single kernel cannot: the sink fires on 1 optimizing compile
in 112 across the regression suite. Defaulting these on wants a measurement on a
real workload, and that is the next thing this file should record.

#### Turning them on, and the two bugs that only appeared when they met

Thirteen switches, every one of them measured on `OsrTierBench.kernel` and
every one of them shipping OFF:

| switch | what it does |
|---|---|
| `CRATONVM_JIT_OSR_OPTIMIZING` | the OSR door reaches the optimizing tier |
| `CRATONVM_JIT_IR_OSR_ENTRY` | the entry stubs that door needs |
| `CRATONVM_JIT_IR_DEOPT_REGS` | the register image a deopt frame reads |
| `CRATONVM_JIT_IR_PHI_COPY_REGS` | edge copies through registers |
| `CRATONVM_JIT_IR_SKIP_REPUBLISH` | no reload of a live register |
| `CRATONVM_JIT_IR_PUBLISH_AT_DEF` | publish from RAX at the store |
| `CRATONVM_JIT_IR_DROP_PHI_HOME` | a phi with no home word |
| `CRATONVM_JIT_IR_DROP_HOME` | the same for ordinary values |
| `CRATONVM_JIT_IR_RESERVE_CARRIED` | a register each for the carried set |
| `CRATONVM_JIT_LS_CARRY_RELIEF` | price a carried value's eviction (0 → 64) |
| `CRATONVM_JIT_IR_CARRY_SINGLE_USE` | one-instruction live ranges stay in a register |
| `CRATONVM_JIT_IR_SINK_LATE` | pure work out of loops it is not used in |
| `CRATONVM_JIT_IR_ALU_IMM` | constant operands folded into the ALU op |

All thirteen are now ON, each keeping `=0` as its kill switch. The three that
were tested with `runtime_var_os(..).is_some()` now read the VALUE, because
presence alone cannot express an off word.

**Flipping them found two bugs that no arrangement of them one at a time
could.** Both were caught by `cargo test -p cratonvm-jit`, and neither was a
stale expectation:

* **A carried value and a dropped home refused each other.**
  `lower_data_node_tracked` bailed on any value whose home was dropped and
  whose lowering published no register — and a carried value never publishes
  one, because being left in RAX or RCX for its single consumer *is* the
  contract. The probe this was all built on could not reach it: every
  intermediate there is named by a frame state, so no carried value's home was
  ever dropped and the two mechanisms never met. Two hand-built lowering tests
  with no safepoints at all did meet them, and failed **closed** —
  `n5's home was dropped but its lowering published no register` — rather than
  emitting anything wrong.
* **The level-2 machine list drifted from the arms it is measured against.**
  `mir_emitted_bytes` already forced residency off for *both* arms of its
  comparison, with the reason written down: a comparison that left it on for
  one arm would measure residency rather than the selector. The carry and the
  folded immediates are two more of exactly that. They also now refuse under a
  MIR mode outright, and not merely for the lane's convenience — in
  `MirMode::Emit` a tiled node is emitted by the SELECTOR and its arm never
  runs, so a mix is genuinely broken rather than different: an arm could start
  a carry its tiled consumer never reads, or fold an immediate the tiler then
  re-materialises.

Two tests also had to stop asserting a default and start asserting a property.
`the_osr_entry_kill_switch_emits_no_stub` and the deopt-region test now reach
for a force-off, because **a default-on codegen change is only as good as its
way back**, and the way back is the thing worth pinning.

**Engagement, with nothing set at all** — this is the census on a plain run,
which is what "default on" has to mean:

```text
osr optimizing OsrTierBench.kernel pc=7: stub=true entries=[7] sentinel_free=true
[ir-ls] resident=5 (fp=0 gp=5) scan_promoted=20 peak_live=13
[ir-ls] carries: planned=4 taken=4 read=4 refused=0 stores_dropped=0 still_deopt_named=4
[ir-ls] homes: dropped_values=4 stores_skipped=7 read_refusals=0 def_publishes=1
[ir-ls] alu immediates folded: 5
[ir-sink] moved=3 reverted_for_safepoints=0
```

**Verified.** `cargo test -p cratonvm-jit` and `-p cratonvm-vm` in debug, so
`debug_assert` is live. The regression suite **90/90 with the new defaults and
90/90 with every kill switch set** — both directions, because a switch nobody
exercises is not a switch. `AluImmProbe` and `OsrTierBench` agree with HotSpot
under the defaults, under every kill switch, and under `--nojit`.

**What is still owed, and it has not changed.** Every number in this section
comes from one six-line kernel. The engagement censuses say what that kernel
cannot: the sink fires on **1 optimizing compile in 112** across the regression
suite. These are on now because they are correct, reversible and free where
they do not fire — not because a real workload has been measured. That
measurement is the next thing this file should record, and until it does, the
right reading of the parity result is "on this kernel", not "in general".

#### An inline cache that installed, and then stopped being used

Defaulting the optimizing-tier switches on made
`test_inline_cache_takes_over_the_sam_call_site` fail intermittently — 3 runs
in 46, never in 60 with the switches off, always at ~398 000 of 800 000
dispatches "still going through the Rust arm". The number's tightness across
occurrences (398568, 398618, 398496, 397002) said race, not latency drift.

**Two theories died before the right one, and both are worth recording because
each looked conclusive.**

* *The optimizing OSR artifact has no inline-cache slots.*
  `compile_optimizing_artifact` mentions `ic_slots`, `mic_slots` and
  `pic_slots` exactly zero times, which reads as a smoking gun. It is not: the
  function routes through `try_compile_with_invokespecial_resolver` into
  `try_compile_inner`, and *that* builds `ir_ic_slots`. A grep over one
  function is not a call graph.
* *The door rebuilds an optimizing artifact it will refuse, delaying OSR
  entry.* True, and worth fixing on its own — `ir_osr_sentinel_free` requires
  no deopt stub and no call-exception stub, so **any method containing a call
  is refused**, and the door was paying a full optimizing compile per OSR
  attempt for most of them. But memoizing that refusal **did not change the
  failure rate** (3 in 40). A real inefficiency, not this bug.

**What it actually was.** Adding the counters *as of the instant of install* to
the `lambda-adapter installed` line settled it in one run:

```text
lambda-adapter installed ... at site_direct=0 fast_returns=2962
lambda-adapter installed ... at site_direct=1 fast_returns=3198
RESULT: 397002 of 800 000 dispatches still went through the Rust arm
```

Both thunks installed **immediately** — and the site still served 397 002 calls
from Rust afterwards. The feature engaged and then stopped working, which is
why `site_adapters=2` looked healthy the whole time.

`claim_adapter_install` latched a bare `bool` for the life of the process. The
MIC/PIC slots it fills, though, belong to the **caller's compiled body** — and
callers get recompiled: C1 then C2, or an OSR body published beside the entry
one. The new body's slots are fresh and empty, and a site already latched
`true` can never fill them, so every dispatch after the recompile falls back to
Rust permanently.

The latch is now the **slot pair** rather than a bool. A repeat of the same
pair still refuses — that is the 202 000-re-install case the latch was added
for, and it is unchanged — while a different pair claims once more. Re-installs
are bounded by the number of distinct compiled bodies, which is small, instead
of by the number of calls, which is not.

| build | failures |
|---|---|
| `dev` with the defaults on | 3 / 46 |
| + the refusal memo alone | 3 / 40 |
| **+ the slot-keyed latch** | **0 / 108** |

**The bug predates the defaults.** Nothing in the optimizing tier caused it;
turning the switches on merely made caller recompilation likely enough to
expose it, and the test was the only thing in the tree sensitive enough to
notice. Any workload whose lambda call site sits in a method that tiers up has
been losing its inline cache at the tier-up boundary.

#### The frame states nothing can reach, and how to drop them without removing the net

The stores this section kept naming as the residual were home writes for values
pinned by frame states. `graph.safepoints` records the **full operand stack at
every bci**, so an intermediate is deopt-named from its definition until its
consumer pops it — which is what the home census's `deopt` cause (then
`plan_register_residency`'s `blocked_deopt`) and the carry's
`still_deopt_named` refuse on. Meanwhile the OSR door reports
`sentinel_free=true` for the same method: it emits **no deopt stub and no
call-exception stub**, so nothing inside it can transfer to the interpreter.
Thirty-three frame states, not one reachable, all of them pinning intermediates
to memory.

**The obvious implementation is the dangerous one.** Stop building the
unreachable points and the trap classification becomes load-bearing: misclassify
one op and a deopt reconstructs a confidently wrong value — the failure mode
this area produces, and the reason this was split out of the perf arc rather
than done inline.

So this does the opposite. It **predicts** trap-freedom from the graph to decide
what to drop, and then **verifies the prediction against the emission that
actually happened**. `lower_inner` computes `osr_sentinel_free` from
`deopt_stub_patches` and `call_exc_patches` *before* `build_deopt_points` runs,
and a point is skipped only when the graph prediction and the emission agree.

If they disagree, every point is built, `frame_value_of` meets a dropped home it
cannot describe, and refuses the compile — the behaviour with this switch off,
unchanged. **Nothing is taken away; a case is added in which the net is provably
not needed.** `op_cannot_deopt` is an allowlist, so an op this file has never
heard of counts as trapping.

That direction was not theoretical. The first cut omitted `Op::Return`, so
`OsrTierBench.kernel` — pure arithmetic and a return — reported
`graph_trap_free=false` and **declined itself**: `homes_freed=0`, loop unchanged.
A missing entry costs an optimization, never a value.

With it fixed:

```text
[ir-ls] unreachable frame states: graph_trap_free=true homes_freed=4 points_skipped=33
[ir-ls] carries: planned=4 taken=4 read=4 refused=0 stores_dropped=4 still_deopt_named=0
[ir-ls] homes:   dropped_values=8 stores_skipped=7 read_refusals=0
```

| arm | loop insns | frame ops | loads | stores |
|---|---|---|---|---|
| the arc as merged | 40 | 10 | 3 | 7 |
| **+ unreachable homes dropped** | **37** | **7** | 3 | **4** |

`still_deopt_named` goes 4 → 0 and the seven stores this section has been
pointing at become four.

**Correctness is shown where the loop kernels cannot show it.** They are
trap-free, so no deopt is ever taken and a frame state that reconstructed
garbage would never be consulted — a green run there is agreement about a path
nobody took. `probes/DeoptLiveProbe.java` is the other half: three hot loops
that really trap part-way through, with an intermediate live across the trap
point (a zero divisor, a null receiver, an out-of-bounds index), each folding
into a checksum that depends on the values live at the deopt. It agrees with
HotSpot under the default, with the switch on, with the switch on plus the OSR
door, and under `--nojit`.

**On the clock it is a null result, and that is the right size.** Paired user
CPU, each arm run twice per round over fifteen rounds at host load 5-8:
`before` 0.4840 s, `after` 0.4850 s — 0.2% apart, against a same-config control
floor of **2.2-2.9%**. Three instructions and three frame operations out of 40
and 10 is roughly 7% of the loop, and this host cannot resolve that. An earlier
nine-round pass appeared to show a 16% regression; it was small-sample scatter
in a bimodal distribution and did not survive pairing. The counted change is the
result here — the stopwatch has nothing to add at this size.

(The first run of that comparison passed **vacuously** — the probe was not yet
on the remote worktree, HotSpot printed nothing, and empty matched empty. The
script now refuses to compare against an empty oracle. A differential harness
that cannot fail is worth less than no harness, because it reports success.)

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

---

## 2026-09-06 — the seven-item pass: C2 was a SMALLER optimizer than C1

### The audit that opened it

An audit asked why the optimizing tier delivers no measurable gain, and found a
different answer from the one this file's tier-inversion arc had been chasing.
That arc was about the BODY — frame traffic, register residency, the latency of
a store-then-load chain — and it took `OsrTierBench.kernel` from 1.70x behind to
parity. This one is about the OPTIMIZER, and the finding is structural:

**C2 is not a superset of C1.** Tracing which modules each analysis is reachable
from, on `dev@91d3077a9`:

| Optimization | C1 single-pass | C2 optimizing (before this pass) |
|---|---|---|
| Bounds-check elimination (`range_analysis` + `x64/bce.rs`) | default on | **absent** |
| Null-check elimination (`null_check_elim`, the `this` seed) | default on | **absent** |
| Runtime-trip loop unrolling (`x64/loop_unroll_admission.rs`) | 4x | const-trip only |
| Method inlining (`x64/inlining.rs` + guarded virtual) | default on | **off by default** |
| Call-site intrinsics (~40) | ~40 | **0, and refuses the method** |
| Vectorization (`x64/simd_analysis.rs`, `x64/vec_emit.rs`) | present | **absent** |
| Loop-carried values in registers | default on | write-through cache |
| Escape analysis / scalar replacement | bytecode-walk | SSA graph |
| SSA GVN, LICM, sink-late, ALU-imm folding | partial | full |
| Exception-handler bodies | compiled | skipped |

`range_analysis` is referenced by `x64/bce.rs` and nothing else;
`null_check_elim` by `x64/*` and nothing else. So
`ir_lower::emit_array_null_bounds_guards` emitted a null test, a length load and
a bounds compare at EVERY `ArrayLoad` and `ArrayStore`, every iteration, with no
elision and no proof — in the tier that is supposed to be the optimizing one.

**And the tier consumed no profile.** `CRATONVM_TIER_PGO` has never shipped on,
so `MethodProfile::branches` is empty and the `ir_branch_hints` map
`lower_inner` receives is empty in every default run. Worse, the production
pipeline called `ir_schedule::schedule(&graph)`, which is
`schedule_with_options(graph, &ScheduleOptions::default())` — and `Default` is
documented as "the historical scheduler exactly: no profile, no reordering,
dependence-order-only intra-block scheduling". Two of the three stages that
module implements were off in every compile the VM has ever run.

### The measurement that made it a defect rather than an observation

H2 JDBC (`probes/DodJdbcWorkload.java` against the local h2corpus), three arms
interleaved run-by-run with the order reversed on alternate reps, the same
configuration run twice as its own noise floor, process CPU time. Windows
workstation, 32 logical cores, host load ~76% from other sessions — so only
within-run ratios are readable, which is why the control pair is there.

| arm | run A median | run A min | run B median | run B min |
|---|---:|---:|---:|---:|
| C2 on (default) | 3.938 | 3.594 | 4.734 | 4.109 |
| C2 on (control) | 3.953 | 3.516 | 4.547 | 3.922 |
| **`CRATONVM_C2_SUPERSEDE=0`** | **3.703** | **3.297** | **4.234** | **3.891** |

Control-vs-control floor 0.4% and 4.1%. `c2-off` is the fastest arm in both
runs, on median AND minimum. About a quarter of the difference is background
compile CPU (161 extra compiles, `total_compile_time_ms` 201 against 143); the
rest is the published bodies and the supersede epoch bump.

The size census says the same thing from the other side. Over 161 supersedes:
**81 C2 bodies larger** than the C1 body they replaced, 66 smaller, 14 the same,
519,344 bytes against 488,834 — **6% larger in aggregate**. A tier that neither
inlines nor unrolls cannot explain a larger body by having done more work.

### The refusal census: one site refused the whole method, five times over

Reach at the method-entry door was never the problem — 164 of 199 C1 compiles
reach C2 on H2. What was lost, was lost to one design decision repeated at five
sites: when the IR front end meets something it cannot lower, it discards the
entire method rather than the site.

| refusal | count | what it was |
|---|---:|---|
| `ir.rs:7150` — invoke with no `invoke_info` | 64 | downstream of the discard below |
| invoke plan discarded whole | **68** | **100% "one call-site intrinsic somewhere in the method"** |
| `invokedynamic` anywhere | 53 | C1 lowers indy to an uncommon trap and compiles the rest |
| opcode `0x53` (`aastore`) | 35 | no IR lowering, though `jit_aastore` exists |
| unresolved `checkcast` / `instanceof` | 34 | a class that has never been loaded |
| `new` of a not-yet-loaded class | 13 | an exception construction on a path that never runs |

The intrinsics were `Math.max` (7), `System.arraycopy` (7), `Long.longValue`
(7), `AtomicLong.get` (7), `Long.numberOfLeadingZeros` (4), `Math.min` (2),
`Integer.compare` (2), `Long.compare` (2) and a long tail.

And the deferred-`new` retry sweep, which runs on every class definition,
re-resolved **8,131 sites over 101 distinct ones** in a single run — the top one
3,905 times, a `new java/nio/charset/MalformedInputException` on a decoding
error path inside `java/lang/String`, for a class the program never loads
*because* that path never runs.

### What landed

Seven changes, each with a kill switch and an engagement census.

| # | change | default | kill switch |
|---|---|---|---|
| 1 | IR-tier inlining | **ON** | `CRATONVM_JIT_IR_INLINE=0` |
| 2a | IR null-check + bounds-check elimination | **ON** | `CRATONVM_JIT_IR_CHECK_ELIM=0` |
| 2b | unroll over unreachable frame states | **ON** | `CRATONVM_JIT_IR_UNROLL_UNREACHABLE_FRAMES=0` |
| 3a | scalar intrinsics lowered as arithmetic | **ON** | `CRATONVM_JIT_IR_SCALAR_INTRINSICS=0` |
| 3b | uncommon trap at an `invokedynamic` site | **ON** | `CRATONVM_JIT_IR_SITE_TRAP=0` |
| 3b' | ...at an unresolved `checkcast`/`instanceof`/`new` | **OFF** — the coldness argument was refuted; see below | `CRATONVM_JIT_IR_UNRESOLVED_CLASS_TRAP=1` |
| 3b" | ...only where the trap could be RESUMED | **ON** (added 2026-09-07) | `CRATONVM_JIT_IR_TRAP_REPLAY_GUARD=0` |
| 3c | `aastore` through `jit_aastore` | **ON** | `CRATONVM_JIT_IR_AASTORE=0` |
| 4a | frequency block layout + list scheduling | **ON** | `CRATONVM_JIT_IR_HOT_LAYOUT=0`, `CRATONVM_JIT_IR_LIST_SCHED=0` |
| 4b | branch profile window around a C2 nomination | **ON** | `CRATONVM_TIER_PGO_C2_WINDOW=0` |
| 5 | `ScalarIntrinsic` + `ArrayLoad` may drop their home | **ON** | (rides `CRATONVM_JIT_IR_DROP_HOME`) |
| 6 | C1→C2 acceptance gate | `evidence` | `CRATONVM_C2_ACCEPT=always` |
| 6b | a refused supersede abandons the publish | **ON** | rides the gate; `CRATONVM_C2_ACCEPT_MEMO=0` for the memo half |
| 7 | deferred-`new` look budget | **16** | `CRATONVM_JIT_DEFERRED_NEW_LOOKS=0` |

Four of them are worth reading for their reasoning rather than their effect.

**The trap is not a new mechanism, it is `Op::Guard` with a constant-zero
condition** — the shape `add_div_zero_guard` already builds. Its lowering
already emits `DeoptReason::UncommonTrap` with `DeoptAction::Reinterpret`, so
the interpreter re-runs the bytecode at that bci, which is exactly what a site
this tier declined to compile needs. Each caller owes a coldness argument and
two of the three have a real one: an unresolved `new`/`checkcast`/`instanceof`
names a class that **has never been loaded**, and a class that has never been
loaded cannot have been touched by any path that has executed. That is a proof.
`invokedynamic` has no such proof, and is on the list because the single-pass
backend has made exactly this trade by default since it stopped bailing on indy.

**A trap the interpreter cannot get back from is not a slow path (2026-09-07).**
That last sentence — matching the single-pass backend's indy trade — copied the
trade without its precondition, and the precondition is the whole thing. The
single-pass `0xba` arm checks the snapshot it just built and bails the entire
compile (`mark_codegen_unencodable("unresumable-indy-trap")`) when the trap
could not be resumed. This tier needs that check MORE, not less: `x64::driver`
sets `can_deopt_resume = !deopt_points.is_empty() && !has_elided_monitor`, while
`ir_lower` sets it only on the scalar-replacement path, so on a production
artifact an optimizing-tier deopt has exactly one fallback — the interpreter's
whole-method replay from entry — and that replay is refused, fatally, once the
bytecode before the trap has committed something a re-run would duplicate.

`IrBuilder::trap_replay_is_safe` now asks the CONSUMER's own predicate
(`replay_from_entry_is_observably_equivalent`) before planting, and a refusal
returns `false`, which the callers already turn into `ir_build_bail` — so the
method falls back to the single-pass backend rather than going uncompiled.
`ir_trap_refusal_census()` counts refusals by cause beside `ir_trap_census()`'s
plants.

Note that `invokedynamic` is `0xba` and `opcode_commits_side_effect` commits the
whole `0xb6..=0xba` invoke range, so a body containing an indy can never satisfy
the whole-body clause: every indy trap is decided by the prefix before it.

It costs nothing measurable. An indy trap is UNCONDITIONAL, so an optimizing
body whose live path reaches one pays a deopt on every call and is strictly
worse than the single-pass body it superseded; declining it hands the method
back to a tier that runs it. Measured ABBA-interleaved over 24 Spring classes
(673 test methods) on one binary: guard ON 294.9 s / 286.6 s, guard OFF
316.4 s / 336.9 s — no overlap, and the fastest slot is the last one, so host
drift cannot explain the ordering.

What it is worth, measured on the same day's dev tip and AFTER both deopt-sink
resume fixes had landed: on the 56-class Spring Framework cluster, switching it
off costs **20 classes and 296 test methods** (53 OK / 1 FAIL / 2 TIMEOUT
becomes 34 OK / 21 FAIL / 1 TIMEOUT). Confirmed ABBA-interleaved over six of
them, byte-identical between repeats of each arm. The sink fixes make an
unresumable trap *recoverable*; not planting it is what stops these classes
failing.

Before any of the three fixes, every in-process javac compile under Spring's
`TestCompiler` died with `InternalError: precise deoptimization unavailable
... refusing side-effecting replay` (javac catches it, prints its own banner to
stderr and returns `false` with an empty `DiagnosticListener`, which reads as a
compile that failed with no diagnostics), and every H2 CRASH class in the
2026-09-07 3-arm run died the same way. See the retired
`testcompiler-injit-mode-silent-compile-failure-19-class-aot-cluster` and
`precise-deoptimization-unavailable-cross-suite-crash` write-ups.

**The intrinsics split by what the intrinsic replaces, not by convenience.**
Letting an unlowerable intrinsic site take an ordinary `Op::Call` is a
DOWNGRADE — a dispatch is a ~300 ns floor where the single-pass backend emits
two instructions — so the families whose emitted form is a handful of
straight-line instructions (`Math.min`/`max`/`abs`, `Integer.compare`,
`Long.compare`) are lowered as ARITHMETIC through one new op,
`Op::ScalarIntrinsic`, and the families whose intrinsic replaces a LOOP
(`arraycopy`, `Arrays.fill`/`equals`/`sort`, `String.equals`/`indexOf`, CRC32)
keep the method-level refusal. `scalar_intrinsic_census` counts both halves, and
the refused count is the work list for whoever extends the first.

One op with a sub-enum rather than eight ops, because every new `Op` has to be
added to `ir_verify`'s arity table, `regalloc::ir_op_defines_value` and
`ir_lower::op_defines_result_slot` — and the last two are the pair
`the_two_value_defining_enumerations_agree` exists to keep in lockstep after a
drift that silently disabled register residency on every array-touching method.

**The unroller's blocker was never sizing.** It refuses any loop whose body is
named by a safepoint snapshot, because a `SafepointSnapshot` is keyed by one
`bci` and `trip` copies of a body bci cannot be represented by one snapshot.
That reasoning is correct and it is unchanged. What it did not ask is whether
any of those snapshots is REACHABLE — and on a trap-free graph none is, which is
the same fact `lower_inner`'s `graph_trap_free` already computes and acts on
when it skips building unreachable deopt points. The relaxation is a
PREDICTION, and it carries its own net: `UNROLL_USED_UNREACHABLE_FRAMES` is read
in `lower_inner`, and if any deopt point was in fact built the compile is
refused and the method takes the single-pass backend — which is what it did
before the relaxation existed.

**The acceptance gate is a policy and says so.** `ir_evidence` records which
transforms a compile applied, and `CRATONVM_C2_ACCEPT=evidence` (the default)
publishes only a body that applied one the baseline tier has no equivalent for:
scalar replacement, splicing, guard elision, late sinking, a scalar intrinsic.
`Unrolled` and `Licm` are deliberately absent — the single-pass backend has
both, so doing the same is not a reason to replace its body. The membership of
that list is a judgment, stated as one so it can be argued with. A compile with
no recorded evidence (a plumbing mistake) is ACCEPTED and counted as `unjudged`,
because a mistake there must cost an unfiltered publish and never a silently
disabled tier.

### Verified

`cargo test -p cratonvm-jit --lib`: **2,267 passed, 0 failed**. `cargo test -p
cratonvm-types`: green, including the flag-surface guards and the regenerated
`docs/config/flag-inventory.md` / `docs/flag-tokens.md`.

The new emitted sequences are EXECUTED against the answers the JLS specifies,
edges included: `the_scalar_intrinsic_sequences_execute_to_the_specified_answers`
runs each compiled body and checks `Math.abs(Integer.MIN_VALUE) ==
Integer.MIN_VALUE` (JLS 15.15.4 — the negation wraps, and the branchless
`(x ^ (x>>31)) - (x>>31)` reproduces that exactly), signed `min`/`max` across
zero, and `Long.compare` on operands whose low halves are equal — the case that
catches sizing the `CMP` from the RESULT type, which is why
`ScalarOp::operands_are_long` exists separately from `ScalarOp::result_type`.

Three source-scanning audits keep the couplings visible rather than remembered:
`every_eligible_op_is_claimed_or_explicitly_rejected` (an op whose arm is shaped
for a dropped home must be claimed or listed in `DELIBERATELY_NOT_DROPPABLE`
with a reason), `the_block_node_order_is_the_emission_order` (the check-elision
pass reads `Block::nodes` in order and treats that as program order, which is
sound only while `ir_lower` emits a block the same way), and the existing
`every_droppable_op_writes_its_home_once_through_store_rax`.

### What is owed, stated rather than left to be inferred

**No throughput claim is made for any of the seven.** Every number above is a
COUNT — a census, a refusal tally, a body size — or the C2-off A/B that
motivated the work. The arms that would price these changes have not been run,
and the host they would have to be run on was at ~76% load throughout.

Three measurements are owed, in this order:

1. **The H2 A/B again, on the new binary.** The claim being tested is that the
   `c2-off` arm is no longer the fastest one. If it still is, the acceptance
   gate is doing its job and the tier is still not worth its epoch bumps.
2. **The IR-inlining flip on a second real workload.** Its 8% netty / 15-26%
   hibernate came from the 2026-08-28 gauntlet soak, taken BEFORE the deopt-bci
   fix; those numbers should be re-taken rather than inherited.
3. **A per-collector regression sweep.** Frequency-driven block layout moves
   every oop-map and safepoint position in every compiled method, which is
   exactly the change the RPO-layout work validated per collector for the same
   reason.

And one thing is NOT claimed: that C2's code generation was wrong. It was not.
The existing arc took `OsrTierBench.kernel` from 1.70x behind to parity, and
every checksum on every arm of this pass matched HotSpot. The problem was the
size of the optimizer, not the quality of its emitter.

### The measurement, and the three things it corrected

The section above listed what was owed. It was taken. Three of its claims did
not survive, and the corrections are more useful than the original text.

#### 1. The unresolved-class trap's coldness argument was wrong

The argument was: "the named class has never been loaded, and a class that has
never been loaded cannot have been touched by any path that has executed." The
first clause is true and the conclusion does not follow, because the observation
is made at COMPILE time. A class not loaded when the method compiles can load a
moment later, the path then runs, the trap fires on a LIVE path, and the body
returns the deopt sentinel — on every call, for the life of the process.

`ir_vs_singlepass_checkcast_not_yet_loaded_refuses_ir` executes exactly that
path and caught it on the first run. The fixture predates the trap by years.

Worse, trapping BYPASSES the machinery built for this transience:
`note_deferred_new_bail` / `take_deferred_new_retry` refuse the method and
re-offer it once the class loads. With the builder no longer bailing, nothing
armed that memo and nothing re-offered the method — trading a delayed optimizing
body for a permanently deopting one. The H2 census shows the repair:
`deferred-new retries: held=152 spent=2 retired=10 re_offered=1`, where the
trapping build read `spent=0 re_offered=0`.

So `CRATONVM_JIT_IR_UNRESOLVED_CLASS_TRAP` is its own switch and ships **OFF**.
`invokedynamic` keeps its trap and stays on: there is no "later" for a bootstrap
this tier will never lower, and the single-pass backend has made that exact
trade by default since it stopped bailing on indy. The reach claim drops from
~200 methods to ~150 accordingly.

What would make the other three safe is `DeoptAction::RecompileAndReinterpret`
instead of `Reinterpret`, so the first trap triggers the recompile that resolves
the class. `Op::Guard`'s lowering hard-codes the action; parameterising it is
the work that switch is waiting on.

#### 2. The acceptance gate refused, and then published anyway

The gate discards the optimizing body, `try_compile_inner` falls through to the
single-pass backend, and `try_jit_compile_callee_slow` puts THAT in the cache —
replacing a C1 body with an equal one and bumping the process-wide supersede
epoch, which stales every cached invoke target in every thread. The entire cost
the gate exists to avoid, paid in full by a gate that refused.

A refused verdict on a method that ALREADY HAS a published body now abandons the
supersede. Both halves of that condition are load-bearing: a refusal alone is
not enough, because at the eager first-call door there is no predecessor and
skipping the publish there would leave the method INTERPRETED.

| H2 census | before | after |
|---|---:|---:|
| supersede publishes (`changed`) | 179 | **69** |
| IC evictions from the epoch bump | 1,558 | **696** |
| fell through to single-pass | 128 (275 ms) | **19 (42 ms)** |
| supersedes abandoned | — | **113** |

#### 3. "C2 supersede costs 6% of CPU" is true and was reported as more than it is

**Process CPU counts background compile threads.** The tier's cost is largely
the compile itself — `lowered=50 (33 ms) fell_through=19 (42 ms)` against a
~1.9 s run — and that work overlaps with the mutator. Measuring only CPU
attributes a parallel cost as though it were serial.

Three interleaved runs of `probes/DodJdbcWorkload.java`, order reversed on
alternate rounds, the default configuration run TWICE as its own floor, paired
counts (a fair coin under no effect):

| run | host load | CPU: default beats c2-off | WALL: default beats c2-off | control (CPU / wall) |
|---|---|---|---|---|
| A | 10 | **5 / 21** | not measured | 9/21 / — |
| B | mid | **7 / 21** | 11 / 21 | 9/21 / 10/21 |
| C | 38 | 10 / 25 | 11 / 25 | 12/25 / 11/25 |

The control pair is a coin in every run, in both instruments, which is what
makes the rest readable.

**On wall clock there is no difference.** 11/21 and 11/25 against controls of
10/21 and 11/25. **On process CPU, `CRATONVM_C2_SUPERSEDE=0` is a few per cent
cheaper on a quiet host** (5/21 and 7/21) and washes out on a busy one — the
signature of background work, not of a slower body.

So the recommendation is NOT to flip the supersede default: the only instrument
where turning the tier off wins is the one that charges parallel compile work to
the serial total. And the honest verdict on the seven items is that they are
**correctness and reach work whose throughput effect on this workload is below
the measurement floor in both directions** — not a win, and not the regression
the CPU-only reading suggested.

The pre-change finding that motivated the work stands unaltered: before these
changes C2 bodies were 6% larger in aggregate than the C1 bodies they replaced
(81 bigger, 66 smaller, 14 equal over 161 supersedes), and `c2-off` was the
fastest arm. It is no longer the fastest arm on wall clock.

#### What is still owed

A second real application. Every number here is H2, and H2 is a workload whose
own profile is dominated by allocation and young-GC throughput rather than JIT
codegen — which is the least favourable place to look for a codegen win. netty
and hibernate are where the IR-inlining soak measured 8% and 15–26%, and those
are the arms that would price item 1 on its own terms.

The per-collector sweep is no longer owed -- it was taken, and the section
below records both what it found and the G1 flake it turned out NOT to be.

### The per-collector sweep, and the G1 failure that was not this branch's

Owed because frequency-driven block layout moves every oop-map and safepoint
position in every compiled method. Three collectors exist
(`parse_gc_algorithm`): ZGC (default), G1, Generational.

| collector | new defaults | every kill switch set |
|---|---|---|
| ZGC | 91/91, twice | 91/91 |
| Generational | 91/91 | — |
| G1 | 91, 91, **90** (`RMapGcStress`) | 91, 91, **90** (`RTreeRangeGc`) |

**Both arms fail at 1 in 3 full-suite runs, with DIFFERENT vectors, both
GC-stress.** That is a collector-level intermittency on G1, not something these
changes introduced — and `RTreeRangeGc` is the vector `known-flaky.txt`'s own
header records as quarantined at 3/12 (25%) on 2026-08-22, which is the same
rate.

Two things follow, and the second matters more than the first.

`RMapGcStress` alone on G1 passes **4/4** with the new defaults; it fails only
inside a full-suite run. So whatever perturbs it needs the suite's own memory
and GC pressure, which is why running the vector alone — what `known-flaky.txt`
asks for — cannot measure it.

And **a single G1 suite run is not a gate.** At ~1/3, one green run says little
and one red run says less. Any claim of the form "91/91 on G1" from one run is
over-read, this file's earlier ones included.

The hypothesis that was wrong is worth recording because it was well-motivated:
`RMapGcStress` stresses HashMap and ConcurrentHashMap chains, which is exactly
where JDK-internal `aastore` on an `Object[] table` lives, and this branch both
added a new `jit_aastore` call site and moved every oop-map position. The
all-off arm failing at the same rate refutes it.

### The acceptance gate's evidence list was too narrow, and the measurement said so

The gate shipped refusing 580 bodies on H2. The list omitted constant folding,
algebraic simplification, GVN and dead-node elimination — real optimizer work —
on this argument, written when the diagnostic landed:

> the single-pass backend folds constants too, so a graph getting smaller says
> the optimizer ran, not that its output beats C1's.

It is a reasonable argument and it is wrong. `Transform::Simplified` was added
to split the refusals by whether `ir_optimize`'s fixpoint loop actually removed
nodes, and the split came back **296 simplified against 284 inert** — half the
refusals had the optimizer genuinely work on them.

So the gate was A/B'd against accepting everything. Twenty-one interleaved
rounds, host load 7, order reversed on alternate rounds:

| arm | wall mean | CPU mean |
|---|---:|---:|
| the gate (`evidence`) | 2.315 | 2.458 |
| the gate again (control) | 2.306 | 2.463 |
| **`CRATONVM_C2_ACCEPT=always`** | **2.237** | **2.378** |

`always` won **15 of 21** rounds on wall AND on CPU, ~3.4%, against a control
pair agreeing to 0.4% on the means. **The bodies the gate refused were better,
and the gate was the thing costing throughput.**

`Simplified` is evidence now. That keeps the half that is provably inert
refused — 282 methods where the optimizer removed NOTHING — so the abandon path
still saves their epoch bumps, and stops the gate discarding the half that did
work:

| census | narrow list | corrected |
|---|---:|---:|
| accepted | 238 | **568** |
| refused (all `inert` now: `simplified=0`) | 580 | **282** |
| supersedes abandoned | 113 | 42 |

**And the regression that opened this whole section is gone.** Paired counts,
same harness, same workload:

| comparison | narrow list | corrected list |
|---|---|---|
| C2 supersede vs `CRATONVM_C2_SUPERSEDE=0` | c2-off won **16 of 21** on CPU | **coin** — 9/21 CPU, 10/21 wall |
| the gate vs `CRATONVM_C2_ACCEPT=always` | the gate lost, 6 of 21 | **coin** — 9/21 and 9/21 |

So the gate is now free: indistinguishable from accepting everything, while
still refusing 282 methods whose optimizing body provably did nothing and
saving their publishes and epoch bumps. Regression suite 91/91 on ZGC and 91/91
on G1 with the corrected list, which publishes 568 optimizing bodies instead of
238.

### A harness defect worth copying, because it produced a fake control

The arm order is reversed on alternate rounds so no arm is systematically first.
With THREE arms that balances arms 1 and 3 and leaves arm 2 in the middle every
single round — so the CONTROL pair, which is arms 1 and 2, carries a position
bias its paired count cannot shed. It read 7 of 21 while its own means agreed to
0.4%.

Read the paired count off the 1-vs-3 comparison, which is balanced, and read the
control for its MEANS. A control whose paired count is skewed by position looks
exactly like a control that has found an effect.

One run was discarded outright rather than reported: host load reached 76 and
the control pair disagreed with ITSELF by 3.7%, larger than the effect being
measured. This file has recorded that rule several times and it applied again.

### The unresolved-class trap: the blocker was named wrong, and the reach is smaller than claimed

The previous section left this: *"What would make the other three safe is
`DeoptAction::RecompileAndReinterpret` instead of `Reinterpret`, so the first
trap triggers the recompile that resolves the class. `Op::Guard`'s lowering
hard-codes the action; parameterising it is the work that switch is waiting
on."*

**That names the wrong field.** `Op::Guard` does bake an action into its
`DeoptimizationPoint`, but nothing reads it. The runtime recomputes the action
from the REASON on every deopt — `record_deoptimization` calls
`DeoptimizationLog::recommend_action_at_bci(method, reason, bci)` and returns
that. The baked `action` is dead metadata on this path.

And the reason it does bake, `DeoptReason::UncommonTrap`, already escalates.
`UncommonTrap` takes the count-based policy: `Reinterpret` on the first deopt,
then `RecompileAndReinterpret`. So the self-healing the switch was said to be
waiting for is already there, one deopt later than ideal. Planting
`DeoptReason::ClassLoading` instead would reach it on the FIRST deopt and would
additionally clear the method's assumptions — but that is a one-deopt saving on
a path measured below, not the unblocking the note described.

#### The measurement, which is what should have been taken first

| H2, `CRATONVM_JIT_IR_UNRESOLVED_CLASS_TRAP` | off | on |
|---|---:|---:|
| traps planted (typecheck / new) | 0 / 0 | **39 / 9** |
| bodies accepted | 579 | 586 |
| bodies lowered | 118 | 124 |
| fell through to single-pass | 19 | 15 |
| deferred-new retries `spent` / `re_offered` | 2 / 1 | **0 / 0** |
| eager re-queues (`RecompileAndReinterpret`) | 0 | **0** |
| `DOD RESULT` | OK | OK |

**All 48 traps plant and not one fires.** On this workload the coldness argument
holds exactly as written: no path that executes reaches a site whose class was
unloaded at compile time.

Which cuts both ways, and the second reading is the one worth keeping. Zero
fires means there is no evidence the trap is harmful — and equally none that
the escalation path works, because it never ran. "I read the policy function
and it returns `RecompileAndReinterpret` at count 1" is a claim about source,
not a measurement. The switch stays OFF until something fires it.

`ir_vs_singlepass_checkcast_not_yet_loaded_refuses_ir` cannot supply that.
It was read earlier as proving the body "returns the deopt sentinel instead of
the object — forever, on every call". It proves the first half only:
`call_with_dummy_context` invokes the compiled body directly, with no VM, no
deopt entry, no `record_deoptimization` and no recompiler. A unit test with no
runtime cannot tell "traps once and heals" from "traps forever", and this one
was cited for the second.

So the honest state of this item: reach is +7 accepted bodies and +6 lowered on
H2, the danger is unobserved rather than absent, and what it actually needs is
a VM-level test that FIRES a trap and shows the recompile come back without it.
That test is the work; the action parameter never was.

### Range-based bounds-check elimination

`bounds_elided=8` against `bounds_emitted=174` was the largest number left on
the board, and it has a structural cause: the dominance pass removes the SECOND
check on an SSA `(base, index)` pair and can never remove the FIRST. Almost all
Java array traffic has no second access to remove.

The new pass proves the check dead outright. A check at `(base, idx)` goes when
both halves hold:

* `idx < base.length` — a dominating `If` on `Cmp(Lt)[idx, ArrayLength(base)]`,
  taken on its true edge, matched on the array's SSA node so two arrays of
  equal length stay two arrays.
* `idx >= 0` — a non-negative constant, another array length, or a unit-stride
  induction variable.

Both, always. The emitted check is an UNSIGNED compare, which is exactly the
conjunction `0 <= idx < len`; the source-level test it is being proven from is
SIGNED, and a negative index passes that. Eliding on the upper bound alone
indexes behind the object.

Two restrictions are proofs rather than conservatism, and the tests say so in
their names:

**Unit stride only.** For `i = [region, init, i + s]` guarded by `i < len`, the
guard gives `i <= len - 1`, so after the increment `i <= len - 1 + s`. An array
length reaches `i32::MAX`, so any `s > 1` overflows at the top of the range; the
wrapped `i` is negative, passes the signed test, and indexes out of bounds with
the check gone. At `s == 1` the bound is `i <= len <= i32::MAX` and the proof
closes without knowing `len`. A "small constant" stride bound would not — it is
still unsound against a near-maximal array.

**The guard must dominate the BACK EDGE, not just the access.**
`for (int i = 0; ; i++) if (c) if (i < a.length) a[i] = 1;` has a bounds test
dominating every access and still lets `i` reach `i32::MAX` and wrap, after
which that same test passes on a negative index. Requiring every back edge into
the loop header to pass through the guard is what rules it out.

The census splits the two mechanisms — `ir bounds elisions by range proof` —
because a single total moves for either reason and they have opposite reach.

#### Two ways it nearly shipped inert, and how each was caught

Both were found by asking what shape the BUILDER actually emits, not by any
test -- every unit test in the module passed through both.

**Loop headers are `Op::Merge`, never `Op::Region`.** `IrBuilder::ensure_merge`
creates every merge point as an `Op::Merge`; `activate_loop_header` then fills
in its inputs and phis and never rewrites the op. `Op::Region` appears only in
hand-built graphs -- including this module's own tests. A first version
required `Op::Region` and would have eliminated exactly zero bounds checks in a
compiled method while showing twelve green tests.

**javac puts the loop body on the FALSE edge.** The builder preserves bytecode
branch polarity: `if_icmpge exit` becomes `Cmp(Ge)` with `Proj(0)` going to the
BRANCH TARGET, so for a top-tested loop the useful fact is `not (i >= len)` on
`Proj(1)`. Matching only `Lt`-on-true finds the bottom-tested shape and misses
the other one silently. Both edges are read now, with the four
`(edge, comparison)` pairs tabulated at `upper_bounds`.

Each has a test named for the failure rather than the feature --
`the_header_op_the_builder_actually_emits_is_recognised`,
`the_bound_is_read_off_the_false_edge_of_a_negated_test` -- because the thing
that needs catching is a silent zero, and a zero is what a workload with no
such loops also produces.

#### Measured

`probes/BceProbe.java` is the behavioural half: the unit tests assert what the
ANALYSIS decides, the probe asserts what the EMITTED CODE does. It sums, fills
and walks a jagged array on the proven shape, and demands
`ArrayIndexOutOfBoundsException` from four shapes the pass must refuse -- an
inclusive bound, a negative start, a second shorter array, and an empty array
-- all after 20,000 warm-up calls, so it is the compiled body under test and
not the interpreter.

| | range pass on | off |
|---|---:|---:|
| `BceProbe` bounds elided / emitted | **6 / 4** | 0 / 10 |
| `BceProbe` verdict | OK | OK |
| H2 bounds elided (of which by range) | **18 (10)** | 8 (0) |
| H2 `DOD RESULT` | OK | OK |

Six of the probe's ten checks go, and every refusal still traps. On H2 the
range pass adds ten elisions on top of the eight redundancy already found --
purely additive, since the two prove disjoint things. Ten of ~166 is a modest
share, and expected: H2's hot code is collections and MVStore rather than raw
array loops, which is the same reason the seven-item pass measured flat there.

Regression suite 91/91 on ZGC.

### The remaining 57 refusals are recoverable for free, and recovering them costs 3%

`CRATONVM_JIT_IR_OVER_INTRINSIC=1` already exists and already works. With it on,
the planner stops refusing a method for containing an intrinsic call site it has
no node for, and falls back to an ordinary `Op::Call` there:

| H2 | default | `OVER_INTRINSIC=1` |
|---|---:|---:|
| `refused_method` | 57 | **0** |
| bodies accepted | 569 | 614 |
| bodies lowered | 113 | 123 |
| fell through to single-pass | 19 | 12 |
| `lowered_as_arithmetic` | 20 | 29 |
| `DOD RESULT` | OK | OK |
| regression suite | 91/91 | **91/91** |

So the whole remaining work list clears, with no correctness cost. That is the
easy half, and it is the wrong half.

The refusal carries an argument rather than a measurement:

> Every family that reaches here has an emitted intrinsic that replaces a LOOP
> … or a memory form this tier has no node for … For those the intrinsic really
> is worth more than the rest of the method's optimization, so the method-level
> refusal stays.

Earlier in this file an argument of exactly that shape — the acceptance gate's
evidence list — was measured and turned out to be wrong by 3.4%. This one was
measured too, and **it is right.**

Three interleaved runs, order reversed on alternate rounds, the default
configuration run TWICE as its own noise floor, paired counts:

| run | host load | control (cpu / wall) | `over` beats `default` (cpu / wall) |
|---|---|---|---|
| 1 | 18% | 11/21, 12/21 — coin, means within 0.6% | **8/21, 8/21** |
| 2 | high | 17/21, 16/21 — **means 6% apart** | *discarded* |
| 3 | 31% | 9/21, 7/21 — means within 1.4% | **3/21, 2/21** |

Run 2 is discarded rather than reported: the control disagreed with ITSELF by
6% on the means, larger than the effect under test, and the absolute times
jumped from ~2.1 s to ~2.8 s mid-run. Its treatment comparison happened to read
as a coin, which is exactly why the rule is to discard on the control and not
on whether the answer is convenient.

Over the two valid runs `over` wins **11 of 42 on CPU and 10 of 42 on wall** —
about 3% slower on the means, and agreeing in DIRECTION in both instruments and
both runs.

*(Re-scored 2026-09-07 against the stricter standard the hibernate reversal
forced on this file. Per run: run 1 is 8 of 21 on both instruments, z = -1.09 —
**a coin on its own**; run 3 is 3 of 21 and 2 of 21, z = -3.27 and -3.71. Pooled,
z = -3.09 and -3.39. So "consistent in both runs" overstated run 1: what is
consistent is the DIRECTION, four times out of four, and the pooled count is
what carries the significance. That is still a much stronger position than the
withdrawn hibernate result, whose two samples pointed OPPOSITE ways (+2.47 and
-2.49) — direction agreement across independent runs is exactly the check that
one failed and this one passes.*

*What remains untested is the same thing that broke hibernate: both runs used
the SAME 21 classes, so this establishes REPEATABILITY, not that the effect
generalises to other H2 classes. The claim is load-bearing — it is why
`OVER_INTRINSIC` stays off and why the accessor work is scoped as "lower these
families" rather than "stop refusing them" — so the disjoint-class check is
worth running before anyone leans on it harder than that.)*

*And that check cannot currently be run, which is the more useful finding: **the
instrument that produced these numbers is not in the repo.** `tools/suite-pair-ab`
is fork-per-class JUnit only ("netty, hibernate-reactive" by its own header) and
knows nothing about H2; the three-interleaved-run harness described above was
ad-hoc and did not survive its session. So the measurement backing a default-OFF
flag and a filed work item is, today, unreproducible by anyone including its
author. Committing an H2 equivalent of `pair-ab` — same ABBA-per-unit shape, same
same-config noise floor, same split-half check — is the prerequisite for
re-testing any H2 throughput claim in this file, not just this one.*

**So the intrinsic at those sites really is worth more than optimizing the
method around it.** Trading an inline unboxing load or an `Atomic*` accessor for
a generic `jit_invoke_dispatch` costs more than the surrounding body gains, and
the ~57-method refusal is paying for itself.

The flag stays OFF, and its default is now measured instead of argued.

#### What this makes the work list mean

`refused_method` is a list of families to LOWER, not a list of refusals to lift.
The bit-scan families above are the pattern that works: recognised in
`try_ir_scalar_intrinsic`, lowered as real IR nodes, no call at all — both the
intrinsic's speed and the method's optimization. Calling them instead gets the
reach and loses the point.

Ranked by how much of the current H2 refusal each family holds:

| family | methods held | shape |
|---|---:|---|
| `System.arraycopy` | 10 | memory; a real loop-replacing intrinsic |
| `Long.longValue` | 8 | a field load behind two header layouts |
| `AtomicLong.get` | 7 | a volatile load (plain `MOV` on x86-64 TSO) |
| `String.valueOf(Object)` | 5 | allocation + dispatch |
| `String.trim` | 4 | string internals |
| `String.isNotContinuation`, `String.<init>([BB)V` | 4 | string internals |
| `String.isLatin1` | 3 | string internals |
| `Integer.intValue` | 3 | as `longValue` |
| `AtomicInteger` inc/dec, `AtomicLong.getAndAdd` | 3 | `LOCK XADD` |
| `Math.abs(F)` / `Math.abs(D)` | 2 | `ANDPS`/`ANDPD` with a sign mask |
| `Long.bitCount` | 1 | needs POPCNT, deliberately excluded |
| 7 more, one method each | 7 | `String` ctor/`indexOf`/`join`/`valueOf(J)`/`startsWith`, `Arrays.equals`, `AtomicLong.<init>` |

The two `Math.abs` FP forms are the cheapest real entry (a sign-mask AND, no
memory, no guard); the unboxing and `Atomic*` accessors are the largest single
block but need a header-shape decision — `box_unbox_intrinsic_shape` resolves a
`value_compact_offset` AND a `value_legacy_offset`, so an IR node for them has
to pick between two layouts or guard on one.

### netty and IR inlining: the pass rate is clean, and the throughput number could not be taken

`CRATONVM_JIT_IR_INLINE` ships default ON as of this branch, and every number
justifying that came from H2. The debt this section pays is the one named above:
"netty and hibernate are where the IR-inlining soak measured 8% and 15–26%, and
those are the arms that would price item 1 on its own terms."

200 netty test classes on the Azure host, fork-per-class, 4 shards, 180 s
per-class cap. ONE binary, one lever, three arms — `on`, `off`, and `on` AGAIN,
because that box is shared and two arms cannot separate an inlining effect from
the machine getting busier between them.

| arm | `IR_INLINE` | wall | `sum_class_ms` | PASS | FAIL | ABORTED | HANG | `[ir] spliced` |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| on (1st) | 1 | 977 s | 2,279,174 | 111 | 57 | 18 | 2 | 2802 |
| off | 0 | 1061 s | 2,374,963 | 110 | 57 | 18 | 3 | **0** |
| on (2nd) | 1 | 1083 s | 2,693,458 | 111 | 57 | 18 | 2 | 2836 |

#### The engagement census, first

`spliced` is 2802 and 2836 on the two `on` arms and **0** on the `off` arm.
That is what makes the rest of the table readable at all: the lever demonstrably
moves the thing it names, on this workload, in this binary. A soak without that
row is not a soak, and netty is exactly the workload where IR inlining has
something to chew on — 6600 inline-plan sites against H2's handful.

#### The pass rate: no regression

Across 200 classes the `on` and `off` arms differ in **exactly one class**:

```
only in the INLINE=1 arm:  io.netty.buffer.AdaptiveByteBufAllocatorGrowthTest ok=400 failed=0
only in the INLINE=0 arm:  (none)
```

Every other class reports identical `ok=` and `failed=`. `FAIL=57`,
`ABORTED=18` and `NOTESTS=12` are byte-identical in all three arms, and the two
`on` arms agree exactly (111/57/18/2/12). The one class that differs completed
400 tests with inlining and hit the flat 180 s cap without it — which is a
HANG-vs-PASS at a timeout on a host under load 10–20, not a demonstration that
inlining rescued it.

**So the default-ON flip does not regress netty.** That is the claim worth
making and it is the one the run supports.

#### The throughput number: NOT measured, and not reported as if it were

The naive reading of the table is that `on` beats `off` by 7.9% on wall and 4.0%
on `sum_class_ms`, comfortably in line with the 8% this section owed. That
reading is wrong, and the third arm is what says so:

* wall: the two IDENTICAL arms are 977 s and 1083 s — **10.8% apart**, against a
  7.9% on-vs-off difference.
* `sum_class_ms`: 2,279,174 and 2,693,458 — **18% apart**, against 4.0%.

**The control disagrees with itself by more than the effect**, in both
instruments, so no throughput conclusion is available. Host load ran between
2 and 21 over the three arms (another session was building with `-j 6`
throughout), and the arms are necessarily sequential because the harness is
fork-per-class rather than interleaved.

This is the third time in this document that rule has fired — the acceptance
gate's discarded run, the over-intrinsic run 2, and now this one. It keeps
firing because the tempting number and the invalid control arrive together: had
the third arm been skipped, this section would have reported "8% confirmed on
netty" and been believed.

What it would take to price it properly: a quiet host, or an interleaved harness
that alternates the lever per CLASS rather than per run so drift cancels
pairwise. The per-class `@@RESULT ms=` values make the second one cheap to
build, and that is the right next step for anyone who wants the number rather
than the pass rate.

hibernate-reactive was not run. The netty arm alone took ~50 minutes of host
time in a three-arm shape, and a second suite would have added nothing the
control did not already invalidate.

### The per-class alternating harness, and what it says about the host

The section above ended with what it would take to price IR inlining properly:
"a harness that alternates the lever per CLASS rather than per run so drift
cancels pairwise". That is `tools/suite-pair-ab/pair-ab.sh`.

It is generic over the lever (`--lever CRATONVM_X --on 1 --off 0`) and over the
suite, because nothing in it is netty-specific beyond the runner directory it is
pointed at.

#### The design, and why each piece is load-bearing

**ABBA, not AB.** Each class is measured as four runs, `A B B A` (and `B A A B`
on odd classes, so the block asymmetry cancels across the list). ABBA cancels
LINEAR drift exactly: the mean timestamp of the two A runs equals the mean
timestamp of the two B runs, so a host steadily getting busier contributes
equally to both arms. Plain alternation does not have that property.

**The within-arm noise floor.** `|A1-A2|` and `|B1-B2|` are two runs of the SAME
configuration, so they measure the host and not the lever. This is the piece the
per-run shape could not have at any repetition count, and it is what lets the
harness say **UNMEASURABLE** instead of reporting a number. A harness that
cannot decline will eventually assert something false.

The two floors are deliberately asymmetric and the pessimistic one is used: in
ABBA the B runs are adjacent (positions 2, 3) while the A runs are separated by
them (1, 4), so `|B1-B2|` understates the noise and `|A1-A2|` overstates it. The
reported floor is `max(A, B)`, because the failure being defended against is
claiming an effect that is really drift.

**The same-work gate.** A pair counts only when all four runs report identical
`found/ok/failed/skipped/aborted`. Two runs that executed different numbers of
tests have incomparable `ms=`, and without this gate a flaky class contributes a
work difference disguised as a timing difference. Classes with `ok=0`, any
failure, or any abort are dropped for the same reason — on the 24-class
validation slice that dropped 13 of 24, which is the gate working, not a defect.

**Strictly sequential.** No shards. Sharded forks compete with each other, so
the two members of a pair would see different contention — the exact thing the
design exists to remove.

#### What it measured, which is the host

24 netty classes, `CRATONVM_JIT_IR_INLINE` on vs off, host load 11-13 with
another session building throughout:

```
A faster than B in 7 of 11 classes  (fair coin under no effect)
median per-class delta : +1.0%
median within-arm noise: 11.6%   (SAME config, two runs)
  of the 2 classes whose own delta beats their own noise: A faster in 1
VERDICT: UNMEASURABLE.
```

Read naively that is "inlining wins 7 of 11 and is 1.0% faster". The harness
refuses it, and it is right to: **two runs of the identical configuration differ
by 11.6%**, eleven times the effect. Only two of eleven classes had a delta
larger than their own noise, and those two split 1-1.

That number is the useful output. It quantifies, for the first time, why the
three-arm per-run A/B could not work on this host — not "the arms were 17
minutes apart" as a hypothesis, but **11.6% same-config variance measured
back-to-back on the same class**. Any per-run design was doomed by a wide
margin, and so is this one at this load.

Two individual classes are worth recording because the per-run shape could never
surface them: `AdaptiveLittleEndianHeapByteBufTest` came in at -3.9% against a
1.6% floor (inlining SLOWER) while `AdaptiveBigEndianHeapByteBufTest` read
+12.9% against 11.6%. Whatever the aggregate turns out to be, IR inlining is not
uniformly good or bad across classes, and a single suite-wide number would hide
that.

#### How to get the number

Run it on a quiet host. The floor is a property of the machine, not the harness:
on the 12-class smoke run earlier the same day, individual classes reported 1%
and 3% floors, so a quiet box should resolve effects in the low single digits.
`--min-ms` drops classes too short for the JIT to matter, and `--count` /
`--start` shard the list across sessions.

The verdict line is the contract: if it says UNMEASURABLE, the run has produced
a noise measurement and no throughput claim, and the honest report is the floor.

#### The floor is the machine, and the 8% is not there

The claim above — that the 11.6% floor is a property of the host rather than of
the harness — is testable, so it was tested: the SAME classes, the same lever,
run again when the box had quietened from load 11-13 to load ~4.

| class | floor at load 11-13 | floor at load ~4 |
|---|---:|---:|
| `BootstrapTest` | 2.5% | **0.9%** |
| `ServerBootstrapTest` | 26.9% | **1.9%** |
| `AbstractReferenceCountedByteBufTest` | 93.0% | **2.7%** |
| `AdaptiveBigEndianDirectByteBufTest` | 3.7% | **2.1%** |
| `AdaptiveBigEndianHeapByteBufTest` | 11.6% | **0.9%** |

The floor is the machine. On the quiet run the harness resolves to about 2%,
and it does that on the very classes that read 27% and 93% an hour earlier.

```
A faster than B in 2 of 6 classes
median per-class delta : -0.6%
median within-arm noise: 2.0%   (SAME config, two runs)
  no class had a delta larger than its own within-arm noise
VERDICT: UNMEASURABLE
```

**And that is the substantive finding, not a shrug.** At a 2.0% floor the
effect of IR inlining on these six netty classes is smaller than 2%, and the
median points very slightly the OTHER way (-0.6%, inlining marginally slower).
The original 8% is not merely unconfirmed here — it is excluded at this
resolution on this slice. Six classes is a small slice and the honest scope is
"these six", but the instrument was good enough to have seen 8% and did not.

#### The one class that differed in the suite A/B was a timeout, confirmed

The per-run three-arm comparison found exactly one class differing between
inline-on and inline-off: `AdaptiveByteBufAllocatorGrowthTest` passed with
inlining and HUNG without it, at the flat 180 s cap. That was read cautiously at
the time as "a slow class near the cap, not a rescue".

Run sequentially with no shard contention it takes **91.4 s with inlining and
90.7 s without** — a 0.7% difference against a 3.0% floor. It is a ~91 s class
that crosses a 180 s cap when four shards compete, and the lever had nothing to
do with it. The caution was right, and this is what it looks like to close that
kind of loose end instead of leaving it as a hedge.

## The seven follow-ups, 2026-09-07

### 1. Pricing what shipped default-ON without a price

Range BCE and the scalar-intrinsic families both went in on correctness and
engagement — 91/91 on three collectors, six of ten checks removed on a probe,
twenty-odd sites lowered — and neither was ever measured for throughput. That
is the same omission this file criticised the acceptance gate for, so it was
closed. H2, one binary, one lever, three arms with the default run TWICE as its
own floor, host at 4% CPU:

| lever | paired count (cpu / wall) | control (cpu / wall) | verdict |
|---|---|---|---|
| `CRATONVM_JIT_IR_BCE_RANGE=0` | 11/21, 11/21 | 13/21, 11/21 | **coin** |
| `CRATONVM_JIT_IR_SCALAR_INTRINSICS=0` | 9/21, 10/21 | 12/21, 12/21 | **coin** |

Both are throughput-neutral on H2. For the scalar-intrinsic arm the control's
own means sat 3.6% apart, which is as large as the treatment gap, so only the
paired counts are readable there — the means are not.

Neither result is a disappointment and neither is a reason to remove anything:
they are reach and correctness work, which is the same verdict the original
seven-item pass earned. What changed is that it is now measured rather than
assumed in the favourable direction.

### 2. IR inlining on netty, with the per-class harness

Two runs of `tools/suite-pair-ab/pair-ab.sh`, 46 usable classes between them:

| run | classes | A faster | median delta | noise floor | verdict |
|---|---:|---:|---|---|---|
| quiet host | 6 | 2 | -0.6% | **2.0%** | UNMEASURABLE |
| busier host | 40 | 17 | -0.7% | 8.8% | UNMEASURABLE |

**19 of 46 overall** — a coin, leaning very slightly against inlining, with both
runs agreeing on the sign and the magnitude (-0.6% / -0.7%). The 8% that
motivated this whole line of work is not there. The quiet run resolves to 2%,
so an 8% effect would have been unmissable.

Five classes had a delta beating their own noise; A was faster in three of
them. Even the individually-significant subset is a coin.

### 3. Why the other bounds checks are not provable — and why NOT to extend the pass

The range pass proves 9-11 checks of ~130 on H2. The obvious next move is to
relax its two restrictions (unit stride, guard-dominates-back-edge). The
refusal census says that would be wasted work:

```
[c2-supersede] ir bounds range refusals:
    no-length-test=110  other-array=16  index-not-non-negative=4
```

`guard-not-dominating=0`. `iv-shape-rejected=0`. **Not one bounds check on H2
fails because of the stride rule or the back-edge rule.** 110 of 130 fail
because the index is never compared against ANY array length anywhere in the
graph — they are isolated accesses, not loop-guarded ones, and no relaxation of
a loop-shape rule reaches them.

Removing those needs a different technique altogether (whole-method length-fact
propagation, or speculative predication with a deopt), not an extension of
this pass. Sixteen more are indexed by one array and length-tested against
another, which needs an equal-length or aliasing fact this tier does not have.

That is the entire value of the census: without it the next session extends the
stride rule, measures no change, and has to work out why. `pair-ab` and this
are the same lesson in two places — build the instrument that can say "no".

### 4. `Math.abs(float)` / `Math.abs(double)` as scalar intrinsics

The first FP members of `ScalarOp`, and the cheapest entry left on the
work list. One AND against a sign mask — no branch, no memory, no CPU feature.

The sign-mask form is not merely faster than `x < 0 ? -x : x`, it is *more
correct*: the comparison form returns **-0.0** for `abs(-0.0)`, because
`-0.0 < 0` is false. `probes/ScalarFpAbsProbe.java` pins that (via `1/x`, since
`-0.0 == +0.0` compares true), plus NaN, both infinities, both `MIN_VALUE`
subnormals and a full-mantissa value, and agrees with HotSpot on all of them.

These do NOT go through `gp_load_value`/`store_rax` like every other member, so
the lowering arm gained an `is_fp()` guard that returns before the
general-purpose load. Two things made that safe to bolt onto the existing op
rather than needing a new one: `fp_load_value`/`fp_store_value` already exist,
and `value_home_droppable` refuses any type that is not `Int`/`Long`, so the
`op_home_is_one_store_rax` claim over `Op::ScalarIntrinsic` is filtered by type
before it can be consulted for an FP node.

`every_declared_family_is_recognised` now drives off an EXHAUSTIVE match
instead of a hand-kept tuple list. The old form could not catch the one failure
it existed for — a family present in the enum and the lowering but missing from
the recognizer, which reads from outside exactly like a workload with no such
call site. Adding a variant is now a compile error until its signature is
declared.

H2 `refused_method` 57 -> 45.

### 5. The unresolved-class trap does NOT self-heal — and the correction above was wrong

This file said, earlier today, that the original assessment of this trap had
named the wrong blocker:

> `Op::Guard` does bake an action into its `DeoptimizationPoint`, but nothing
> reads it. […] `UncommonTrap` takes the count-based policy: `Reinterpret` on
> the first deopt, then `RecompileAndReinterpret`. So the self-healing the
> switch was said to be waiting for is already there, one deopt later than
> ideal.

**That correction was itself wrong, and the text it corrected was right.**
`probes/UnresolvedTrapProbe.java` fires the trap and the answer is not
ambiguous.

The probe's shape is the awkward part and worth keeping: for a trap to be
planted the class must be unloaded when the method compiles, but a method only
gets hot by running, and running the cast would load the class. So the cast
sits behind a parameter that is false during warm-up — 200,000 calls with
`doCast=false` compile the method with `Shape` still unloaded, then 200,000
calls with `doCast=true` put the trap on a live path.

```
[c2-supersede] ir site traps planted: unresolved-typecheck=1
200000  reason=UnreachedCode bci=5 action=MakeNotCompilable
        eager re-queue (RecompileAndReinterpret): 0
```

**Every single call deopts.** 200,000 of 200,000, at the checkcast bci, with
`MakeNotCompilable` and not one recompile. The answers stay correct — the
interpreter finishes the bytecode — which is exactly why this could never have
been settled by a correctness probe, and why the earlier reasoning went astray:
the failure mode is throughput, permanently, and it is invisible unless you
count deopts.

Two things the run also settles:

The reason is `UnreachedCode`, not the `UncommonTrap` that `Op::Guard`'s
lowering bakes into its `DeoptimizationPoint`. So the guard's `reason` field is
as dead as its `action` field — the runtime sees a fixed code — and
`UnreachedCode` maps to `MakeNotCompilable` on the FIRST occurrence, with no
count-based escalation at all. That is the mechanism, and it is worse than
either the original text or its correction supposed.

It is not the acceptance gate hiding the IR body either. With
`CRATONVM_C2_ACCEPT=always` (`accepted=4 refused_no_evidence=0`) the result is
byte-identical: 200,000 deopts, same reason, same action.

`CRATONVM_JIT_IR_UNRESOLVED_CLASS_TRAP` stays **OFF**, now for a measured
reason. And a flag worth raising rather than burying: the `invokedynamic` trap
is DEFAULT ON and is planted by the same `plant_uncommon_trap`, so it has the
same property. No harm is observed — H2 plants 21 of them and fires none, and
an indy bootstrap this tier will never lower has no "later" to wait for — but
"none of them has ever gone live on a workload we run" is the only thing
standing between that default and this behaviour.

The lesson worth keeping is about the correction, not the trap: a unit test
with no deopt runtime could not distinguish "traps once and heals" from "traps
forever", and I used that inability as licence to prefer the reading I had
derived from reading a policy function. Reading a policy function is not
running one.

### 6. hibernate-reactive

60 classes, one binary, one lever.

| arm | `IR_INLINE` | PASS | NOTESTS | `[ir] spliced` |
|---|---|---:|---:|---:|
| on | 1 | 59 | 1 | **12,677** |
| off | 0 | 59 | 1 | **0** |

Pass rates identical, and the engagement census is emphatic: hibernate gives IR
inlining more than four times the work netty does (12,677 splices against
2,802). If any workload here were going to show the effect, it is this one.

`sum_class_ms` reads 1,061,583 on and 1,179,251 off — a 10% win, the largest
apparent number in this whole investigation. **It is not reported as a result**,
because it is a per-run sequential comparison, which the section above proved
cannot produce a timing number on this host. It is exactly the shape that read
"8% on netty" and turned out to be drift.

So the harness was pointed at hibernate instead, and it found something.

### The one real throughput result: inlining is a small, consistent win on hibernate

37 usable classes, ABBA per class:

```
A faster than B in 26 of 37 classes
median per-class delta : +0.3%
median within-arm noise: 6.9%   (SAME config, two runs)
  of the 3 classes whose own delta beats their own noise: A faster in 3
sign test on the paired count: z = +2.47  (consistent, p < 0.05)
VERDICT: SMALL BUT CONSISTENT.
```

26 of 37 is not something a fair coin does. Each individual class is
noise-dominated — 0.3% against a 6.9% floor — but the DIRECTION survives
averaging over 37 of them, and the three classes that individually clear their
own noise all point the same way.

**This is the first positive throughput result for IR inlining on real code in
this document**, and it is nothing like 8%: it is a fraction of a percent,
detectable only because the sign test aggregates many classes. Set against
netty's 19 of 46 (z = -1.18, a coin), the picture is that IR inlining is
somewhere between neutral and slightly positive on real applications, and the
original 8% does not reproduce anywhere.

#### The harness had to be corrected to see it

The first version compared the median effect against the median noise floor and
called hibernate UNMEASURABLE. That rule is right for a single class and wrong
for a suite: a small effect that is CONSISTENT across many classes is exactly
what a per-class design can detect and a per-run design cannot, and folding the
paired count out of the verdict threw away the only thing this harness was
built to find. `pair-ab.sh` now reports the sign test as a z-score and has a
third verdict, SMALL BUT CONSISTENT, for effect-below-floor with a paired count
a coin does not produce. netty still reads UNMEASURABLE under the new rule
(z = +0.90); hibernate reads z = +2.47.

### 7. The two flakes

**G1's GC-stress family is a harness TIMEOUT, not a stochastic defect.** Four
clean full-suite G1 runs on an uncontended host: **92/92, four times.** The
earlier "91, 91, 90" that put this on the residual list was taken while other
work shared the box.

A fifth run, taken deliberately while twelve `cargo test` invocations ran
alongside, reproduced the failure and labelled it:

```
RExceptions   FAIL rc=124: HARNESS FAULT — TIMED OUT; the harness killed the
                   VM, it did not fail [try TIMEOUT=600]
RMapGcStress  FAIL rc=124: HARNESS FAULT — TIMED OUT ...
HARNESS ERROR [G4] RJdkFormatLocale: the HotSpot oracle run FAILED (rc=1)
```

The HotSpot ORACLE failed in that run too, which no CratonVM defect can cause.
So the vector to record is not "RMapGcStress is flaky on G1" but "the suite's
flat per-class timeout is too tight for a contended host", and the fix is
`TIMEOUT=600` or an uncontended run, not a `known-flaky.txt` row. That also
explains why the vector passed 4/4 when run alone and failed only inside a full
suite: the suite is what supplies the load.

**`test_jit_cache_clear_all_evicts_entries` is NOT MEASURED, and the reason is
worth writing down.** The `cratonvm-vm` lib-test target does not build in this
checkout: release ends in `rustc` exit 101, and debug fails with
`os error 112 — not enough disk space`. The machine had **0 bytes free of
930 GB**, and the debug tree from that one attempt was itself 9.3 GB. Nothing
was measured because nothing could be run.

What the G1 result does supply is a much better prior. The original observation
was "1 failure in 11 PARALLEL runs, 0 single-threaded" — the same contention
signature that turned out to explain the G1 family entirely. That is a
hypothesis with new support, not a result, and it is recorded as one.

### The invokedynamic trap fires on real code, and firing cost the method every tier

The section above flagged this and did not measure it: the `invokedynamic`
trap is DEFAULT ON, is planted by the same `plant_uncommon_trap` as the
unresolved-class trap, and therefore has the same never-heals property —
"no harm is observed" being the only thing standing between that default and
the behaviour.

Harm is observed. H2, default configuration:

```
ir site traps planted: invokedynamic=21 ... | TAKEN at runtime: 1
org/h2/mvstore/MVStore.getMapId:(Ljava/lang/String;)I
    reason=UnreachedCode bci=5 action=MakeNotCompilable
```

One of the twenty-one went live, and `MakeNotCompilable` is consulted by
`compile_gate` itself (`is_jit_bail_listed`), so `getMapId` lost its body on
**every** tier — including the single-pass backend, which lowers
`invokedynamic` perfectly well and had been compiling that method before site
traps existed. Planting a trap to gain the rest of the method's optimization
cost the method all of its compilation, permanently, the first time the trapped
path ran.

#### Where it came from

`try_resume_trapped_callee` hard-codes the reason it reports, ignoring the
`DeoptimizationPoint` the artifact carries — which is why `Op::Guard`'s baked
`UncommonTrap` never reaches the policy. Its comment says so, and anticipates
this exact case:

> Reason: `UnreachedCode` — the one-shot "give up immediately" policy […] so
> the trapping method is made not-compilable on the FIRST resolution […]
> (A guard-bail stash reaching this arm is over-blacklisted by this —
> acceptable: it reverts to the interpreter, which is always correct.)

Correct for the single-pass indy trap, where no tier can do better. Wrong for an
IR site trap, where only the OPTIMIZING tier had the problem. "Reverts to the
interpreter, which is always correct" is true about answers and silent about
throughput, and this is the second time in this document that a
correctness-only argument hid a permanent slowdown.

#### The fix

A site trap now says what it means: ban the optimizing tier for this method —
the memo `try_compile_inner` already consults — and pick a reason that
RECOMPILES rather than blacklists. The recompile then goes single-pass and does
not trap. `SpeculationFailed` is that reason: `RecompileAndReinterpret` until
the per-method deopt count crosses `max_deopts_per_method`, which keeps a
backstop if the assumption is ever wrong.

Telling the two apart needs the runtime to know the artifact carries a site
trap, so `plant_uncommon_trap` now records the method (`build(mut self, ..)`
consumes the builder, so the count comes back through a per-build thread-local,
the same idiom `reset_string_access_sites` uses).

H2, same workload, after:

```
ir site traps planted: invokedynamic=21 ... | TAKEN at runtime: 1
org/h2/mvstore/MVStore.getMapId  reason=SpeculationFailed
                                 action=RecompileAndReinterpret
eager re-queue (RecompileAndReinterpret) org/h2/mvstore/MVStore.getMapId
```

One deopt, one recompile, IR banned for that method, single-pass body restored.
`DOD RESULT OK`, regression suite 92/92.

#### TAKEN, at last

`plant_uncommon_trap`'s own doc has promised this since it was written —
"[`ir_trap_census`] counts what was PLANTED by cause; the runtime side counts
what is TAKEN. A cause whose taken count is not ~0 has had its coldness
argument refuted" — and the runtime side did not exist. It does now, and it is
the number that refuted the argument: `TAKEN at runtime: 1` on H2 by default,
and 200,000 on `UnresolvedTrapProbe`.

#### What is NOT fixed

The fix restores the method's compilation; it does not stop a HOT trapped path
from deopting. `UnresolvedTrapProbe`, whose trapped path runs 200,000 times,
still reads 200,000 deopts — 20 of them `RecompileAndReinterpret` and the rest
`MakeNotCompilable` once the per-method count crosses its threshold. The IR ban
takes effect (`memo_skips=5`) and the re-queues happen (20), but the trapping
artifact is not displaced from under the running caller, so calls keep entering
it.

So: a trap on a COLD-ish path (the real H2 case, one fire) is now cheap and
self-correcting. A trap on a HOT path is still a permanent deopt loop, and the
remaining blocker is artifact displacement, not the deopt policy. That is the
next piece of work, and it is why
`CRATONVM_JIT_IR_UNRESOLVED_CLASS_TRAP` — whose sites are demonstrably hot —
stays OFF.

#### Artifact displacement: what it is, and what could actually be done

The residual above was "the trapping artifact is not displaced from under the
running caller". Tracing it named the mechanism exactly.

`main` bakes a raw direct `CALL` to the callee's entry
(`CRATONVM_DBG_JIT_FIELD_SITES` prints one `[jit-emit-direct]` per baked call,
here at pc=23 and pc=92). Eviction machinery all fires on the first trap:
`jit_cache.remove` runs `invalidate_matching`, whose transitive reverse closure
evicts *the direct caller too*, and the epoch is bumped. It demonstrably works
— the tracer shows `main` recompiled and baking a SECOND callee entry.

And it changes nothing here, because the caller is a single **in-flight
invocation** running a loop: 200,000 calls happen inside one `main` frame, whose
already-executing code holds the old address. Cache eviction governs future
ENTRIES to a method, not a frame midway through one. Displacing that needs the
caller's frame deoptimized, or the callee's entry patched to a re-dispatch stub
— and `MakeNotEntrant` is an enum variant in this tree with no entry-patching
behind it, so there is no cheap displacement to reach for. Writing one is real
runtime surgery (atomic patching of live code under W^X, against threads that
may be at the entry) and is not something to bolt on beside a trap fix.

What IS in reach is refusing to compound the damage. The deopt bookkeeping
exists to DECIDE a policy; once IR is banned for the method and the recompile
has happened, every later trap re-runs a decision already taken — and that is
not free, because `SpeculationFailed` escalates on the per-method deopt COUNT.
A trapped site in a long-running caller therefore drove the method to
`MakeNotCompilable` purely by being reached often: the exact outcome the fix
exists to prevent, arrived at by a different road.

So the decision is taken once, and the rest resume in the interpreter.

| `UnresolvedTrapProbe` | before the trap fix | after it | after this |
|---|---:|---:|---:|
| deopt EVENTS | 200,000 | 200,000 | **1** |
| `MakeNotCompilable` | 200,000 | 199,980 | **0** |
| site traps TAKEN | (uncounted) | 200,000 | 200,000 |
| re-fired after the decision | — | — | 199,999 |

The trap still fires 199,999 times — that is the in-flight caller and nothing
short of displacement removes it — but it now costs an interpreter resume
apiece instead of deopt bookkeeping plus permanent blacklisting, and the method
is fully compiled again the moment that frame returns. The residual is
COUNTED rather than hidden: `ir site traps re-fired after the decision` is a
direct measure of how much a real workload would gain from entry patching, and
on H2 it is **0**.

H2 unchanged: 21 planted, 1 taken, 0 re-fired, 0 blacklisted, suite 92/92.

### A census number is only comparable to one taken BACK TO BACK

Several deltas in this file were quoted from runs taken minutes or hours apart —
`refused_method 57 -> 45` for the `Math.abs` work most recently. That is not a
valid delta, and this section is the measurement that says so.

Twenty-four runs of the same workload on the same binary, no lever changed:

| batch | `accepted` | `refused_method` | `lowered_as_arithmetic` | `bounds_emitted` |
|---|---|---|---|---|
| A (7 runs) | 527-585 | 43-54 | 19-24 | 129-164 |
| B (5 runs) | 526-582 | 44-54 | 19-24 | 130-166 |
| C (12 runs) | 560-578 | — | — | — |

Across all 24, `accepted` spans **526 to 585 — 11%**. Within a contiguous batch
it is roughly ±1%. The between-batch spread is an order of magnitude larger
than the within-batch spread.

An intermediate reading of the first twelve runs looked cleanly BIMODAL — nine
at ~529 and two at ~584, well separated and internally tight — and that reading
was wrong. The third batch sat at 570-578, between the two supposed modes, which
no two-mode model produces. Twelve samples were enough to fit a story and not
enough to test it.

What survives is simpler and more useful: **the census drifts with ambient
machine state**, and the drift dwarfs most of the effects being reported. The
cause is not chased here — background compilation is a race between the
compiler threads and the workload's own progress, and which methods cross their
thresholds depends on how the machine feels — but the consequence is concrete:

> A census delta is only meaningful between runs taken **back to back**, in one
> batch, on one binary, with one lever changed. Exactly the discipline the
> timing work already uses; it applies to deterministic-looking counters too,
> because they are not deterministic.

#### The claims this corrects

`refused_method 57 -> 45` for `Math.abs(F)/(D)`: the 57 and the 45 came from
different sittings and the gap is inside the 43-54 range a single configuration
produces. The DIRECTION is not in doubt — the two families are recognised and
lowered, `every_declared_family_is_recognised` proves the recognizer sees them,
and `Math.abs` disappears from the per-family refusal breakdown — but the
MAGNITUDE was never measured. The honest statement is "two families moved from
refused to lowered", with no number attached.

The same caution applies to every single-run census figure quoted above. The
ones taken as an A/B pair in one sitting — the range-BCE arms, the
over-intrinsic arms, the trap on/off arms — are unaffected, because both halves
were taken back to back. That is the whole distinction.

### `test_jit_cache_clear_all_evicts_entries`: 111 runs, 0 failures

Recorded on the residual list as "1 failure in 11 parallel runs, 0
single-threaded — measured, not proven pre-existing". It was unmeasurable for
most of a day because the test target would not build: release ended in `rustc`
exit 101 and debug in `os error 112, not enough disk space`, on a machine with
**0 bytes free of 930 GB**. Once space came back it built immediately, which
says the exit-101 was the disk too.

Then a false start worth recording, because it produced a clean-looking zero:
the first attempt ran the `cratonvm-vm` lib-test binary and got 0 failures in
30 — from a binary that does not contain the test. `--list | grep -c` said
`present: 0`. The test lives in `jit/src/lib.rs`, not the vm crate. A pass count
from a binary that never ran the test is the vacuous green this file keeps
re-learning, and the only thing that caught it was asking the binary whether it
had the test rather than assuming the filter matched something.

With the right binary, four shapes:

| shape | runs | failures |
|---|---:|---:|
| the test alone, `--test-threads=1` | 30 | 0 |
| the `jit_cache` module, 8 threads | 20 | 0 |
| the FULL binary (2,283 tests), default parallelism | 25 | 0 |
| **six CONCURRENT full binaries**, deliberate max contention | 36 | **0** |

**111 runs, no failures.** At 0/111 the 95% upper bound on the rate is about
2.7%, which excludes the 9% that "1 in 11" implies. The last shape matters most:
the original sighting was inside a `cargo test` run, which starts many test
binaries at once, so six concurrent copies of the heaviest one is a harder
version of the same condition.

Two readings survive and the second is better supported. Either the single
observed failure was far rarer than one in eleven, or — consistent with the G1
family in this same session, where failures under load turned out to be
`rc=124 HARNESS FAULT — TIMED OUT` and the HotSpot ORACLE failed too — it was
whole-machine contention during a `cargo test` that was also compiling. The
recorded rate is not supported either way, and the residual should say so rather
than carry a number nothing reproduces.

### The hibernate inlining result REVERSES on a disjoint sample — there is no positive throughput result

This file said, earlier today:

> **This is the first positive throughput result for IR inlining on real code
> in this document** […] 26 of 37 is not something a fair coin does.

It was tested on the rest of the suite and it does not hold. Same binary (one
`mtime`, both runs on it), same harness, same lever, disjoint classes:

| sample | classes | A (inline ON) faster | median delta | median noise | z |
|---|---:|---:|---:|---:|---:|
| hibernate, classes 0-39 | 37 | 26 (70%) | +0.3% | 6.9% | **+2.47** |
| hibernate, classes 40-119 | 78 | 28 (36%) | **-0.4%** | **2.7%** | **-2.49** |
| **hibernate pooled** | **115** | **54 (47%)** | — | — | **-0.65** |
| netty | 46 | 19 (41%) | — | — | -1.18 |
| **every inline pair taken** | **161** | **73 (45%)** | — | — | **-1.18** |

Two disjoint halves of ONE suite, each "consistent, p < 0.05", pointing in
OPPOSITE directions, with z-scores that are near mirror images. Pooled, the
whole thing is a coin — and so is every inlining pair ever taken here, 73 of
161.

The second sample is the better one on every axis that matters: twice the
classes, and a median within-arm noise of 2.7% against the first's 6.9%. If
either were to be believed it would be the one saying inlining is SLOWER. The
honest reading is that neither is: **IR inlining has no measurable throughput
effect on hibernate**, and the earlier claim is withdrawn.

#### What went wrong, and what the harness now has to say

The design was right about the thing it was built for — a per-class ABBA pairing
does remove the drift that made a per-run comparison useless, and the noise
floor it reports is real. The error was in the inference laid on top: a sign
test over classes assumes the per-class deltas differ only by the lever plus
symmetric noise. They do not. Classes carry their own systematic
differences — how much of the run is JIT-visible at all, how much is MySQL
round-trips — and slicing a null effect into two class subsets can hand you a
significant count in either direction. Which is exactly what it did.

So a paired count is evidence about THE CLASSES IT WAS TAKEN OVER, and a
significant z is a reason to take a SECOND, disjoint sample — not a result.
This is the same lesson as the census drift recorded above, one level up: there
the trap was comparing runs across time, here it is generalising from a sample
to the suite.

`pair-ab.sh` prints the count, the z and the noise floor and it printed them
correctly both times. The verdict line is what over-reached, and it now says so:
SMALL BUT CONSISTENT requires a confirming disjoint sample before it means
anything.

### A one-lever A/B found the TRIGGER and I called it the defect

The question was whether `CRATONVM_JIT_IR_UNRESOLVED_CLASS_TRAP` could default ON
now that firing no longer blacklists a method. H2 said yes emphatically — 48
traps planted, **none taken**, `accepted` 579 -> 592, `lowered` 116 -> 127, no
blacklists. Thirty hibernate-reactive classes said the opposite, and the control
arm is where the interesting number was:

| arm (binary WITHOUT the deopt-sink fix) | ok | failed | TAKEN | `refusing side-effecting replay` |
|---|---:|---:|---:|---:|
| unresolved-class trap ON | 64 | 33 | 120 | 102 |
| the shipped default (indy trap ON) | 182 | 7 | 4 | **36** |
| all site traps OFF | **241** | **0** | 0 | **0** |

The third arm only got run because the second — the *control* — had 36 hard
errors sitting in it. One lever, 241/0/0 against 182/7/36, and the conclusion
looked inescapable: the default-ON `invokedynamic` trap was costing 59 passing
tests and 36 `InternalError`s in the stock configuration. The callees named in
those errors were exactly the ones tabulated on the `TransferToInterpreter`
known-issue page filed that morning. So site traps were switched to default OFF.

**That was wrong, and the check that caught it was re-reading dev before
pushing.** Another session had spent the same afternoon on the same family from
the other end and found the actual defect: one deopt SINK aborted on a trapped
frame that its sibling sink resumed (`CRATONVM_JIT_DEOPT_SINK_RESUME`, default
ON). Re-measured on a binary carrying their fix:

| arm (binary WITH the deopt-sink fix) | ok | failed | TAKEN | `refusing side-effecting replay` |
|---|---:|---:|---:|---:|
| site traps ON | 239 | 0 | 7 | **0** |
| site traps OFF | 241 | 0 | 0 | **0** |

Zero errors either way. The trap was never the defect — it was the thing that
*produced the deopts* the broken sink then mishandled. Traps fire (`TAKEN=7`)
and nothing breaks. The default-OFF flip is reverted.

And the arm that started all this reverses too. The unresolved-class trap, the
one that read `ok=64 failed=33` and looked destructive, on the fixed binary:

| arm (binary WITH the deopt-sink fix) | ok | failed | TAKEN | `refusing side-effecting replay` |
|---|---:|---:|---:|---:|
| `CRATONVM_JIT_IR_UNRESOLVED_CLASS_TRAP=1` | **241** | **0** | **112** | **0** |
| the shipped default | 241 | 0 | 9 | 0 |

One hundred and twelve traps fired, no failures, no errors, the same pass count
as the default. Every number this file has ever recorded against that switch —
"200,000 deopts", "traps forever", "craters hibernate" — was measuring a broken
deopt sink through it.

So the CORRECTNESS objection to `CRATONVM_JIT_IR_UNRESOLVED_CLASS_TRAP` is gone.
It stays OFF anyway, because the case for turning it ON was always throughput
(+13 accepted bodies and +11 lowered on H2) and that has never been measured —
and the inlining reversal recorded above is a fresh demonstration of how hard
that measurement is to get right here. What has changed is the reason: it is no
longer "this is harmful", it is "this is unmeasured".

#### The methodological point, which is the part worth keeping

A single-lever A/B is airtight about one thing and silent about another. Turning
site traps off removed the errors, and that PROVED the trap is on the causal path
to the failure. It said nothing about whether the trap or something downstream of
it was the defect — and a kill switch answers identically in both cases. Every
feature that produces deopts would have "fixed" this bug by being switched off.

The tell was available and I read past it: the failing arm's errors named
`can_deopt_resume=false`, a property of the METHOD and the SINK, not of the trap.
A lever that removes a symptom by removing its input is a bisection step, not a
diagnosis.

Two further notes. `TAKEN` undercounts on the pre-fix binary by construction:
the counter sits in the resume path past the point where the resume succeeded,
so a trap whose resume was REFUSED never reached it — `TAKEN=4` beside 36 errors
was 4 traps that resumed and 36 that could not, and that discrepancy was itself
a signal the trap was not the whole story. And H2 remains unable to see any of
this: it plants traps and fires none, so its census reads as pure gain either
way. A lever whose entire risk is what happens when a trap FIRES has to be
measured where traps fire.

#### What this does retire

The case for artifact displacement — entry patching, a real `MakeNotEntrant`.
Its premise was that a trap on a HOT path needs the trapping artifact displaced
to be survivable. With the sink fixed, traps fire on hibernate and nothing
breaks, and the residual re-fire counter reads 0 on both real workloads. Nothing
here is asking for live-code patching, which in this tree means atomic surgery
under W^X with no safepoint hook and no existing patch site to copy. It is not
being built, and this is the measurement that says why.

#### The split-half check, so the next run catches this itself

The reversal above took a day and a second deliberate sample to find. It should
not have: the evidence was inside the FIRST run, in the classes it had already
measured. `pair-ab.sh` now scores its own two halves and prints them:

```text
sign test on the paired count: z = +2.85  (consistent, p < 0.05)
split-half   : first 20 classes z = +4.02 | last 20 classes z = -0.45
  ** THE HALVES DISAGREE IN SIGN. ...
VERDICT: UNMEASURABLE (SPLIT-HALF DISAGREEMENT). A wins 29 of 40
         overall, but the two halves of this run point OPPOSITE ways.
```

The discriminating case is the pair the self-test is built on: two runs with the
IDENTICAL pooled count — 29 of 40, z = +2.85 — where one has halves at
+1.79/+1.79 and the other +4.02/-0.45. The first is reported as SMALL BUT
CONSISTENT; the second is refused. A check that could not separate those two
would be doing nothing, which is why the self-test asserts the pooled counts
match before asserting the verdicts differ.

`tools/suite-pair-ab/selftest.sh` runs the real awk out of `pair-ab.sh` rather
than a copy, so the two cannot drift, and it was verified to FAIL when the
detector is disabled.

**The first version of that check was confounded, and it was my own.** Rows are
written in RUN order, so first-half-vs-last-half is also EARLY-vs-LATE across a
run that can span an hour: a disagreement could equally be the classes or the
host drifting underneath. It now computes an ODD/EVEN split as well, which
interleaves the same classes in time and is therefore blind to drift:

| first/last | odd/even | reading |
|---|---|---|
| agree | agree | stable direction |
| **disagree** | agree | **drift during the run**, not a class effect |
| — | **disagree** | **genuinely class-dependent** — does not generalise |

A near-zero z has no sign to disagree with, so a disagreement counts only when
both sides clear \|z\| >= 1; without that gate the drift fixture reads +0.00
against -0.45 and gets reported as class-dependence, which is how the bug was
found. And the "identical pooled count" pair now separates three ways rather
than two: `agree` reports a direction, `hidden` (one half carries it, the other
is flat) keeps the direction but is annotated as resting on half the sample, and
only a real sign reversal is refused. Its own first draft had the bug this file keeps meeting:
the "same pooled count" assertion compared two EMPTY strings and reported `ok`
when the summary had not run at all.

### And the trap does not buy anything measurable either

With the correctness objection retracted, the only thing keeping
`CRATONVM_JIT_IR_UNRESOLVED_CLASS_TRAP` off was that its benefit had never been
measured. Measured now — 57 hibernate-reactive classes, ABBA per class, on a
binary with the deopt-sink fix:

```text
A faster than B in 34 of 57 classes            z = +1.46  (a coin)
split-half   : first 28 z = +1.13 | last 29 z = +0.56
median per-class delta : +1.4%
median within-arm noise: 10.7%   (SAME config, two runs)
VERDICT: UNMEASURABLE
```

No effect. The two halves at least AGREE in direction this time — both weakly
positive, so this is not the reversal pattern the split-half check exists to
catch — but the pooled count is a coin and the effect is a seventh of the noise
floor.

That floor is the caveat and it is a large one: **10.7%, against 2.7% on the
quiet-host hibernate run earlier the same day**. Load was 3.5-6.5 on 8 cores
throughout. This is a weak measurement, and it is reported as one. The
sub-result that "of the 10 classes whose own delta beats their own noise, A is
faster in 9" is NOT quoted as evidence: it is a selection conditioned on the
noise estimate, over ten classes, and this document has already been burned once
today by a significant-looking count over a small sample.

So the switch stays OFF with both halves of its case now measured rather than
assumed:

- **not harmful** — 112 traps fired across 30 classes, `ok=241 failed=0`,
  zero `refusing side-effecting replay` (the old "it craters hibernate" was a
  broken deopt sink seen through this switch);
- **not beneficial** — no measurable throughput effect, on a noisy run.

What would settle it: the same 57-class A/B on a host at load < 2.5, which is
what produced the 2.7% floor. Anything less and the answer is the noise floor,
not the lever.

#### `tools/h2-ab` — so an H2 claim can be re-checked at all

The audit above found that no H2 throughput number in this file is
reproducible: the harness that produced them was never committed, and the
corpus's `test-classes/` directory is empty on this box, so even the 21-class
shape cannot be rebuilt. `tools/h2-ab/h2-ab.sh` is the replacement, and its
limits are stated in its own header rather than discovered later.

It carries over the two rules that made the original trustworthy — ABBA per
round (BAAB on odd rounds, so order bias cancels across rounds too), and a
CONTROL arm measured twice every round whose spread is the noise floor, with an
effect inside the floor refused. It adds a third: with no control pair at all it
reports NO CONTROL rather than comparing against nothing.

**What it cannot do, and the header says so.** One timed unit means no per-class
sign test and no split-half check — the two things that caught a false result
the same day. It answers only "is this bigger than the host's own same-config
spread", the weakest of the three questions, and its positive verdict tells the
reader to confirm on a second workload. `suite-pair-ab` remains the better
instrument wherever the workload is fork-per-class.

`--analyze <samples.tsv>` runs the statistics on recorded samples with no VM, so
`selftest.sh` can check the maths directly. It was mutation-tested, and the
mutation testing paid immediately: an assertion that checked only the VERDICT
passed when `worst` was changed to "whichever round awk visited last", because
awk walks an associative array in unspecified order and both readings happened
to give UNMEASURABLE. The fixture now puts the worst round FIRST and asserts the
reported floor VALUE (30.0%), which fails at 0.3% under that mutation.

First real run, corroborating the Azure result on different hardware: the
unresolved-class trap reads an effect of **-0.2% against a 7.5% floor** —
UNMEASURABLE, agreeing with hibernate's z = +1.46.

### The unboxing accessors, lowered — the first MEMORY family the IR tier has

`Long.longValue()J` and `Integer.intValue()I` were the two largest single
entries on the call-site-intrinsic refusal list (8 and 2-3 sites on H2). They
are now lowered by the optimizing tier instead of refusing the method, and they
are the first family it lowers that touches the HEAP rather than registers.

```
[ir] unbox-intrinsics UnboxIntrinsicProbe.sumLong([Ljava/lang/Long;)J:
     1 site(s) lowered as a guarded field load
```

Both families are gone from the H2 refusal breakdown — `java/lang/Long.longValue`
and `java/lang/Integer.intValue` no longer appear at all, against 8 and 2-3
before — and 11 sites lower per run. `refused_method` reads 43-45, which is
inside the 43-54 band one configuration produces, so **no delta is claimed
there**: the disappearance of the two families from the per-family list is the
engagement evidence, not the total.

#### Why this one was left until last

The arithmetic families are pure register work. These read a field, and the byte
offset of that field is not a compile-time constant: a compact instance keeps
the payload at a registered body offset, a legacy one inside its 16-byte `Value`
cell, and BOTH shapes exist in one heap because different allocators build
different cells. So the lowering is not a load — it is a null check, an exact
receiver class guard, a per-object test of the header's compact bit, and then
one of two loads.

The node carries only the guard class id. Offsets are re-derived at lowering
from `ir::unbox_offsets`, which is the same `AtomicLongFieldLayout` /
`AtomicIntFieldLayout` the single-pass backend asks — because two backends
disagreeing about where a field lives is not a wrong answer, it is a wild read.
A test pins that agreement.

#### What the codebase made me declare

Adding one `ir::Op` variant failed to compile in FIVE places, every one of them
a deliberate forcing function, and each wanted a different decision:

| where | what it forced |
|---|---|
| `ir_verify::expected_arity` | the node's input shape (`[ctrl, mem, obj]`) |
| `declared_lowering` | that it produces a value, not an effect |
| `op_representatives` | a concrete instance for the coverage tests to drive |
| `op_defines_result_slot` | that it allocates a result slot |
| `regalloc::ir_op_defines_value` | the same, for the LIVENESS model — without it every method containing the node silently loses register residency |

And a sixth asked for a judgement rather than a fact: the arm ends in exactly
one `store_rax`, so `every_eligible_op_is_claimed_or_explicitly_rejected`
demanded it be claimed for the home-drop optimization or explicitly rejected
with a reason. It is **rejected**: unlike every claimed op its arm is not
straight-line — two deopts and a layout branch precede that store — and
reasoning about what each deopt edge sees is exactly what that list exists to
stop being done casually. Claimable later with a measurement; not worth a wrong
answer to save one store.

#### Verification

`probes/UnboxIntrinsicProbe.java` mixes boxes from BOTH allocation paths on
purpose — values inside the `Integer`/`Long` cache come from a preallocated
table, values outside it are freshly allocated — so a compact/legacy branch that
was wrong for either shape returns garbage for one group. Every answer is
checked against a value computed without the accessor, so it cannot pass by
agreeing with itself, and a null receiver must still raise NPE rather than read
offset 0 of nothing.

It agrees with HotSpot exactly (`SUMS -4 -3`) under
`CRATONVM_COMPACT_REF_FIELDS=1` **and** `=0`, which exercises both offset
derivations. The kill switch was checked in the direction that matters: with
`CRATONVM_JIT_IR_SCALAR_INTRINSICS=0` the two families reappear in the refusal
log (86 lines); on, zero. Suite 92/92, 2,285 jit tests green.

**Not claimed: any throughput number.** `h2-ab` says an effect this size is
inside this host's noise floor, and today's two withdrawn results are the reason
that is left as a measurement someone takes on a quiet host rather than a figure
asserted here.

---

## 2026-09-11 — a phi copy staged in RAX, and a census that redirected the work

Two things landed, and the second is the reason the first was findable.

### The census: operand POSITION is 3.8%, not 82%

`c2-one-carry-slot-is-the-frame-traffic-ceiling-FIXED-20260910.md` closed by
naming `ir_schedule::pair_single_use_operands` as the next lever, on the
strength of the deferred carry declining **82%** of its candidate windows for
`operand_position`, and asking for a census of that pass because *"which of
those four dominates is not yet counted"*.

Built (`ir_schedule::PairCensus` — one counter per `continue`, under a
`debug_assert`ed accounting identity so a new reason cannot read low) and run
over 188 probes, **34,289 windows**:

| cause | share |
|---|---:|
| producer is multi-use | **78.3%** |
| producer's arm not certified by `op_home_is_one_store_rax` | **16.2%** |
| the four POSITION buckets, together | **3.8%** |
| paired | 1.7% |

The two figures have different denominators and both are right — the carry's
82% is over windows where a consumer already takes its first operand in RAX,
the pass's is over every `(consumer, operand)` pair — but only the second says
what the PASS could act on. **94.5% of the operands it sees were never
eligible**, and no scheduling change reaches them. The carry page now carries
the correction beside its prediction.

Where it points instead: `producer_arm`, and inside it `Op::Load`. A
`getfield`'s three lowering paths are mutually exclusive and each ends in one
`store_rax` with RAX holding the result, but the mechanical test counts
`self.store_rax(slot);` textually and sees three, so the op is excluded from a
certification it appears to satisfy. Unowned, and the per-op breakdown that
would size it is one more counter.

### The change: `CRATONVM_JIT_IR_PHI_COPY_DIRECT`, default ON

Every phi edge copy staged through RAX and then published into the phi's own
register. On a loop back edge that is `mov rax,r15` / `mov r12,rax` for a
resident source and `mov rax,[slot]` / `mov rbx,rax` for one still in its
word — **one instruction per loop-carried value per iteration**, to move a
value that is already in a register or already in the word.

`emit_copy_op` now reads straight into the phi's register when it has one, so
the publish disappears. It is the same program: the write to that register
moves earlier inside ONE `CopyOp`, crossing only that copy's own store, so
`resolve_parallel_copy`'s cross-op invariant is untouched.

`FieldLoop.sum`'s back edge goes from four instructions to two, its loop body
from 26 to 24, and its body from 1071 to 1059 bytes. Measured
(`tools/tier-ab/cpu-ab.ps1`, four invocations, all outside their own floors and
agreeing on the sign, the last re-taken after merging `dev`):
**−4.8% / −7.0% / −7.3% / −10.5%**, i.e. about **1.08x** on
that shape, and the tiering inversion there goes **1.208x → 1.11x** on this
host. `FieldLoop.sumWide`, which folds twice as many copies, is
**UNMEASURABLE** — four times the arithmetic per iteration, so the same two
instructions are a quarter of the share.

**A restriction worth copying, not just recording.** The first version also
staged when the phi's home store survived, and wrote that home from the staged
register. Replacing that store with `panic!()` left the **entire**
`cratonvm-jit` suite green — 2356 unit tests and 145 differential tests — so
the branch was shipping unexercised; and forcing it to run still could not
catch storing the WRONG register, because nothing reads a resident phi's home
word back. The change was narrowed to the home-dropped case (where the store
does not exist at all) rather than the test weakened, and
`a_phi_copy_that_keeps_its_home_is_byte_identical` pins the exclusion by
demanding byte equality. One instruction given up on a path nothing reaches,
in exchange for every remaining path being one the suite can fail.

### And a third witness that the register file is not the constraint

`c2-the-gp-register-file-is-not-the-binding-constraint-20260910.md` argued from
census counters that widening the GP file does not pay. The disassembly now
shows what the two extra Win64 registers actually buy: with
`CRATONVM_JIT_IR_GP_WIDE=1` the induction variable's home store AND its reload
on the back edge disappear entirely — `lea r15d,[rbx+1]`, no frame traffic —
which is exactly the store-to-load-forwarding pair this document traced the
tier's residual 1.36x to. It still measures **+0.4% against a 0.8% floor**.

So that chain is not this loop's critical path, whatever its latency is in
isolation, and the next person reaching for "the loop-carried value round-trips
through the frame" has a measurement to answer first.

Full write-up, including the per-iteration instruction budget that says where
the remaining gap is — **20 instructions against 26**, with the two survivors
being that this tier does not unroll (so it pays the safepoint poll and the back
edge every iteration rather than every fourth) and that its receiver null check
is explicit where the single-pass tier's is implicit — in
[`internal/performance/c2-the-phi-copy-staging-register-20260911.md`](internal/performance/c2-the-phi-copy-staging-register-20260911.md).

### The same budget's next line: the loop branched the wrong way

`FieldLoop.sum` takes **four** branches per iteration at this tier against the
single-pass tier's one, and one of the four was free:

```asm
25d: cmp ebx,r14d
260: jl  +5        ; to the loop body -- TAKEN every iteration
266: jmp exit      ;   ...skipping this
26b: <loop body>
```

The fused-branch arm picks its fall-through edge from `branch_hints`, which is
empty without `CRATONVM_TIER_PGO`. The fallback that left behind — *the `true`
edge is the near one* — is inverted for every javac counted loop, and for a
reason this document already records in the range-BCE closeout: **javac puts the
loop body on the FALSE edge**, because `for (i = 0; i < n; i++)` compiles to
`if_icmpge exit`. So the near edge was the loop EXIT, the exit was not the next
block, and `ir_fallthrough_enabled`'s `JMP rel32` elision — default-ON since
2026-09-09 and built for exactly this — could never reach it.

`CRATONVM_JIT_IR_BRANCH_LAYOUT_POLARITY` (default ON) takes the fall-through
edge from the block LAYOUT when there is no hint: `layout_hot_paths` is
default-ON, needs no profile, and `block_idx + 1` is its decision. The sequence
becomes one not-taken `jge exit`: **−5 bytes, −1 instruction, −1 taken branch
per iteration**, and 41 branches take it on `CratonBenchC2`.

**It measures nothing** — UNMEASURABLE on `FieldLoop` (+0.6% against a 0.6%
floor) and on `CratonBenchC2` (−2.5% against a 4.8% floor), checksums identical
throughout. It ships ON because it is weakly better in both instructions and
taken branches and strictly better whenever the near edge would otherwise need a
`JMP`, not because anything here shows it pays.

**It does not override a profile hint, and a test caught it trying.**
`step4_ir_lower_consumes_branch_bias_hint` went red on the first version. The
interaction it exposed is a real gap: `ScheduleOptions::branch_counts` is
documented as taking the same per-bci bias the lowerer takes, and
`production_schedule_options()` leaves it **empty** — so a profile informs the
polarity of one `Jcc` and never informs which block is placed next. Populating
it is small, and nobody has.

**And a harness finding worth more than the number.** The first run reported
+1.3% against a **0.0%** floor — the tightest this apparatus has printed, and
meaningless: Windows accounts CPU in ~15.625 ms ticks, the samples were 0.586 s,
so one tick was 2.7% of a sample and both medians had merely landed on the same
one. `cpu-ab.ps1` now prints the tick as a percentage of the median and refuses
a verdict inside it. That is the second way a clean floor misleads — the first
being drift between invocations (§5.2 of the GP-register page) — and both make a
tight floor read as permission to stop.

### CORRECTION: "the optimizing tier does not unroll" — true, and not for the reason implied

This document has said since 2026-09-03 that the optimizing tier does not
unroll, priced it at about 1.12x on a counted loop, and listed it as the
largest remaining item in that tier's per-iteration budget. All three stand.
What does not stand is the conclusion anyone would draw from them — that an
unroller needs writing.

**`ir_optimize::unroll` exists, is default-ON, and recognises a javac counted
loop exactly.** Driven against a real bytecode-built `for (i = 0; i < 5; i++)
a += i;` it reports `trip=5 init=0 stride=1` and then declines, silently, on the
`body_named_by_safepoint` refusal — whose escape hatch is gated on
`CRATONVM_JIT_IR_DROP_UNREACHABLE_HOMES`, **default OFF**. With that flag set,
the same loop unrolls. The flag's own sibling
(`CRATONVM_JIT_IR_REG_AUTHORITATIVE`) rests on the identical prediction, was
soaked and flipped ON on 2026-09-09, and says so in its doc comment; the flag it
names was never revisited.

**And that would not reach the loops that matter.** `ir_optimize::UnrollCensus`
— one counter per `continue`, under a closing identity — says every counted loop
in both benchmark suites has a RUNTIME bound, which full unrolling can never
serve:

| | CratonBenchC2 | CratonBench |
|---|---:|---:|
| loops found (merges − not_single_backedge) | 13 | 6 |
| of which runtime-bounded | **6** | **4** |
| `safepoint_named` | 0 | 0 |
| unrolled | **0** | **0** |

So the thing to build is a PARTIAL unroller, in the single-pass tier's own shape
(keep the test in every copy, amortise only the poll and the back edge — no
trip-count arithmetic, so none of the overflow hazard the range-BCE closeout
records). It was designed and deliberately **not built**, because both of the
gates under which it could be written without touching deopt metadata measure
**zero**:

| gate | asks | CratonBenchC2 | CratonBench | `FieldLoop` |
|---|---|---:|---:|---:|
| whole method trap-free | `graph_cannot_deopt` | 0 of 6 | 0 of 4 | 0 of 1 |
| **cloned nodes all pure** | the real obligation | **0 of 6** | **0 of 4** | **0 of 1** |

The second is zero for the same reason these loops are worth unrolling:
`FieldLoop.sum`'s body IS a field read, and `Op::Load` is not pure.

**What unrolling actually needs is one thing, and it is the same for both
unrollers: a deopt point addressable per COPY rather than per bci.**
`DeoptimizationPoint` already carries `(native_offset, bci, frame_state)` and
two points may share a bci — the representation is fine. Two things collapse
them: `bci_native` keeps the EARLIEST offset per bci, so only copy 0 is
anchored, and `find_deopt_point` is an exact-offset binary search returning
`None` for the rest; and `graph.safepoints` has one snapshot per bci naming the
original nodes, so a later copy has no frame describing its own values. That is
a bounded change to three named places, and it is the prerequisite for every
version of this feature.

Full write-up, including the census, the refusal taxonomy and the partial-unroll
design that was not built, in
[`internal/performance/c2-unrolling-is-a-deopt-metadata-problem-20260911.md`](internal/performance/c2-unrolling-is-a-deopt-metadata-problem-20260911.md).

### FOLLOW-UP: the per-copy deopt frame, built (`CRATONVM_JIT_IR_PER_COPY_FRAMES`, default OFF)

The "bounded change to three named places" above is done, and it is off by
default because it is deopt metadata: the failure mode is a right-looking wrong
answer, not a crash.

* **`Node::frame_snapshot: Option<u32>`** — the per-copy identity, on the node.
  `None` on everything the builder makes, so the by-bci scan is unchanged for
  every compile that does not unroll. `Graph::set_node_frame_snapshot` refuses a
  snapshot whose bci is not the node's own.
* **`ir_optimize::install_copy_frames`** — one substituted snapshot per
  iteration; iteration 0 rewrites its own in place so `bci_native`'s anchor and
  the frame at it keep describing the same code.
* **`Lowerer::snapshot_native` / `resolve_frame_state_for_site`** — the anchor
  and the frame taken from the copy rather than from the bci.

Two places the design note did not name turned out to matter. **GVN's identity**
now includes `frame_snapshot`: two copies of a body compute the same value at
different program points, and merging them hands one copy's code the other's
frame. And **`ir_verify`'s duplicate-bci rule**, which existed because of this
exact collapse, is now *"a duplicated bci is a violation unless every snapshot at
it is claimed by a node"* — an unclaimed duplicate is still the bug, and is what
a half-finished copy looks like.

**One thing the end-to-end run taught that is not about unrolling.** The probe
that exercises a deopt out of copy 3 still crashed on a default run, and the
reason was `ir_evidence::accept`: it priced the unrolled C2 body as not worth
publishing and handed the method back to the single-pass tier. The C2 body was
never running. `CRATONVM_C2_ACCEPT=always` installs it, and then all three
shapes match HotSpot exactly (20 000 `NullPointerException`s out of a cloned
body, each resuming in the copy that trapped). Worth remembering generally: with
an acceptance gate between a transform and its execution, "the checksum matched"
can be a statement about code that never ran.

**It wins nothing measurable yet, and that is expected.** The census above says
every counted loop in both suites has a runtime bound, so `per_copy_frames`
(a new sub-count of `unrolled`) is zero there. What it buys is that the sentence
the previous page ended on is no longer owed: the partial unroller can now be
written against a frame mechanism instead of around one.

One cost is worth knowing before it is discovered: **safepoint slots are DCE
roots**, so per-copy frames keep every iteration's intermediates alive to
describe them. On the `for (i = 0; i < 5; i++) a += i;` fixture the loop folds to
`Const(10)` and five `Const` nodes survive anyway, materialised purely for the
frames. The narrower fix (root only snapshots that can be consulted) is a DCE
change, not this one.

Full write-up in
[`internal/performance/c2-per-copy-deopt-frames-20260911.md`](internal/performance/c2-per-copy-deopt-frames-20260911.md).

### FOLLOW-UP: the partial unroller was written (`CRATONVM_JIT_IR_PARTIAL_UNROLL`, default OFF)

**2026-09-11, same day.** It keeps the loop test in every copy — so no
trip-count arithmetic and no speculation — and sends each copy's failing test
**back to the header** rather than to a new exit merge, which is what keeps the
transform closed under the loop and leaves every post-loop use and safepoint
slot untouched.

It is **correct and it is not faster**: 9 alternating pairs on
`bench/C2PartialUnrollProbe.java` read 356 ms rolled against 362 ms unrolled at
factor 4 — a ratio of 0.98 against a ±8% spread — with checksums matching
Temurin 25 on trip counts both divisible and not divisible by the factor. The
per-iteration instruction count *does* fall, 20 to 16.25. It buys nothing
because the rolled loop keeps `a` and `i` in `rbx`/`r15` with **no memory
operand in its loop at all** and the unrolled one spills every carried value:
`sink_pure_nodes` moves a node only when the loop depth strictly DECREASES, and
every copy of an unrolled body sits at the header's own depth, so all four are
computed above the first test and eight intermediates contend for a
five-register file.

The sentence at 1437 and item 1 at 1484 both need a caveat now. The optimizing
tier *can* unroll; unrolling is not by itself what the baseline's 4x buys. The
baseline also colours its locals into callee-saved registers, and that is the
half this tier is still missing.

Two wrong-code defects were found on the way, both in shared code, both live
before this transform and reachable by anything that clones a control node: an
`If`'s successors were ordered by **node id** rather than by projection index,
and an OSR entry resolved a bci **two blocks claimed**. Both are fixed and
pinned by tests.

Full write-up, including why the obvious schedule-late fix is not landed, in
[`internal/performance/c2-the-partial-unroller-20260911.md`](internal/performance/c2-the-partial-unroller-20260911.md).
