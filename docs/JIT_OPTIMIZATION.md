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

Every part of the register choice is forced. They are callee-saved on **both**
ABIs, which this wiring needs because it has no reload machinery: a value's
register must survive a call by the calling convention rather than by analysis,
and that rules out even the otherwise-obvious System V candidates RSI/RDI. They
are untouched by this emitter's own tiers. The prologue saves them and every
exit restores them (`IR_GP_PROLOGUE_SAVED`), on the same footing as the XMM save
area and just as dynamically — a method that promotes nothing emits no save.

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

**This is not an implicit null check, and that was the choice, not an
omission.** The audit item asked for the HotSpot mechanism: let the load fault
on the null page and translate the signal. `vm/src/runtime/crash_handler.rs`
can already resume — the Windows VEH rewrites `RIP` and returns
`EXCEPTION_CONTINUE_EXECUTION`, and the Unix handler has the faulting PC, the
faulting address and `si_code` — and a lock-free append-only PC table would be
async-signal-safe. What is missing is **lifetime**: a `CompiledMethod`'s buffer
is unmapped on invalidation and its address is immediately reusable by the next
`alloc_executable` (`unregister_jit_method_name` exists for exactly this
reason), so a stale entry would recover at a PC that now belongs to different
code. Correct registration therefore has to participate in the code-cache
lifecycle, which is where that work belongs. Against that, the check being
removed is `TEST r,r; JZ rel32` — 9 bytes and two well-predicted µops — and
proving it away costs nothing at runtime and cannot mistranslate a signal.
Eliding by proof is strictly better than faulting where the proof exists; the
implicit check is only worth its machinery where it does not.

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
| IR-tier register residency (GP + FP files) | off — built and verified, flip wants a measurement | `CRATONVM_JIT_IR_LINEAR_SCAN=1` |
| Gated inline reference stores | **ON** where a collector publishes a plan | `CRATONVM_JIT_GATED_REF_STORE=0` |
| Operand-stack register cache beyond pure kernels | off (see the section above for the ARG_REGS collision) | `CRATONVM_JIT_OPERAND_CACHE=1` |
| Optimizing tier for allocation-bearing methods | off (a tier-population change, no longer a codegen gap) | `CRATONVM_JIT_C2_ALLOC_UPGRADE` |
| Inline TLAB bump in the optimizing tier | **ON** | `CRATONVM_JIT_IR_INLINE_TLAB=0` |
| `this` seeded non-null at method entry | **ON** | `CRATONVM_JIT_THIS_NONNULL=0` |
| `getfield` receiver null-check elision | **ON** | `CRATONVM_JIT_RECEIVER_NULL_ELIM=0` |

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
