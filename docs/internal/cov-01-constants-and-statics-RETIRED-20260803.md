# COV-01 — `getstatic` and `ldc`, RETIRED 2026-08-03

The lane brief is reproduced at the bottom. It owned the `0x12` / `0x13` /
`0xb2` arms of `IrBuilder::build`'s opcode match in `jit/src/ir.rs` and whatever
`jit/src/ir_lower.rs` needed to lower the nodes they add. All four of its
increments landed; the three opcodes are at **zero** events on the workload the
lane was sized from.

## The measurement

`CRATONVM_DBG=ir-compiles` on `ConditionalOnPropertyTests`, default
configuration, the Spring Boot runner's usual `CRATONVM_JIT=rootsnap-cache`.
Two runs per arm, **interleaved and in both orders** — these are counts, but
tiering is time-driven, so the number of compile requests a run issues is not
perfectly stable and a block-per-arm layout would attribute that drift to the
change.

| | base r1 | base r2 | cov-01 r1 | cov-01 r2 |
|---|---:|---:|---:|---:|
| admitted to the optimizing pipeline | 695 | 694 | 694 | 695 |
| **bodies the optimizing backend produced** | **410** | **410** | **501** | **502** |
| `IrBuilder::build returned None` | 282 | 281 | **180** | **180** |
| `no lowering for opcode 0x12` | 77 | 77 | **0** | **0** |
| `no lowering for opcode 0x13` | 6 | 6 | **0** | **0** |
| `no lowering for opcode 0xb2` | 72 | 71 | **0** | **0** |
| `SBRUNNER_RESULT` | 38/0/0 | 38/0/0 | 38/0/0 | 38/0/0 |

**+91 bodies, +22%.** 155 opcode-gap events removed, 101 fewer refused builds.
The tests pass in every arm and no run crashed.

Quote both numbers, as the brief asks, because they do not match: 155 events
went away and only 91 bodies appeared. The difference is the outcome the brief
predicted — "a method can stop failing on `0x12` and immediately fail on the
next unlowered opcode in the same body — that is a real outcome and it is not a
body." What those methods now fail on is in the next section.

Increment 1 alone (immediate `ldc` only) was +9 bodies from 83 events removed:
almost every `ldc`-bearing method had something else in it too. The `getstatic`
increment is where the body count actually moved.

The other two Spring workloads from the survey, same protocol, agree:

| workload | bodies before | bodies after | Δ | `cov-01` gaps | tests |
|---|---:|---:|---:|---|---|
| `ConditionalOnPropertyTests` | 410 | 501–502 | **+91 (+22%)** | 155 → **0** | 38/38 |
| `AutoConfigurationSorterTests` | 123–124 | 142 | **+18 (+15%)** | 23 → **0** | 18/18 |
| `ConditionalOnClassTests` | 57–58 | 63 | **+6 (+10%)** | 12–13 → **0** | 5/5 |
| **total** | **590–592** | **706–707** | **+116 (+20%)** | 190–191 → **0** | — |

That total is the survey's own headline figure for all ten workloads (592
bodies): the bench phases contribute two and these three classes contribute the
rest. So **+20% on the whole measured population**, not on a chosen slice.

## Where the 180 remaining refusals are, and who owns them

The whole opcode gap is now `cov-02`'s. Re-derived from the same runs:

| opcode | mnemonic | before | after | lane |
|---|---|---:|---:|---|
| `0xb2` | `getstatic` | 72 | **0** | cov-01 |
| `0x12` | `ldc` | 77 | **0** | cov-01 |
| `0x13` | `ldc_w` | 6 | **0** | cov-01 |
| `0xbe` | `arraylength` | 28 | 31 | `cov-02` |
| `0x32` | `aaload` | 10 | 12 | `cov-02` |
| `0x2e` | `iaload` | 3 | 6 | `cov-02` |
| `0xbc` | `newarray` | 1 | 3 | `cov-06` |
| `0x5a` | `dup_x1` | 1 | 1 | `cov-02` |
| `0x33` | `baload` | 1 | 1 | `cov-02` |
| `0xb3` | `putstatic` | 0 | **1** | **nobody** |

And the structural refusals inside the builder, which is where most of the
shortfall went:

| site (post-change line) | what it is | before | after | lane |
|---|---|---:|---:|---|
| `ir.rs:5331` | `invokespecial` that is neither resolvable nor a trivial `<init>` | 36 | **71** | `cov-04` |
| `ir.rs:5212` | `putfield` whose type tag is not `I/Z/B/C/S` | 33 | 38 | `cov-03` |
| `ir.rs:5391` | an invoke with no `invoke_info` at that pc | 8 | 9 | `cov-04` |
| `ir.rs:5168` | `getfield` of a `long`/`float`/`double` | 5 | 5 | `cov-03` |
| `ir.rs:5346` | `invokespecial <init>` whose receiver is not a fresh `new` | 1 | 1 | `cov-04` |
| `ir.rs:5256` | a `new` with no entry in `new_info` | 0 | **1** | nobody |

`cov-04`'s largest refusal **doubled**, 36 → 71, without anyone touching it.
That is the README's own rule playing out one level down: lifting a refusal
admits methods that were hiding behind it, and they fail on whatever they meet
next. `cov-04` is now the single largest thing between the optimizing tier and
those 180 methods, and it is the lane the README already says cannot be sized
from the survey.

Two rows appeared that were previously invisible, both at one event and neither
owned by a lane: `putstatic` (see "What is refused" below) and a `new` whose
site the scalar-new resolver declined. Recording them is the point of
re-deriving the table rather than assuming the ranking held.

## What landed

Four increments, in the brief's order, each a separate commit.

**1 — `ldc` / `ldc_w`, the immediate case.** A constant node and nothing else.
The one thing the caller did *not* already supply was the **width**: both the
`int` and the `float` case arrive as `JitLdcConstant::Immediate`, because the
single-pass backend never needs to know which it has — it pushes the bits with
`MOV imm64` and the CONSUMING opcode picks the width. The IR builder has no
consuming opcode to ask, and it cannot infer the width from admission either,
because `is_float_opcode` does **not** list `ldc`: a body whose only
floating-point content is the constant has `fp_in_body() == false` and is
admitted through the int clause. So `Immediate` grew an `is_float` flag, the
same shape `cp_ldc2w_resolver`'s `(bits, is_double)` already has, and a float
entry is fed to the builder only under `ir_emit_fp`.

**2 and 3 — `ldc <String>` and `ldc <Class>`.** Two new IR nodes,
`Op::ConstString { bytes, len }` and `Op::ConstClass { holder_class_id,
cp_idx }`. Both name a constant-pool SITE and materialise their value on every
execution through the same helper the single-pass backend calls. That is the
correctness requirement, not a missed optimisation: an `ObjectRef` baked at
compile time is stale the moment a relocating collector moves it.

**4 — `getstatic`.** `Op::LoadStatic { class_id, field_index, type_tag,
is_volatile }`, with the two lowerings the single-pass `0xb2` arm has, chosen by
the same predicate rather than by a second opinion — `x64::resolve_static_base`
for the direct two-load form, `helpers.getstatic` for everything it declines.

All three nodes carry `[ctrl, mem]` and the `MemAccess::Opaque` memory shape
`Op::Call` has: a safepoint, an allocation, and a barrier nothing reorders
across. That one classification is what makes them correct in the scheduler,
the alias model, DCE, the unroller, the register allocator's clobber and
safepoint sets, and the escape-analysis bridge — none of which needed a
per-node opinion.

## Three things the brief did not say

**The premise held, except in one place, and that place was the work.** The
brief's central claim — "this lane is *consume a table the caller already
computed*, not *resolve a constant pool*" — is true for
`static_field_info`, `ldc_string_info` and `ldc_class_info`, all three of which
the caller computes and hands to the single-pass backend already. It is false
for exactly one field of one table, and the reason it is false is instructive:
the int/float discrimination did not exist *anywhere*, because no consumer had
ever needed it. A lane sized as "plumbing" contained one genuine ABI change.

**`getstatic` is not only an arm.** Compiled code reads static storage
directly, bypassing the interpreter's `ensure_class_initialized_shared`, so an
artifact containing one owes two things that live on the `CompiledMethod` and
not in the opcode arm:

* `static_init_classes` (RBC.5) — the declaring class of every static site, so
  the interpreter's compiled-entry path ensure-initializes it once per
  artifact. `x64::compile` has recorded this since the `jit-clinit-gap` fix;
  the IR path had never needed it.
* `has_dispatch` — `jit_getstatic` resolves `&mut JvmThread` through the
  `JIT_THREAD` TLS in order to run `<clinit>`, and the `!has_dispatch` fast
  entry never sets that TLS. This is the same defect
  `x64/driver.rs`'s `!compiler.static_field_info.is_empty()` clause already
  exists for, whose original symptom was a method writing a static *without
  running `<clinit>` first*.

Neither is visible from the opcode match. Both would have failed silently and
far from their cause — the failure mode this directory's rule 3 is about.

**A gate in the harness was reading garbage.** `ir_lower.rs`'s
`declared_op_variants` — the check that proves the lowering tables cover every
`ir::Op` variant — finds the enum body by splitting `include_str!("ir.rs")` on
`"\n}\n"`. In a CRLF checkout that never matches, so the "enum block" ran to the
end of the file and the check compared its list against every variant of every
*other* enum in `ir.rs`. Verified failing on an unmodified `dev` on Windows; it
passed on Linux only because the checkout is LF there. Fixed by normalising line
endings before the split, which is what let it do its job here: it is the check
that caught the three new variants missing from `op_representatives`.

## What is refused, and where each refusal lives

The brief's "what to refuse" section, as implemented:

* **`J` / `D` / `F` statics** — refused in the **builder**, because the refusal
  is about the value tier and not about the load. Admitting one would put a
  `Long`/`Double`/`Float` node in a graph whose admission clause may have been
  the int one, and a static field carries no equivalent of the `ir_emit_long` /
  `ir_emit_fp` signal the `ldc2_w` arm consults. **Residual**: giving statics
  that signal is a real follow-up and is not hard; nobody owns it.
* **`putstatic`** — not fed to the builder at all. A static reference WRITE owes
  an SATB pre-barrier that no collector `set_field` barrier covers (statics live
  in a Rust-side table, not the heap), which is exactly why the single-pass
  backend keeps writes on `jit_putstatic_*`. This lane owns the read arm. It is
  now a **measurable** 1 event, where before it was hidden behind the
  `getstatic` in the same method.
* **`MethodHandle` / `MethodType` / condy `ldc`** — absent from all three tables,
  so the arm bails. Permanent; no later increment lifts it.
* **an unwired helper** — `ldc_class_cp` is an `OptionalPtr` and is omitted at
  the feed, mirroring the single-pass refusal of the same condition;
  `ldc_string` and `getstatic` are refused by `lower_inner`, the same guard and
  the same history as the monitor helper's.

`ir_compatible` was not widened. Its conjuncts still belong to `cov-05`,
`cov-06` and `cov-07`.

## Verification

**The counter moves.** The table at the top; both numbers quoted.

**`ir_vs_singlepass`.** Nine new differential cases (`jit/tests/ir_vs_singlepass.rs`),
104 in the file, all green — plus the whole `cratonvm-jit` suite on Linux
(1869 + 12 binaries, 0 failures) including the `x64_artifact_corpus` differ.
Five of the nine are fail-closed rungs, and the two most load-bearing were
checked by making them fail:

* deleting the `!is_float || ir_emit_fp` guard makes
  `float_ldc_stays_on_single_pass_with_the_fp_gate_off` fail — verified, not
  asserted. That test also carries a positive control on the *same body* under
  the gate ON, without which "the gate refused it" and "the builder cannot
  lower this shape at all" would be indistinguishable.
* the direct `getstatic` route is **driven, not inspected**. It is the only
  hand-written instruction encoding this lane adds, so the test registers a
  static-base resolver (answering for one class id and declining everything
  else — the discipline `x64::tests::test_getstatic_inline_direct_load_and_fallback`
  already uses, because the setter latches for the life of the process), reads
  back both payload widths through the two `FIELD_CELL_PAYLOAD{32,64}_OFFSET`
  biases, and includes the declined-site control that proves the result is
  measuring the ROUTE rather than merely that `getstatic` works.

**A reference constant is a root.** `probes/Cov01RootProbe.java`. Three
references are materialised by the three cov-01 bytecodes and held live across
an `invokestatic` whose callee allocates hard and then **overwrites the static
field**, so after it returns the compiled frame's copy is the only live
reference to the object that was loaded. 300,000 iterations, `--Xmx 256m`,
default configuration:

```
Cov01RootProbe OK iters=300000 checked=300000
```

Non-vacuous, and the control is exact. Under `CRATONVM_DBG=ir-compiles` the
cov-01 binary reports

```
[ir] optimizing backend produced a body for Cov01RootProbe.heldAcrossSafepoint(I)I
[ir] optimizing backend produced a body for Cov01RootProbe.hotStaticRef()LCov01RootProbe$Box;
[ir] optimizing backend produced a body for Cov01RootProbe.hotString()Ljava/lang/String;
[ir] optimizing backend produced a body for Cov01RootProbe.hotClass()Ljava/lang/Class;
[ir] optimizing backend produced a body for Cov01RootProbe.hotStaticInt()I
```

and the **baseline** binary, on the identical probe, reports **zero** IR bodies
for those methods and instead logs `no lowering for opcode 0x12` (2) and
`0xb2` (3). So the probe exercises the new lowerings and nothing else.

**The half this cannot reach — and it is the half the brief names.** The brief
asks for "does not lose *or fail to rewrite*", and says `CRATONVM_MOVING_YOUNG`
is what makes "not rewritten" observable. It is not observable today, for a
structural reason rather than a missing test:
`cratonvm_types::flags::JIT_PUBLISHES_RELOCATION_CONTRACT` is `false`, so
`conservative_roots` refuses coverage for any frame with no matching oop map —
which is every IR frame, because the IR backend emits none — and `gen_heap`
diverts that cycle to non-relocating. A relocating young collection therefore
cannot observe a live IR frame at all. What the probe *does* exercise is the
"does not LOSE it" half, which a non-relocating young collection reaches
perfectly well, because it still sweeps. The rewrite half becomes testable the
moment that constant flips, and `Cov01RootProbe` is written so it needs no
change when it does.

## One knock-on outside the tier

`vm/tests/pgo02_guarded_virtual_inline.rs` failed, 3/3, on the cov-01 branch —
and passed 3/3 on an unmodified `dev`, which is how it was established as this
lane's and not a pre-existing red.

Every entry point in its fixture is `getstatic <A>; invokevirtual tag`. Until
this lane, the IR builder had no `0xb2` arm, so the optimizing tier refused
those methods and the C1 artifact was what stayed in the cache. Once
`getstatic` lowered, `callA` was compiled by C2 on its next promotion, its
`inline_tally` was empty, and `check_guard_hit` reported
`speculative_sites == 0` — "the guard never fired".

The capability under test is single-pass-only: guarded monomorphic inlining is
planned by `x64`'s inliner and reported through `CompiledMethod::inline_tally`,
while the IR backend serves a virtual site from a MIC/PIC cascade and records
no tally. So the test was passing *because* the IR builder refused `getstatic`,
and nobody had written that dependency down. It now pins the tier with
`CRATONVM_JIT_IR_CALL_VIRTUAL=0` (the declared opt-out) and asserts
`!used_ir_backend` **first** — without that rung, "the guard did not fire" and
"a different backend compiled it and was never asked to plan an inline" are
indistinguishable, and they are opposite conclusions.

Nothing in the product regressed: the flag it drives is default-off, and the
site the IR tier now owns is served by an inline cache rather than by nothing.
But the general form is the thing to carry forward, and it is recorded in
`docs/feature-designs/profile-guided-inlining.md` §8: **as the `cov-*` lanes
land, every single-pass-only capability loses population.**
`getstatic; invokevirtual` is the most ordinary virtual-call shape there is,
and one lane took the whole shape away from PGO-02.

## Residuals

| Residual | Where | Owner |
|---|---|---|
| `J` / `D` / `F` statics are refused; statics have no `ir_emit_long`/`ir_emit_fp` equivalent | `IrBuilder::build`'s `0xb2` arm | nobody |
| `putstatic` (`0xb3`) has no IR lowering — 1 measured event, deliberately out of scope (the SATB pre-barrier) | `ir.rs`, `ir_lower.rs` | nobody |
| the "fails to rewrite" half of the root test is unreachable until `JIT_PUBLISHES_RELOCATION_CONTRACT` flips | `types/src/flags.rs` | nobody |
| `cov-04`'s `invokespecial` refusal is now 71, double what the survey measured | [`cov-04`](../known-issues/c2/cov-04-the-invoke-arms.md) | `cov-04` |
| a `new` with no `new_info` entry, 1 event, newly visible | `ir.rs:5256` | nobody |
| guarded virtual inlining (`pgo-02`, default-off) no longer reaches a `getstatic; invokevirtual` method | `docs/feature-designs/profile-guided-inlining.md` §8 | nobody |

## Files

`jit/src/ir.rs` (the three arms, the three `Op` variants, the memory shape),
`jit/src/ir_lower.rs` (three lowering arms, `emit_inline_getstatic`, the
unwired-helper refusals), `jit/src/lib.rs` (the four table feeds, the
`JitLdcConstant::Immediate` shape, `static_init_classes` / `has_dispatch` /
`_jit_strings` on the artifact, the layout-constant inventory),
`jit/src/ir_verify.rs`, `jit/src/ir_optimize.rs`, `jit/src/regalloc.rs`,
`vm/src/runtime/interpreter/jit_bridge.rs` (three `ldc` resolvers),
`jit/tests/ir_vs_singlepass.rs`, `probes/Cov01RootProbe.java`,
`docs/internal/arch-2026-07-26/{layout-constant-hazards,header-shrink}.md`.

---

## The brief, as written

# COV-01 — `getstatic` and `ldc`: 69% of every opcode gap

**Status:** not started. **Independent of every other lane.**
**Owns:** the `0x12` / `0x13` / `0xb2` arms of `IrBuilder::build`'s opcode
match in `jit/src/ir.rs`, and whatever `jit/src/ir_lower.rs` needs to lower the
nodes they add. Nothing else.

## The measurement

189 of 273 opcode-gap events, across three Spring Boot classes:

| opcode | mnemonic | events |
|---|---|---:|
| `0xb2` | `getstatic` | 92 |
| `0x12` | `ldc` | 90 |
| `0x13` | `ldc_w` | 7 |

`ir-coverage-survey-20260803.md` has the derivation. There is no arm for any of
the three — `IrBuilder::build`'s match ends at `0xb9`, and `[ir] IrBuilder::build
has no lowering for opcode 0x12` is what a method containing a string literal
gets.

## Why this one is first

It is the largest bucket, and it is the one whose *inputs already exist*. The
single-pass backend receives `ldc_info` (int/long constants), `ldc_string_info`
(interned string pointer), `ldc_class_info` (CP-indexed class handle) and
`static_field_info` (`(pc, cp_index, offset, type_tag, is_ref)`) as
caller-supplied per-pc tables, and `compile_with_param_slots` already threads
all four. The IR builder is handed the same compile request. So this lane is
"consume a table the caller already computed", not "resolve a constant pool".

Check that before writing code: if `IrBuilder` cannot see those tables today,
plumbing them is the first increment and the opcode arms are the second.

## The first increment

`ldc` / `ldc_w` for the **int and long** cases only — the ones `ldc_info`
already carries as an `i64`. That is a constant node and nothing else: no
memory edge, no safepoint, no GC interaction, no new refusal. It is the
smallest change in this directory that moves a measured number.

Then, in order of what the survey says and what each costs:

1. `ldc` of a **String** — the value is an already-interned pointer, so the
   node is a constant too, but it is a **reference** constant and every
   safepoint from there on must publish it as a root. Do not start it until
   increment 1 is green.
2. `ldc` of a **Class** — CP-indexed, served by a helper in the single-pass
   backend (`ldc_class_cp`). Same root question.
3. `getstatic` — a load from a statics base plus the same ref/non-ref split.
   `static_field_info` names the offset and the type tag. The reference case
   has the root question again *and* the class-initialisation question: the
   single-pass backend's `0xb2` arm is the reference implementation for both,
   and this lane's job is to match its behaviour, not to invent one.

## How to verify

* **The counter moves.** `CRATONVM_DBG=ir-compiles` on
  `ConditionalOnPropertyTests` before and after; `no lowering for opcode 0x12`
  goes to zero for increment 1, and `optimizing backend produced a body` rises.
  Quote both, because a method can stop failing on `0x12` and immediately fail
  on the next unlowered opcode in the same body — that is a real outcome and it
  is not a body.
* **`ir_vs_singlepass`** (`jit/tests/ir_vs_singlepass.rs`, 92 tests) is the
  differential harness for exactly this: same method, both backends, same
  answer. A new arm lands with a case there.
* **A reference constant is a root.** For increments 1–3, a test that a GC at a
  safepoint after the `ldc`/`getstatic` does not lose or fail to rewrite the
  loaded reference. `CRATONVM_MOVING_YOUNG` is what makes "not rewritten"
  observable; a non-moving run will pass while the bug is present.

## What to refuse

Anything whose constant this lane cannot prove is already resolved. The
single-pass backend has a *deferred* path for a class that is not yet loaded
(`ldc_class_info`'s CP index, `new_deferred_info`'s sibling treatment) because
resolution can throw, run `<clinit>`, and re-enter. If the IR builder cannot
express that, it must refuse the method — the same way it refuses today, only
narrower. A constant materialised without its resolution side effects is a
wrong-code bug, not a missing optimisation.

And refuse to widen `ir_compatible` in this lane. Its conjuncts belong to
`cov-05`, `cov-06` and `cov-07`; touching them here is how two lanes collide.

## Ownership note

`cov-01` … `cov-04` all edit **one match statement** in `jit/src/ir.rs`. The
arms are disjoint and hundreds of lines apart, but the file is 365 KB and a
long-lived branch will conflict. Rebase daily; land increments, not lanes.
