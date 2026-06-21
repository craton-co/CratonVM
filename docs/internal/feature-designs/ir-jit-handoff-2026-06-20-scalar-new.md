# Handoff — IR/JIT field + `new` scalar-replacement frontier (2026-06-20, session 2)

> **UPDATE (session 3, increment 20): Gap A is CLOSED — and it was not just a
> flag flip.** The flip exposed a *latent bug*: `new` scalar replacement
> (inc 17–19) **never fired on real bytecode**. The IR builder had no `astore`
> handler, so every real-javac allocation (`new; dup; invokespecial; astore_N`)
> bailed to single-pass. Inc 19's "POJO probe == HotSpot" soak was **vacuous**
> (a non-escaping POJO yields the same result whether or not it is
> scalar-replaced). Fix: lower `astore`/`astore_0..3` in the builder + both
> length walkers; flip `CRATONVM_JIT_SCALAR_NEW` to **default-ON**
> (`=0` opt-out). Now `oneShot`/`sumPoints`/`sumBoxes` scalar-replace `1/1`
> (verified via a new `CRATONVM_DBG_SCALAR_NEW` live-fire diagnostic), bt10/14/16/18
> == HotSpot (default-on and opt-out), jit 803/803 + harness 21/21. See
> `activate-ir-optimizer.md` increment 20.

> **UPDATE (session 4, increments 21–23): Gap B is CLOSED too.** `Op::Call` for
> `invokestatic` landed (inc 21 int-only/oop-free; inc 22 lifted to oops-live-
> across-call via the conservative IR-frame GC scan) and is now **default-ON**
> (inc 23, `CRATONVM_JIT_IR_CALL=0` opt-out). Soak: gate-ON ≡ gate-OFF on
> bt10/14/16/18 == HotSpot, the `IrCall`/`IrCallGc`(+`DBG_GC_STRESS`)/
> `IrCallGcCatch` probes == HotSpot, and a ~20-program bench differential ==
> HotSpot; jit 804/804 + harness 25/25. Remaining frontier: `invokespecial`/
> virtual dispatch + category-2 (long/float/double) args. See
> `activate-ir-optimizer.md` increments 21–23.

Supersedes [`ir-jit-handoff-2026-06-20.md`](ir-jit-handoff-2026-06-20.md).
Authoritative per-increment detail: [`activate-ir-optimizer.md`](activate-ir-optimizer.md)
(increments 14–20). This is the session-level map + **the gap** (where to resume).

All work below is **merged to `dev`** (tip was `b03112ba` at handoff). The main
worktree `C:\craton\CratonVM` stays on `dev`; feature work happened in the sibling
worktree `C:\craton\CratonVM-embed`.

---

## 1. What this session delivered (all merged, all tested)

The IR optimizer's field/allocation frontier — "§4 the next frontier" of the
prior handoff — is now substantially built:

| Inc | What | Commit |
|-----|------|--------|
| 14 | int `getfield` → `Op::Load` (first production IR memory read) | `2c47f367` |
| 15 | int `putfield` → `Op::Store` (memory-token chain serialises RAW/WAR/WAW; DCE roots from stores) | `8a977fe9` |
| 16 | EA bridge translates the full-layout `Load`/`Store` (compact `[holder,value]` + real field index) | `fddf323f` |
| 17 | `Op::New` emission + scalar-replace mechanism + receiver-checked `<init>` elision + surviving-`New` gate | `c3fb6348` |
| 18 | `apply_ea_to_ir` materialises `Const(0)` for an un-stored scalar field (latent-bug fix) | `ba882122` |
| 19 | VM trivial-constructor signal wired (`CRATONVM_JIT_SCALAR_NEW`, **default-OFF**) | `72167d9f` |

**Soaked/validated**: bt18 == 68332206 (HotSpot) after inc 14/15 *and* with
`CRATONVM_JIT_SCALAR_NEW=1`; a non-escaping default-ctor POJO probe == HotSpot
(`735000000`) with the flag on; jit lib 802/802; field differential harness
20/20; `cratonvm-vm` builds clean.

Increments 14/15 are **on by default** (production now routes int field-read/write
methods through the optimizing IR path). Inc 16/17/18 are sound-but-inert
foundations. Inc 19 is wired but gated OFF.

---

## 2. THE GAP — where to resume (two items, in order)

### Gap A — flip `new` scalar replacement ON (the production-validation step)

Everything for scalar-replacing a non-escaping `new` is built and wired, but
**`CRATONVM_JIT_SCALAR_NEW` defaults OFF** because turning it on changes
production scalar replacement — the kafka-bug-25-sensitive area — and that
change must clear a soak first. **This is the only thing standing between the
work and a real throughput win on allocation-heavy code.**

To close it:
1. **Soak** with `CRATONVM_JIT_SCALAR_NEW=1` across the app gauntlet
   (kafka / spring / tomcat / hibernate suites) **and** bt10/14/16/18
   (must stay 135854 / 3222190 / 14985902 / 68332206). bintrees' own
   `TreeNode(left,right)` ctor is arg-bearing → NOT elidable, so bt18 only
   proves the flag-on path doesn't regress; it does not *exercise* scalar-new.
   The probe `scratch/scalarnew/ScalarNew.java` (gitignored) does exercise it —
   keep comparing its output to HotSpot, and add more non-escaping
   default-ctor-POJO shapes.
2. **Watch** for an object that escapes via an *elided* constructor body. The
   `is_elidable_construction` restriction (body == `aload_0; invokespecial
   java/lang/Object.<init>()V; return`) makes the elided body provably empty, so
   this class of bug is structurally excluded — the soak is the proof.
3. **Flip**: default the flag on (or delete it + the gate), re-run the gauntlet +
   bt checksums (step 8 of `activate-ir-optimizer.md`).
4. **Then refine** `is_elidable_construction` to recurse the super chain (admit
   non-`Object` supers whose `<init>` is itself elidable), widening coverage
   beyond direct-`Object`-subclass POJOs.

Why it's safe to resume here cold: the soundness rests on three landed,
unit-tested guards — the surviving-`New` gate (inc 17) bails any escaping `New`
to single-pass; the receiver-is-`New` check (inc 17) refuses to elide a `<init>`
on `this`/a param; the zero-default (inc 18) makes un-stored field loads read 0
(correct because the elided ctor writes no field).

### Gap B — `Op::Call` for real `invoke*` (the remaining big lever)

The IR builder still bails on every `invoke*` except an elided trivial `<init>`.
Emitting `Op::Call` is the last large lever: it lets `invoke`-bearing methods
take the IR path and de-latents the rest of the inc-4–9 optimizer suite on real
code. It needs:
- **Lowerer gains `JitRuntimeHelpers` access** — currently `ir_lower::lower` has
  no helper table; a `Call` lowers to `CALL invoke_dispatch` (or a direct call),
  which needs the helper pointers + the dispatch ABI from `x64.rs`. (Inc 15's
  putfield deliberately *inlined* its write to avoid this; `Op::Call` cannot.)
- **Memory ordering**: a `Call` is a hard memory barrier (consumes + produces the
  memory token, stronger than a `Store`).
- **VM-level differential validation**: the jit-crate stub harness cannot
  faithfully validate dispatch/allocation (its helpers panic). Per
  `wire-tiered-manager.md`, move differential validation to the VM level — run a
  real Java method twice via the `optimize` toggle and compare. (Or supply real
  dispatch stubs in the harness — feasible for a direct static call, hard for
  virtual dispatch.)

This is "L" effort and warrants its own session.

---

## 3. Gotchas a future session WILL hit (this session's hard-won ones)

- **Single-pass `putfield` methods are `needs_heap` → `needs_context`.** They take
  a hidden VM-context pointer as the first arg; invoke them via
  `try_call_with_context(dummy, [obj, …])`, not `try_call`, or every arg shifts by
  one (manifests as a `STATUS_ACCESS_VIOLATION` writing through `obj == value`).
  The inline IR store needs no context. The differential harness dispatches on
  `CompiledMethod::needs_context()`.
- **The EA bridge expects a COMPACT layout** (`Store [holder,value]`, `Load
  [holder]`, field-index payload) while production `Op::Load`/`Op::Store` are
  full-layout (`[ctrl,mem,base,offset,value]`, `MemKind`). Inc 16's
  `escape_analysis_from_ir` is the single place that translates — recover the
  field index from the `Const` offset operand (input[3]), not the `MemKind`.
- **`apply_ea_to_ir` zero-defaults un-stored fields** (inc 18). This is sound only
  for a zero-initialised object — which is exactly why `is_elidable_construction`
  requires the ctor to write no field. Do not relax one without the other.
- **`classify_init_complexity` is NOT a sound elision signal.** Its `Trivial`
  admits arbitrary calls (`register(this)` escapes the receiver). The sound check
  is the stricter `is_elidable_construction` (inc 19).
- **Synthetic test bytecode must carry the 2-byte `0x00 0x00` trailer** — the VM
  pads bytecode and `try_compile` strips `code.len()-2`; without it the last two
  opcodes (often the `ret`) are dropped.
- **Shared-checkout merges**: the main worktree carries the orchestrator's
  uncommitted doc-triage WIP; a *staged* deletion (`D ` in col 1) blocks a merge.
  `git restore --staged <file>` converts it to an unstaged deletion (intent
  preserved) and unblocks the merge — done at every merge this session.

---

## 4. How to verify the current state

```
cargo test -p cratonvm-jit --lib                      # 802 pass
cargo test -p cratonvm-jit --test ir_vs_singlepass    # 20 pass (differential)
cargo build -p cratonvm-vm                             # builds clean
# bt18 (release): == 68332206, and with CRATONVM_JIT_SCALAR_NEW=1 still == 68332206
# probe (release): CRATONVM_JIT_SCALAR_NEW=1 … -cp scratch/scalarnew ScalarNew == 735000000
```

Pre-existing UNRELATED failure (not from this work, documented in the prior
handoff): `cargo test -p cratonvm-jit --test intrinsic_arrays_ops`
(`test_arrays_fill_null_array_deopts`) — array null-deopt code, untouched here.

---

## 5. One-line status

Field read/write on the IR path is **done + default-on + bt18-soaked**;
`new` scalar replacement is **built, wired, smoke-tested, and merged but gated
OFF** awaiting a gauntlet soak (Gap A); `Op::Call` is the remaining large lever
(Gap B).
