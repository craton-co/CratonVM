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
3. **Nothing compares a C2 body against the C1 body it replaces.** The
   policy question is unchanged and deliberately still open — the obvious
   static metrics both misjudge the good cases, since a bigger body is usually
   inlining or unrolling and more call sites can be a callee's own calls after
   its frame was inlined away. What this pass adds is the DATA: with
   `CRATONVM_DBG=jitc`, a supersede prints `c1=<bytes> c2=<bytes>`. What it
   also does is remove the causes that made a C2 body worse — the tier now has
   an inline TLAB bump, gated inline reference stores, and a register file.
   **What that instrument then said is below.**
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
into it first. `blocked_deopt` is the counter that says what that would buy.

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
