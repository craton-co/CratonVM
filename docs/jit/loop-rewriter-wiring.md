# The bytecode loop rewriter, wired into `compile_with_param_slots`

Companion to [`loop-transforms.md`](loop-transforms.md) (the transform itself)
and [`loop-transform-wiring.md`](loop-transform-wiring.md) (the state before
this change, and the acceptance criterion it set out). This file records what
is wired **now**, how to turn it on, the side-table census, and what is not
validated.

Everything here lives in `jit/src/x64.rs` and `jit/src/x64/licm.rs`.

## Status

| Piece | State |
|---|---|
| `plan_loop_unroll` as the native unroller's admission oracle | wired (unchanged) |
| `plan_loop_peel` reachable from the planner | **wired** (the bypassable-header arm) |
| `plan_loop_version` reachable from the planner | **wired** (all three arms) |
| Compiling `xform.code` instead of `code` | **wired**, opt-in |
| Replicating the pc-keyed side tables | **wired**, all 21, one expression |
| Translating baked-in bcis via `bci_at` | **wired**, 4 sites |
| `osr_pc_to_native` rebuilt in original-bci space | **wired** (`rebuild_pc_to_native`) |
| `osr_dead_mask` rebuilt in original-bci space | **wired** |
| Translating `DeoptimizationPoint::bci` / `FrameState::bci` | **wired** (`build_and_record_deopt_point`) |
| That translation checked before the artifact is published | **wired** (`rewritten_deopt_points_are_publishable`) |
| Validated by anything larger than a unit test | **yes** — probes, benches, and three Spring Boot classes armed |

## How it is enabled

```rust
let previous = cratonvm_jit::x64::set_bytecode_loop_rewriter_armed(true);
// … compiles on THIS thread now route unrolling through the bytecode rewriter …
cratonvm_jit::x64::set_bytecode_loop_rewriter_armed(previous);
```

It is a **thread-local**, off process-wide, and nothing in the VM arms it. Two
reasons it is not an environment variable:

* this repo latches declared flags, and the declaration table
  (`types/src/flag_groups.rs::INVENTORY`) is in a crate this change does not
  touch. An *undeclared* flag is invisible to `runtime_var_os` under a flag
  override and would therefore be untestable — the exact trap recorded in
  `declared-flags-latch-so-set-var-is-invisible-to-tests`;
* the unit tests in `x64.rs` run concurrently in one process. A process-wide
  switch would leak one test's rewrite into another test's compile.

The cost on the default path is one `Cell<bool>` load per compile.

If a `CRATONVM_JIT_BYTECODE_UNROLL` flag is wanted later it must be added to
`types/src/flag_groups.rs` first and then consulted *once* to seed the
thread-local on each compiler worker — not consulted per compile, because
`runtime_var_os` on a declared flag reads the latched snapshot anyway.

`bytecode_loop_xform_rewrites_bytecode()` now answers this thread-local rather
than a hard-coded `false`, and `native_unroller_enabled()` is still its exact
complement — so arming the rewriter turns the native byte-copy unroller off in
the same motion. That mutual exclusion is a correctness requirement, not
tidiness: if both ran, `k+1` bytecode copies would be machine-code-duplicated
`k+1` more times behind one back-edge poll.

## What happens when it is armed

`plan_bytecode_loop_xform` picks **one** loop and rewrites the whole method.
One, because a `LoopXform` describes one rewrite and the primitives in
`licm.rs` do not compose two provenance maps. The first admitted loop in
`detect_loops` order wins (innermost-first for a nest). Every other loop is
shifted correctly by the rewriter and simply not duplicated.

Three arms, in the order they are tried:

1. **Peel**, for a loop whose header is *bypassable* — one an external branch
   can enter. The planner used to skip such a loop outright, because every
   speculating transform in this backend drops its pre-header when it sees one.
   After peel(1) the steady-state copy is entered only by fall-through and its
   own back edge, so the loop the emitter finds has a single entry and keeps its
   hoists.
2. **Unroll at the PGO factor**, when a profiled trip count is available.
3. **Unroll at the static heuristic's factor.**

The unroll band is character-for-character the native unroller's, including its
PGO arm and its "a PGO refusal does not fall back to the static heuristic"
behaviour. So arming changes *which machinery* unrolls a loop, not *which loops*
are eligible. Legality is `plan_loop_unroll`'s structural refusals, as it
already was for the native unroller.

Every arm goes through `plan_versioned`, which asks
`loop_analysis::analyze_counted_loop_at` + `CountedLoop::prove_trip_count_at_least`
for a `trip >= copies + 1` witness and, when it gets one that
`encode_preheader_guard` can emit, produces a **versioned** artifact: the
transform behind a pre-header check, an untouched copy of the loop on the
failing edge. `Static`, `Refused` and any versioning refusal fall back to the
plain transform, which is byte-for-byte what this emitted before versioning
existed — the guard is a profitability filter, not what makes peel or unroll
legal.

### Reachability

Off by default: the opt-in is a thread-local nothing in the VM sets, plus the
process-wide `CRATONVM_JIT=bytecode-loop-xform`.

It used to be dead code *twice* over — arming was not enough, because the first
whole-compile refusal (`DeoptRealEnabled`) fired on every compile,
`crate::deopt_real_enabled()` being default-ON. `loop-02` retired that refusal
along with `PreciseExceptionFrames` and `InvokedynamicPresent`, so arming is now
sufficient and `CRATONVM_JIT=bytecode-loop-xform` alone reaches loops under the
default configuration. On `core/spring-boot-autoconfigure` that is 95–98% of
compiles eligible (it was 0%) and 10–58 methods per test class actually
rewritten — see the loop-planner admission-gate design.

`the_wired_compile_path_reaches_a_loop_under_the_default_configuration` pins it,
by reading `crate::deopt_real_enabled()` rather than hard-coding it: the answer
must not depend on that flag any more.

`compile_with_param_slots` then

1. shadows `code` / `code_len` with the rewritten bytes and `exception_ranges`
   with `xform.exception_ranges` (a range enclosing the loop is widened over
   the copies);
2. rebinds all 21 pc-keyed tables through `LoopXform::replicate_pc_keyed`;
3. compiles exactly as before — the ~40 analyses and the emitter are
   unmodified, they simply see a different method;
4. changes coordinates back on the way out.

## Coordinate change: where a pc becomes a bci

The report's plan assumed step 4 could be "a single post-pass over the produced
`CompiledMethod`". **It cannot.** Four sites bake the bci as an *immediate into
machine code*, and the stub emitters run at the end of `compile_bytecode`,
before any artifact exists:

| Site | What it bakes | Consumer |
|---|---|---|
| `emit_bounds_check_stubs` | arg 4 of `jit_throw_aioobe` | reported bytecode index |
| `emit_exception_check_stub` | the arg of `set_throw_bci` | range-tested against this method's exception table |
| `emit_deopt_stubs` | arg 3 of `jit_uncommon_trap` | interpreter resume point |
| the `athrow` (`0xbf`) lowering | arg 2 of `jit_throw_exception` | `route_jit_exception_through_method`'s throw pc |

All four now call `Compiler::orig_bci`, which reads `Compiler::bci_provenance`
(`LoopXform::bci_of`, installed before `compile_bytecode`) and is the identity
when it is `None`. The fourth site is not in `loop-transform-wiring.md`'s list
— it was found by grepping every `pc as i32` / `bci as u64` immediate in the
backend rather than by reading that list.

Two things are deliberately **not** translated:

* **`OopMapEntry::bytecode_pc`.** `loop-transform-wiring.md` lists it as
  requiring `bci_at`. Reading the code says otherwise: the only value it is
  ever compared against is the one this same codegen stores into the frame's
  safepoint-id slot (`emit_pre_safepoint_spill`, `self.cur_bc_pc`), which
  `conservative_roots` reads back from `[rbp - sp_id_slot_off]`. Both sides
  live in output-PC space and are consistent. Translating one of them would
  actively break `find_oop_map_for_safepoint_id`, whose `.find()` would then
  see several maps sharing one key and pick an arbitrary one.
* **`pc_to_native`.** Internal to branch patching, never published.

`osr_entry_native` *is* published (as `CompiledMethod::osr_pc_to_native`) and
*is* indexed by interpreter bci by the runtime, so it is rebuilt with
`LoopXform::rebuild_pc_to_native`. This cannot be a translation-on-read: a bci
inside a transformed region has several native offsets and picking the wrong
one re-runs iterations. `rebuild_pc_to_native` applies `osr_entry_pc`'s
steady-state choice pointwise and leaves `-1` across the unrolled back-edge
gap, where OSR entry must be refused outright. `osr_dead_mask` is indexed by
the same bci and gets the same treatment with the same image choice.

`despec_contains` is consulted with `bci_at(loop_header)` at both call sites
(the aaload/arith LICM hoist filter and the speculative-BCE guard filter),
because the de-spec registry is keyed by interpreter bci.

## Table census: all 21, and the sound/refused split

Counted by reading the signature and body of `compile_with_param_slots`, not
taken from the report. All 21 are replicated in **one** `let (…) = match` so a
22nd parameter cannot be forgotten silently — omitting one is a destructuring
arity error, not a miscompile.

### Replicated, payload trivially shareable (13)

`multianewarray_info`, `field_info`, `static_field_info`, `new_info`,
`new_deferred_info`, `anewarray_info`, `anewarray_deferred_info`,
`direct_calls`, `ldc_info`, `ldc2w_info`, `branch_hints`,
`loop_unroll_hints`, `compact_field_info`.

Plain data. `direct_calls` goes through the same primitive via a packed tuple
because `JitDirectCall` (in `jit/src/lib.rs`) has no `Clone`; deriving it there
would let this call `replicate_pc_keyed` directly.

`loop_unroll_hints` is keyed by back-edge pc. Under `Unroll` an original
back-edge bci has exactly one image (only the last copy carries the back edge);
under versioning it has two — the guarded loop's back edge and the fallback's —
and the map stays well formed because those are two different keys. Its only
consumer is the native byte-copy unroller, which is off whenever any of this
runs.

### Replicated, payload is a READ-ONLY pointer — sound (3)

`typecheck_info` (`*const u8` class name), `invoke_info`
(`*const JitInvokeInfo`), `ldc_string_info` (`*const u8` interned string).

Immutable, caller-owned, outlive the compile. Sharing one target across copies
is indistinguishable from the single-copy case.

### Not replicated at all: the versioning guard's bytes (0)

A versioned rewrite inserts a pre-header check that is not a copy of anything.
Its bytes carry the loop header's bci — provenance must stay total, or
`Compiler::orig_bci` has nothing to answer with — but `outputs_for_bci` skips
them, so no table entry is ever replicated onto them. Keying a field resolution
or an inline cache to a synthetic `iload` would be silent, which is the same
failure mode this whole census exists to prevent.

That the guard *needs* no entry is a property of what it may contain:
`encode_preheader_guard` emits only `iload`, an integer constant push and one
`if_icmp*`. None of those consults any of the 21 tables.

### Replicated, payload is a MUTABLE pointer — sound, argued (2)

`mic_slots` (`*const JitMICSlot`), `pic_slots` (`*const JitPICSlot`).

The copies share **one** inline-cache slot. This is correct rather than a
compromise: an inline cache keyed on a call site sees the same receiver
distribution in every copy, which is exactly what happens today when a
non-unrolled loop executes many times. It is also the only sound option here —
the slots are allocated and owned by `jit/src/lib.rs::try_compile`, and minting
fresh ones per copy needs code outside this file. (The *native* unroller does
mint fresh slots, via `cloned_mic_slots`/`cloned_pic_slots`; it duplicates
machine code with a baked `imm64` and has no other choice.)

### Replicated, but the transform is refused when non-empty (3)

`non_escaping_new`, `inline_sites`, `indy_info`.

* `non_escaping_new` is replicated and is *not* a refusal — it is listed here
  only because the authoritative escape analysis re-runs on the rewritten code
  whenever `new_info` is non-empty, so the replicated parameter is consumed
  only on the `new_info.is_empty()` path.
* `inline_sites` — **refused** (`InlineSitesPresent`). An inlined callee
  contributes its own bci space (`deopt-inline-scopes.md`) that this
  caller-only provenance map does not describe.
* `indy_info` — **refused** (`InvokedynamicPresent`). The `0xba` lowering is an
  unconditional trap that records an `UnreachedCode` snapshot through
  `emit_osr_exit_map_at_reason`, whose `DeoptimizationPoint::bci` the VM
  resumes at — and unlike the other snapshot paths it is **not** gated on
  `deopt_real_enabled()`, so it would fire in production.

Both are still routed through `replicate_pc_keyed` so the census has no
"handled elsewhere" entry and so lifting the refusal is a one-line change.

### Not parameters, handled separately (2)

* `exception_ranges` (from `PENDING_EXCEPTION_RANGES`) — replaced wholesale by
  `xform.exception_ranges`, which the rewriter produced.
* `protected_ranges` (from `PROTECTED_RANGES_REQUEST`) — **not** rewritten.
  `Compiler::pc_is_protected` translates its argument through
  `Compiler::orig_bci` instead, which is the right shape for a table of SPANS
  rather than of pc-keyed entries: every copy of a bytecode inside a `try` maps
  back to the one bci the range covers.

  This entry used to say the ranges did not need translating because their only
  consumer is on the `precise_exception_frames` paths, which the transform
  refused. That was wrong twice: there are three consumers, and the two in the
  sibling-tail-call admission (`bytecode_walk.rs`) are not gated on
  `precise_exception_frames` at all — so the census was already describing a
  latent wrong answer, before `loop-02` made those paths reachable.

## Whole-compile refusals

`plan_bytecode_loop_xform` returns `LoopRewriteRefusal` and the compile
continues with the caller's original bytecode. Refusing costs speed and
nothing else, which is why it is a refusal rather than a `BailoutReason`:
bailing would abandon a method the backend can compile perfectly well.

| Refusal | Why |
|---|---|
| `NotArmed` | the default |
| `InlineSitesPresent` | inlined-callee bci space |
| `NoCandidateLoop` | nothing passed the band + `bypassable_headers` |
| `Planner(_)` | one of `plan_loop_unroll`'s 17 structural refusals |
| `ProvenanceNotTotal` | unreachable; re-checked because `orig_bci` rests on it |

`DeoptRealEnabled`, `PreciseExceptionFrames` and `InvokedynamicPresent` were
here too. All three named one thing — a path that hands the VM an emitter pc as
a resume bci — and all three went when
`build_and_record_deopt_point` started publishing `DeoptimizationPoint::bci`
through `Compiler::orig_bci`.

There is one more refusal, later and louder: if a transform is in effect,
`compile_with_param_slots` puts every recorded deopt point through
`rewritten_deopt_points_are_publishable` and **discards the method** (returns
`None`) if the translation did not hold, if a point landed on the versioning
guard's synthetic bytes, or if two copies of one bytecode disagree about a field
a bci-keyed consumer takes on trust. This used to be "any deopt point at all
discards the method", which was cheap while the vector was provably empty and is
not an option now that it is the normal case.

## What executing it found

`CRATONVM_JIT='bytecode-loop-xform,deopt-real=0'` is the first configuration in
which any of this runs. The first workload put through it —
`bench/StringRegexOnly.java` — threw `NullPointerException` at `sb.append(i)`,
deterministically, from n = 20,000 up.

The bisect (`apps/probes/LoopVersionOsrProbe.java` is the reproducer, and its comment
is the argument) needed three things at once:

* a **versioned** artifact — the same loop with an unprovable trip count gets an
  unversioned unroll and was always correct;
* entry through the **OSR** door — the same versioned artifact entered by
  invocation count was correct;
* a **second loop** in the method, so the OSR request lands on a header whose
  compiled state matters.

Cause: `LoopXform::osr_entry_pc` answered the loop header's OSR entry with the
pre-header **guard**, on the reasoning that re-evaluating it there is exactly
what a fall-through entry does. That is true of the bytecode and false of the
machine code. An OSR entry is only valid at a pc whose compiled state the entry
trampoline can reconstruct from the interpreter frame, and the emitter publishes
that state at loop headers — the `[osr-meta] published_entries` list is exactly
that set, and the guard's pc is not in it. The guard sits in the method's
prologue, where a local can still live in a register the trampoline does not
seed, so the loop ran with a null receiver.

Fix: every bci in the region, header included, enters the fallback copy — which
*is* a loop header. The cost is that an OSR-entered method runs the untouched
loop rather than the transformed one; only entering costs that, not calling.

This is the case for the flag existing. Every unit test passed throughout.

## What IS validated

* Both compile doors, on real Java: the invocation-count door and the OSR door,
  each producing a versioned artifact whose answers match HotSpot's exactly
  (`apps/probes/LoopXformProbe.java`, `apps/probes/LoopVersionOsrProbe.java`,
  `apps/probes/IndyDeoptProbe.java`, `bench/StringRegexOnly.java`,
  `bench/HashMapOnly.java`) — in all three configurations: default,
  `bytecode-loop-xform`, and `bytecode-loop-xform,deopt-real=0`.
* An application suite, armed and under the DEFAULT `deopt_real`:
  `core/spring-boot-autoconfigure`'s `AutoConfigurationSorterTests`,
  `ConditionalOnClassTests` and `ConditionalOnPropertyTests` — 61 tests, 0
  failures, ~83 methods rewritten between them, three runs each, and zero
  discards in all nine
  (`loop_xform_deopt_bci_unpublishable = loop_xform_deopt_frames_diverge = 0`).
* `cargo test -p cratonvm-jit --test '*'` with the rewriter armed and
  `deopt_real` off: 196 tests, all passing, with the transform firing during the
  run (confirmed with `CRATONVM_DBG=jit-gen`, not assumed — a vacuous pass here
  would look identical).
* `cargo test -p cratonvm-vm --lib` armed: no regression against the unarmed
  run.

## What is NOT validated

* No sanitizer or GC-stress configuration has run with the rewriter armed, and
  neither has Tomcat or H2. The Spring Boot classes above are the only
  multi-threaded, allocating workload it has seen.
* No benchmark A/B. Wall-clock on the shared Azure host spreads far too wide to
  read a transform this size, and the only honest meter there is callgrind.
* Nothing has exercised a rewritten method through a real GC pause or a real
  deopt. A real OSR entry is now covered, and that is where the one bug was.
* Inline-cache sharing across copies is *argued*, not measured. If it is ever
  wrong, the symptom is a megamorphic slot where the native unroller would have
  had `k+1` monomorphic ones — slower, not incorrect.
* No peel and no versioned method has been compiled by the VM, because no
  compile reaches the planner (see "Reachability"). Both are compiled by tests
  — `a_versioned_artifact_publishes_its_osr_entries_inside_the_fallback_copy`
  puts the planner's rewritten bytes through the real emitter and checks the
  published OSR metadata — but that is a unit test, not a workload.
* Versioning's *profitability* claim is unmeasured and unmeasurable today: it
  trades one body of code for not entering a duplicated body when the loop may
  not run. Nothing has A/B'd it because nothing runs it.

## Follow-ups

1. `#[derive(Clone)]` on `JitDirectCall` in `jit/src/lib.rs`, which would
   delete the packed-tuple detour.
2. ~~Translating `DeoptimizationPoint::bci` at `build_and_record_deopt_point`~~
   — done, and it retired `DeoptRealEnabled`, `PreciseExceptionFrames` and
   `InvokedynamicPresent` together, as predicted. What it did NOT predict:
   `pc_is_protected` was already asking its question in the wrong space, the
   versioning guard's bytes had to stop being OSR-eligible, and the copies of
   one bytecode legitimately disagree about a slot's oop-ness. See
   the loop-planner admission-gate design.
3. ~~`loop-transform-wiring.md` still describes the pre-wiring state~~ — it now
   carries a superseded banner pointing here.
4. Versioning emits ONE guard. A transform needing a guard *set* (every
   `VecPlan::guards` entry, say) needs `plan_loop_version` to take a slice and
   chain the failing edges to one fallback label. The refusal
   `GuardNotEncodable` on `StrideInRange` is the case that wants it first.
