# IR-tier inlining

`CRATONVM_JIT_IR_INLINE`. **Default ON since 2026-09-06**; `=0` is the kill
switch and restores the pre-flip behaviour (no callee body is spliced). See the
doc on `ir_inline_enabled` in `jit/src/lib.rs`. Sections below written while the
flag was opt-in may still describe it that way.

## What it is for

`IrBuilder::build` walks one method's bytecode. So an accessor that returns a
freshly-allocated wrapper always shows escape analysis an object leaving through
`areturn`, and `scalar-replaced 0/N` is the only answer per-method EA can give —
at any tier, however good it is. That is the whole of
`docs/known-issues/jit/per-voxel-allocation-escapes-its-method-so-ea-cannot-help-20260827.md`:
kfusion's per-voxel `Short2` was 94% of the cost of a voxel read, and no gate fix
moved it, because the blocker was not a gate.

HotSpot deletes the same object by inlining the accessor chain into its consuming
loop first and running EA on the merged graph. This is that missing half. Escape
analysis is the reason it exists; it is not yet the reason it pays (see
[Measured](#measured)).

## How the splice works

Bytecode-domain, not a second walker.

`lib.rs` hands `build` a **combined buffer**: the compiling method's code, then
one copy of each admitted callee body appended after it. Every pc-keyed side
table the builder consumes (`field_info`, `invoke_info`, `new_info`,
`object_init_pcs`) gets the callee's rows rebased into that buffer's
coordinates. `build` walks the caller exactly as before and, at an admitted
`invoke`, installs the callee's locals and jumps `pc` into the body; the
callee's return restores the caller's frame, pushes the result and resumes at
`pc + instr_len`.

The ~2000-line opcode `match` is reused verbatim. That is the point: a second
walker would be a second model of every opcode, and the two would drift.

Relocation is free because JVM branch offsets are relative to the branching
instruction — a whole body moves without rewriting. `tableswitch` /
`lookupswitch` are the exception (their padding is measured from method start)
and are refused, along with everything else in [Admission](#admission).

## Two bcis

A node's `bytecode_pc` is read for two unrelated purposes, and a spliced node
needs a different answer for each. Getting this wrong is not a missed
optimisation; it was measured as both a wrong-code hazard and a ~230x
slowdown, so it is worth stating plainly.

**As a site key.** `ir_lower` looks up the compact field offset, the direct-call
entry and the MIC/PIC pair by `bytecode_pc`. The answer has to be unique per
site, so a spliced node keeps its **combined-buffer pc** and `lib.rs` rebases
the callee's rows to match — including a fresh MIC/PIC for every virtual call a
spliced body leaves behind.

An earlier draft gave every node in a region the caller's `invoke` pc instead.
Consequences, both real:

* the region shares the metadata of the call the splice **replaced**, so an
  inline-cache hit on a matching receiver class jumps to the wrong method;
* with no compact-offset row every spliced field read falls back to the checked
  `jit_getfield` helper, and with no cache every surviving call falls back to a
  blind name resolution. On the `VoxelAlloc2` probe that took the control arm
  from 47 ns to 11 000 ns per element.

**As an interpreter program point.** The throw-site bci the exception table's
`[start_pc, end_pc)` check uses (`jit_set_throw_bci` →
`route_jit_exception_through_method`), and the bci a null/bounds guard's deopt
point resumes at. A combined-buffer pc names no instruction in this method, so
`Lowerer::resume_bci` maps it back to the enclosing `invoke` through
`spliced_ranges`. "The `invoke` at this bci has not taken effect; re-execute it"
is true of the whole region and needs no frame identity.

That last clause is why there is **no `InlineScopeTable` entry**. A real
inlined-scope chain needs the innermost frame's own method key, and
`ir_lower::resolve_frame_state` deliberately leaves that for the VM to fill from
the running `CompiledMethod` — i.e. it would name the caller. Re-execution needs
no identity at all. No safepoint snapshot is pushed inside a splice, so a
combined-buffer pc never reaches `build_deopt_points`.

## What re-execution requires, and how it is proved

Re-executing the `invoke` re-runs everything the spliced prefix did. That is
harmless for a write whose target is an object the splice itself allocated: the
re-run allocates a fresh one and writes that instead, and the first is
unreachable garbage.

`IrBuilder` proves exactly that, with a reachability set rather than an alias
analysis (`splice_store_is_local`):

* an `Op::New` / `Op::NewArray` built inside the splice is local;
* a `Load` whose base is local is local — a field of an object this splice
  allocated holds either `null` or something this splice stored there — **unless**
  what the splice stored there was itself non-local, in which case the base is
  tainted and locality stops. Without that second half the rule is wrong in an
  obvious way: `new W(); w.f = callerObject; w.f.x = 1` would read `w.f` back as
  local and admit a write straight into the caller's object.

A store that fails the test bails the compile. The rule admits `Short2.setX` →
`Short2.set` → `storage[0] = v` (`storage` is read from a `Short2` the splice
allocated three levels up) and correctly refuses `VolumeShort2.set`, whose
`storage` is read from the receiver the caller passed in.

The one thing it does not cover is a CALL the spliced body makes: the builder
cannot see whether it writes. See [Open](#open).

## Admission

`resolve_ir_inline_site` in `vm/src/runtime/interpreter/jit_bridge.rs`. Wider
than the single-pass resolver in what it admits and narrower in what it demands.

**Wider.** `new`, array load/store and `arraylength` are refused for the
single-pass emitter because `try_emit_inline_body` has no arm for them — an
emitter limit, not a resolution one. `IrBuilder` lowers all three, and `new` is
the entire point.

**Narrower.**

| refused | why |
|---|---|
| more than one `return`, or a `return` that is not last | the walk goes straight through from the body's first byte to its return; merging several returns into one continuation is a later increment |
| `idiv`/`irem`/`ldiv`/`lrem` | the div-zero guard is the only guard the builder emits, and a spliced region should carry none of its own |
| `ldc` / `ldc2_w` | `InlineSite` records a raw `i64` where the builder needs the value **and** its float/double discriminator; inventing that bit is how a `long` constant becomes a `double` |
| `getstatic` / `putstatic` | no static-field rows are rebased, and `putstatic` is a side effect re-execution cannot undo |
| a target that is not provably monomorphic | true of v1 (no class-id guard node). Later rounds added profile-guarded splices, which guard the receiver's class and keep the call on a miss; see [Profile-guarded splices after round 20](#profile-guarded-splices-after-round-20) |

### Branches — `ir-splice-branch`

Default ON since 2026-09-09; `CRATONVM_JIT_IR_SPLICE_BRANCH=0` restores the
straight-line-only admission v1 shipped with.

The obstacle was never the branch. It was that `verified_code` above analysed
`code[..code_len]` — the CALLER alone — so a relocated body's merge targets and
loop headers were in nobody's decode. Closing that needs no new analysis,
because a callee body IS a valid method body: `IrBuilder::build` runs the same
verifier over each spliced body and rebases what comes out by the body's
`base`. Branch offsets need no rebasing at all — they are relative, the body is
copied contiguously, so a branch inside it lands inside it in combined
coordinates by construction.

The merge-activation gate in the walk is therefore no longer
`self.splice.is_empty()`. A join inside a spliced region is activated exactly
as one in the caller is, and the φ it builds carries a combined-buffer pc,
which is what every other node in a relocated region already carries.

Pairing that with the OSR door surfaced
`ir-osr-entry-miscompiles-a-spliced-merge-FIXED-20260909`. The page is worth
reading for what it corrected rather than for what it found: an entry stub
jumps past the block that writes a CONSTANT's home word, and a merge store
reads its arms from their home words — a defect with no splice in it at all,
which a spliced clamp is merely how anyone first saw.

"Provably monomorphic" is `static`, `private`, `final`, `<init>`, or a `final`
declaring class. Deliberately **not** a CHA-style "no loaded subclass overrides
it": that is true until the next class loads, so it needs an invalidation
dependency this path does not record. `final` is monotone.

Nesting is bounded by `MAX_IR_INLINE_NEST_DEPTH` (6), deeper than the
single-pass `MAX_INLINE_NEST_DEPTH` (3). A partly-inlined chain is worth
nothing — the moment the object reaches a call the graph cannot see through it
escapes, and EA reports `0/N` exactly as before. kfusion's chain is four levels
(`VolumeShort2.get` → `loadFromArray` → `Short2.<init>()V` →
`Short2.<init>([S)V`), so a budget of three would have measured as no change.

Bounded by `IR_INLINE_MAX_TOTAL_BYTES` (4 × `MAX_INLINE_BYTECODE_SIZE`) and
`IR_INLINE_MAX_SITES` (24). The byte budget is not a code-size budget: every
appended byte is bytecode the builder turns into nodes, and the node count is
what `ir_optimize`'s GVN and the escape-analysis connection graph scale in.

### Profile-guarded splices after round 20

The rules below are the state on 2026-10-12 (JIT round 20, lane splice;
record `docs/internal/jit-proposals/r20-defect-round-SUMMARY-RECORD-20261012.md`).

**Package-private overrides.** `receiver_resolution_is_dispatch_faithful`
(`vm/src/runtime/interpreter/jit_bridge.rs`) used to refuse a package-private
method the receiver walk found below the constant-pool class
(`first-body-refused`). It now asks JVMS 5.4.6 selection from the receiver
against the method the constant-pool class resolves to, which applies the
5.4.5 runtime-package rule (same defining loader and package); the body is a
guarded splice target only when selection picks that same declaration. Any
other answer keeps the refusal. `CRATONVM_JIT_GUARDED_SPLICE_PACKAGE_PRIVATE`,
default ON since round 20 (`=0` kills): `R16MegaOneSite` ent-virt / osr-virt
5.9 -> 1.9-2.1 ns, `R20SpliceNestedHot` pkg 11 -> 7 ns, `R19Mega5Nested`
late-class-virt 6 -> 2 ns.

**The hot cap one level down.** A top-level guarded site in a hot position
takes the 325-byte cap (`MAX_INLINE_BYTECODE_SIZE`, HotSpot's
`FreqInlineSize`) instead of 35 (`MAX_INLINE_SIZE_COLD`). A nested guarded row
(the resolver's `nest-profiled` / `nest-profiled-iface`) was held to 35 however
hot. `jit/src/lib.rs::ir_nested_guarded_body_cap` now gives each of its bodies
an equal share of the hot cap when the site is on the enclosing body's entry
prefix (`ir_splice_pc_in_entry_prefix`) or its row has at least
`GUARDED_SPLICE_HOT_MIN_CALLS` (256) samples, and the planner drops such rows
again under a top-level site it does not call hot
(`ir_splice_strip_cold_nested_hot_rows`). The VM resolver and the planner ask
the same function of the same row, so they agree.
`CRATONVM_JIT_IR_NESTED_GUARDED_HOT_CAP`, default ON: `R20SpliceNestedHot`
mono 12 -> 7 ns.

**Leaf bodies only over 35 bytes.** Unrestricted, the nested hot cap cost
`R16AllocPremium hashmap-cached` about 10 % (11.4 -> 12.7 ns): it spliced the
profiled `hashCode` / `equals` bodies inside `HashMap.hash` / `getNode` /
`putVal`, whose code and class test cost more than the call. A body over 35
bytes now takes the nested hot cap only when it is a leaf: no call, no nested
body, no loop in its tree (`ir_nested_hot_body_is_leaf`,
`ir_nested_hot_body_admitted`; `CRATONVM_JIT_IR_NESTED_GUARDED_HOT_CAP_LEAF`,
default ON).

**The profile over the compiled cache.** A nested row whose callee profile has
at least 32 samples over two or more classes with none dominant is not
overruled by the compiled artifact's inline cache, whose MIC holds whichever
class it installed first (`CRATONVM_JIT_IR_NESTED_PROFILE_OVER_CACHE`, default
ON).

Known trade-off, unexplained: `R19Mega5Nested` inherited-static-mega-call 10
-> 13 ns (one extra nested body in a six-kernel static dispatcher; not budget
exhaustion).

### Admission after round 20 waves 7-12

Round 20's later waves (record sections 6-9) widened and narrowed what the
resolver and planner admit. All switches below are default ON with `=0` as the
kill. Numbers are the host build `w11d` A/B (5 interleaved reps, medians,
default vs `=0`, HotSpot 25.0.4 in brackets); the wave-12 rows are built and
await their host A/B.

**Widened (wave 11, lane pSplice).**

* A variable divisor is admitted in every spliced body
  (`CRATONVM_JIT_IR_PSPLICE_VARIABLE_DIVISOR`; `CRATONVM_DBG_JITC` prints a
  `variable-divisor` line). `R20PSpliceDivVar` divvar-long 2 vs 7 ns [3],
  divvar-store 2 vs 7 [1]. The zero-divisor shapes of `Nio21DivSplice` were
  refused before (SPLICE20-6).
* `ir_splice_body_is_receiver_forwarder` also takes a five-byte
  `invokeinterface` on `this`, resolved for the enclosing body's exact receiver
  (`CRATONVM_JIT_IR_PSPLICE_IFACE_FORWARDER`): `R20PSpliceForwarder` fwd-iface
  2 vs 6 ns [1].
* A nest-exact row on a hot pc of its enclosing body takes the nested hot cap
  when its body is loop-free (`CRATONVM_JIT_IR_PSPLICE_NESTED_EXACT_HOT`):
  neutral on its rows (nested-exact 12 vs 12 ns [2]).
* `ir_splice_order::profile_plan_order` ranks in-loop sites by execution count
  before depth when at least two exist and every one has a count
  (`CRATONVM_JIT_IR_PSPLICE_LOOP_COUNT_RANK`): looprank 27 vs 28 ns [21],
  neutral. `CRATONVM_DBG_JITC` prints an `ir-splice-bytes` line per planned
  site.
* A callee whose `VarHandle` primitive read / write / compareAndSet the IR
  post-build bind loop binds to the same thin helper is no longer refused
  (`CRATONVM_JIT_SPLICE_VH_IR_BOUND`, `varhandle_ir_bound_family`):
  `R20PIndyVhSplice` clq-offer-poll 150 vs 177 ns [32], cf-complete 406 vs 426
  [24].

**`invokedynamic` inside a compile (wave 11 pIndy, wave 12 qLamNew).**

* A non-constant `MutableCallSite` / `VolatileCallSite` site is a guarded site:
  the bridge publishes an immutable `IndyMutableGuardCell` (the call site and
  the bound target as never-released global references, at most 8 per site),
  and the IR builds `IndyRootRef(cs).target == IndyRootRef(planned) ? Call :
  IndyBridge` with the new pinned op `Op::IndyRootRef`
  (`CRATONVM_JIT_INDY_MUTABLE_GUARD`). `R20PIndyMutableSite` mcs-stable 254 vs
  361 ns [0], virt-stable 274 vs 367 [0]; a target flip costs 17.8 us either
  way [24 ns].
* An own non-capturing lambda site with a published singleton is built as
  `w = IndyRootRef(slot); w != null ? w : IndyBridge`
  (`CRATONVM_JIT_IR_INDY_LAMBDA_WORD`): lambda-word 19 vs 16 ns [0], a small
  loss to re-measure.
* A linked capturing site whose proxy is VM-registered is built as
  `Op::New(proxy)` plus one `Op::Store` per capture at the indy pc
  (`CRATONVM_JIT_IR_INDY_CAPTURE_NEW`, `jit_indy_capture_alloc_class`, probe
  `R20QLamNewCapture`), so escape analysis can remove a lambda that does not
  escape. This is the allocation half of SPLICE20-1.

**Narrowed (correctness).**

* A callee that calls the `AtomicInteger` / `AtomicLong` intrinsic family is not
  spliced: spliced, `getAndIncrement` kept the JDK body's CAS retry loop instead
  of one locked instruction (netty `FastThreadLocal` construction 135-142 ns
  against 8-9 ns unspliced; commit `2863eb6834`).
* The optimizing tier keeps `Unsafe.getAndAdd*` / `getAndSet*` /
  `getAndBitwise*` as a call wherever the RMW prefix serves it
  (`CRATONVM_JIT_UNSAFE_RMW_BODY_NO_SPLICE`, refusal
  `unsafe-rmw-body-keeps-its-inline-prefix`): the splice left a volatile load
  plus a `LOCK CMPXCHG` loop where the prefix is one `LOCK XADD` / `XCHG`.
  About 5 % on `R20PAtomRmwNoSplice` (ifu-get-and-set 149 vs 158 ns [5]).
* An invoke site that JVMS 6.5 makes a linkage error (`invokevirtual` of a static
  method, `invokestatic` of an instance one, `invokeinterface` of a class) is
  refused by `CpResolvers::invoke` (`invoke-site-linkage-error`) and the
  resolver refuses to splice a body holding one, so the interpreter raises the
  `IncompatibleClassChangeError` (`R20HuntDispLink`). Only broken bytecode
  reaches it.
* A finalizable or `java.lang.ref` allocation is never scalar-replaced, also
  when it arrives inside a spliced constructor chain (wave 12 hFin; see
  [`escape-analysis.md`](escape-analysis.md) section 11). The spliced-`new` row
  carries no finalizer bit, so with `CRATONVM_JIT_IR_SPLICE_REFUSES_FINALIZABLE_NEW=0`
  that path is unscreened (open page
  `jr20-spliced-new-of-a-finalizable-class-is-unscreened-with-the-refusal-switch-off-FIXED-20261012.md`).

**Single-pass splice walker** (wave 7, lane huntSP; unconditional): its `iinc`
was a 64-bit `ADD` with no `MOVSXD`, so a wrapping increment kept the 33-bit sum
(`R20HuntSPSpliceIinc`); its `pop2` popped two model entries for one long /
double value and underflowed the caller's operand, refusing the whole compile
(`R20HuntSPSplicePop2`). Both are fixed.

### Admission after round 20 waves 17-19

Record section 13. Every switch is default ON with `=0` as the kill; none has
its host A/B yet (`w17s`), so no number below is a verdict.

**Widened.**

* `Reference.get()` splices (lane xLib, THROW20-4;
  `CRATONVM_JIT_IR_REFERENT_SPLICE`). The resolver refused every body with a
  `Reference.referent` getfield (`splice-reference-referent-load`). Now the pc
  keeps its field row and is listed in `InlineSite::referent_load_pcs`, rebased
  into `Graph::referent_load_pcs`; `ir_lower` declines every inline arm there, so
  the load takes `jit_getfield`'s slot-0 arm with the SATB keep-alive, and the
  field-forwarding passes never see it (no declaring-class identity). Probe
  `R20XLibRows`. The single-pass inliner still refuses.
* A String / Class `ldc` of the callee's pool (lane zLdc;
  `CRATONVM_JIT_IR_SPLICE_REF_LDC`). Admitted only when
  `compiled_ldc_slot_for` names a resolved constant held by a GC-maintained root
  (no resolution side effect moves); `append_ir_inline_site` rebases
  `InlineSite::ir_ldc_ref_rows` into `IrInlineTables::ldc_ref_*`, and the
  spliced `ldc` builds `Op::ConstString` / `Op::ConstClass` with the callee's
  (holder, cp index). In the same change, `x.getClass() == C.class` inside a
  splice folds to a constant when every definition of `x` is a `new X` and `C`
  reports only itself (`InlineSite::ir_getclass_compares`,
  `IrBuilder::try_fold_spliced_getclass_of_fresh`; `CRATONVM_DBG_JITC` prints
  `splice-getclass-fresh-fold`). `Date.compareTo` / `after` / `before` keep no
  call of `getMillisOf`. Probe `R20ZLdcRows`. Optimizing tier only.
* An unresolved `new` behind a branch edge the callee's profile never ran
  (lane ySplice, SPLICE20-2; `CRATONVM_JIT_IR_SPLICE_UNRESOLVED_NEW_EXIT`): an
  `ir_new_info` row with class id 0, its `<init>` in
  `unloaded_static_call_pcs`; the builder plants
  `IrBuilder::plant_unloaded_new_exit`, a chain-snapshot precise exit with
  `DeoptReason::ClassLoading`, and the interpreter loads and initialises the
  class. `CRATONVM_DBG_JITC`: `splice-unresolved-new-exit ...`.
* A linked capturing lambda site inside a spliced body (SPLICE20-1;
  `CRATONVM_JIT_IR_SPLICE_INDY_CAPTURE`): `InlineSite::ir_indy_capture_rows` ->
  `IrInlineTables::indy_capture_news` -> `try_indy_capture_new`, the wave-12
  `Op::New` plus stores. Not admitted after a committed store.
* The single-pass resolver admits constant-word lambda `invokedynamic` rows
  (SPLICE20-7; `CRATONVM_JIT_SP_SPLICE_INDY_CONSTANT`); `x64::inlining` emits the
  constant-word probe with the bridge as the cold miss. Probe `R20YSpliceRows`.
* The native-shadow screens of `resolve_inline_site_from` ask the dispatch
  arbitration's own predicate (`inline_screen_native_shadows`, lane yHttp,
  WALKCOMPAT20-6; `CRATONVM_JIT_INLINE_SCREEN_YIELDED_STUB`): an allow-listed
  `SyntheticStub` whose loaded JDK body wins does not run, so it no longer
  refuses the splice. Effective under `--compatible` only (`--jdk-only`
  registers no stubs): `ArrayList.get/size/add`, `Objects.requireNonNull`,
  `ReentrantLock.lock/unlock` splice there as they do under `--jdk-only`.
* `String.codePointAt(I)I` joins the reviewed splice-the-JDK-body rows (lane
  pStr3; `CRATONVM_JIT_SPLICE_INTRINSIC_JDK_BODY_CODE_POINT`).

**Kept a call on purpose.** The new IR prefix kinds stay calls in the splice
planner, so the lowering's prefix serves them: `StringCoding.countPositives` /
`implEncodeAsciiArray`, `StringBuilder.append(int)` / `toString()` /
`appendCodePoint(int)`, `Character.toChars(int)`. `StringBuilder.charAt`
also stays a call (`CRATONVM_JIT_IR_SB_CHARAT_STAYS_CALL`, STR20-1): spliced,
its `checkIndex` / coder test / `StringUTF16.charAt` was the only difference
between the builder-prefix body and the single-pass body in
`Nio20StringIr sbAppendLoop`.

### Admission and frames after round 21 (2026-10-18 / 2026-10-19)

Lane splice (probe `tools/probes/jit/r21/R21SpliceRows.java`) and the review
lanes of waves 8-23. The default-ON rows are correctness fixes with `=0` as the
kill; the opt-in rows are pending the round's A/B, and their status is kept in
the `r21-flip-status` table of `docs/JIT_OPTIMIZATION.md`. Waves 16-23 are
committed unbuilt.

**Fixed, default ON.**

* The planned chain never opens more splice frames than the builder's
  `MAX_IR_SPLICE_DEPTH`: `CRATONVM_JIT_IR_INLINE_MAX_NEST` is clamped to it
  (`CRATONVM_JIT_IR_INLINE_MAX_NEST_FRAME_CLAMP`). A deeper chain refused the
  whole build.
* A constant-pool splice whose selected body is declared above the pool's class
  (an inherited default reached through `invokevirtual K3.d1`) names the
  declaring class for its frame label, class id and dependency, so the trace
  reads `A.d1:110`, not `K3.d1:-1` (`CRATONVM_JIT_SPLICE_DECLARING_CLASS_FRAME`).
  It also records a replacement dependency on that class, so a define that
  replaces it by name withdraws the callers
  (`CRATONVM_JIT_SPLICE_DECLARING_CLASS_REPLACE_DEP`), and an inherited splice
  keeps the new-subtype dependency on the site's class.
* A spliced back-edge mode exit reads the header's chain snapshot at the latch,
  and the keep set and colouring now count that read
  (`CRATONVM_JIT_IR_SPLICED_LATCH_EXIT_READS`, with
  `CRATONVM_JIT_IR_KEEP_SET_MODELLED_CHAIN_READS` for the retention half); a
  callee local only that snapshot names was zeroed in the poll's slow path
  right before the exit resumed with it.
* Finalizable classes: a constructor that can throw before `super(..)` /
  `this(..)` is never spliced, for the class and for the ancestors a loaded
  finalizable subclass reaches; the define of a finalizable class withdraws
  those ancestors' splices (see "Status after JIT round 21" in
  `docs/JIT_OPTIMIZATION.md`).
* A hidden class's self calls bind the class by id
  (`CRATONVM_JIT_HIDDEN_SELF_BY_ID`, `CRATONVM_HIDDEN_SELF_CP_BY_ID`), never a
  same-named ordinary class.

**Opt-in, pending A/B.**

* A guarded splice of a package-private override whose JVMS selection answers
  Lenient is decided on the receiver's superclass chain
  (`CRATONVM_JIT_GUARDED_SPLICE_PP_CLASS_CHAIN`).
* An unprofiled own virtual / interface site is guarded on the class-hierarchy
  single implementor, with the call as the miss edge
  (`CRATONVM_JIT_IR_CHA_GUARDED_SPLICE`).
* A never-run private instance call in a splice is a precise exit, not a
  refusal (`CRATONVM_JIT_IR_SPLICE_NEVER_RUN_PRIVATE_EXIT`).
* A volatile `putfield` stays in the splice as a fenced ordered `Op::Store`
  instead of refusing the body (`CRATONVM_JIT_IR_SPLICE_VOLATILE_STORE`,
  `CompletableFuture.uniApplyNow`'s `d.result =`); the widened callee-handler
  splice admits a `Throwable` / `Error` catch whose handler reads pre-try locals
  (`CRATONVM_JIT_IR_SPLICE_CALLEE_HANDLERS_WIDE`, layered on
  `CRATONVM_JIT_IR_SPLICE_CALLEE_HANDLERS`).
* The self-copy predicate compares the nested static call's resolved declaring
  class id with the site's class id instead of the name
  (`CRATONVM_JIT_IR_SELF_COPY_BY_ID`; the flip review notes its resolution walk
  is a dispatch walk, not JVMS 5.4.3.3).

## Measured

`probes`-shaped scratch `VoxelAlloc2.java`: 64³ voxels, each arm's loop in its
own method (a loop in `main` is OSR-only — see [Open](#open)), warmed on a small
volume so compilation is not inside the timed window. Three arms with the same
checksum by construction, so a transform that breaks the read shows up as a
wrong number rather than as a fast one. Steady state (reps 2-5), one binary,
flag A/B:

| arm | inlining OFF | inlining ON | HotSpot |
|---|---|---|---|
| `volume` — `VolumeShort2.get`, i.e. two segment reads plus the `Short2` pair | 2.1-2.7 µs | **171-313 ns** | 1.3-1.4 ns |
| `rawseg` — the same two reads, no wrapper (the CONTROL) | 39-47 ns | **24-38 ns** | 1.1 ns |
| `array` — a plain `short[]` (the floor) | 2.2-3.7 ns | 2.1-2.9 ns | 0.4 ns |

Checksum `8589672448` in every row of every arm.

**~10x on the target arm, and the allocation is still there**: the same run
reports `scalar-replaced 0/2 alloc(s)` for the spliced method. The win is
removing five dispatch frames per voxel and giving the spliced bodies compact
field offsets and inline caches — not deleting the object. See [Open](#open) for
what deleting it still needs.

Ranges, not points, because the `volume` arm is allocation-dominated and
therefore GC-noisy: across runs the OFF arm sat between 1.9 and 2.7 µs and the
ON arm between 171 and 313 ns. The OFF arm also DEGRADES rep over rep (past 3 µs
by rep 5, and the allocation-free `array` arm degrades with it, which is what
says the cause is heap pressure and not the arm). The ON arm does not degrade —
itself a signal, and the reason the honest comparison is steady-state and not
first-rep.

## Open

1. ~~**Escape analysis still reports `0/2`.**~~ **CLOSED.** Both
   allocations are deleted and the `volume` arm has converged onto its own
   control (24.9 ns/voxel against `rawseg`'s 23.8). It took four things, none of
   which was the one this note guessed at — a per-allocation dominance proof
   from the basic block the IR already knew, array scalar replacement, an
   escape-analysis fixed point (replacing the wrapper is what frees its storage
   array), and a deopt descriptor that can spell an array and a nested object.
   Requires `CRATONVM_SCALAR_DEOPT=1` alongside this flag; see
   `per-voxel-allocation-escapes-its-method-so-ea-cannot-help-FIXED-20260827.md`,
   which is also where the "why is it still opt-in" argument lives.

2. **A call left inside a spliced body is re-executed on a deopt.** The store
   rule proves nothing about it. The bodies this admits are accessors, but that
   is an observation about today's callers, not a proof.

3. **An OSR-only hot loop gets nothing.** `VoxelAlloc`'s loops live in `main`,
   which never reaches the IR admission gate at all, and that probe measures
   787-914 ns with the flag in either position. The win requires the hot loop to
   be in a method the optimizing tier compiles.

4. **Priced on one probe.** The flag is opt-in until the gauntlet has an
   opinion. The trades it makes — more nodes per compile, a spliced body's
   surviving calls losing their profile seed, compile time — are all real and
   none of them are measured beyond this one workload.

5. **Unverified: the receiver of a spliced INSTANCE callee is not known to be
   null-checked at the splice.** JVMS 6.5 raises `NullPointerException` at the
   invoke, before the callee's first instruction, so a spliced body that never
   dereferences `this` (`return 3;`, `return helper(x);`) must still throw on a
   null receiver. Round 9 found the single-pass splice had no such check for
   `invokespecial` (top-level and nested; fixed by
   `Compiler::emit_spliced_receiver_null_check` in `x64/inlining.rs`). The IR
   builder's splice path was not audited for the same property: check that an
   instance-callee splice starts with a receiver null check unless the receiver
   is provably non-null (`<init>`'s receiver, or `this` of an instance caller).

## Instruments

```bash
CRATONVM_JIT_IR_INLINE=1 CRATONVM_DBG_IR_COMPILES=1 ...
```

* `[ir] inline-plan <method>: N site(s), M spliced bodies, B bytes appended` —
  what the planner admitted.
* `[ir] spliced M callee bodies into <method>` — what the builder actually
  walked. A plan with no matching `spliced` line means the build bailed.
* `[cratonvm-jitc] inline-resolve REFUSED <callee> depth=N: <reason>` (needs
  `CRATONVM_DBG_JITC=1`) — every refusal is named.
* `CRATONVM_DBG_SCALAR_NEW=1` → `scalar-replaced N/M alloc(s)`, which is the
  only number that says whether the point of the exercise was reached.
