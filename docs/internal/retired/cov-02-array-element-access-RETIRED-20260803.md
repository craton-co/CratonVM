# COV-02 — a `float[]` element could be lowered and an `int[]` element could not

**Retired 2026-08-03.** Every opcode the lane owned now has an
`IrBuilder::build` arm and a lowering, and the measured refusal count for all
seven is **zero**. The brief this replaces was
`docs/known-issues/c2/cov-02-array-element-access.md`, now archived at
`../known-issues/c2/cov-02-array-element-access.md`.

## What the lane claimed, and what was true

The claim held. On `origin/dev` at `48fba3a31`, `IrBuilder::build` had arms for
`faload`/`daload`/`fastore`/`dastore` and for no integral or reference array
access at all. The `Op::ArrayLoad` / `Op::ArrayStore` / `Op::ArrayLength` nodes,
their arity table, their alias classes, their range lattice, their escape-analysis
bridge entry and their memory-token slots **all already existed** — `ArrayLength`
had been sitting on `ir_lower`'s `UNLOWERABLE` list, constructed only by
`#[cfg(test)]` code, since the IR tier was built. Nothing had to be designed;
three arms and two emitters had to be written.

Three deltas against the brief, recorded here rather than left in prose. The
third is in "How it is verified" below, because it is a property of the test
rather than of the finding.

1. **"`CRATONVM_DBG=ir-compiles` before/after on `AutoConfigurationSorterTests`
   (43 of the 77 events are there)" is wrong.** Four of the 43 `arraylength`
   events are there. The 43 is the total across the survey's ten workloads, and
   28 of them are in `ConditionalOnPropertyTests` — the 1,372-request workload,
   not the 352-request one the brief names. Re-derived below across the three
   Spring Boot workloads the survey used.
2. **"For `aaload`, a moving-GC test. `CRATONVM_MOVING_YOUNG` is what makes an
   unpublished root observable" does not hold today.** It cannot: with
   `cratonvm_types::flags::JIT_PUBLISHES_RELOCATION_CONTRACT == false`, a young
   collection that finds a compiled frame it cannot prove a root map for falls
   back to the NON-MOVING sweep instead of relocating. `probes/IrArrayAccessProbe`
   prints that fallback three times per run
   (`reason=compiled-frame-oop-not-published`). So the moving-GC test is real and
   worth having, but what it proves is that the access is correct under
   collection pressure, **not** that the root is published — the veto, not the
   map, is what stands between an unpublished root and a stale pointer. Typing
   the node `IrType::Ref` (which is what makes `emit_safepoint_map` publish its
   slot) is still the right thing and is done; it just is not what the probe
   measures.

## What landed

`jit/src/ir.rs` — three new arms in `IrBuilder::build`'s opcode match:

| opcode | mnemonic | node |
|---|---|---|
| `0x2e` `0x2f` `0x32` `0x33` `0x34` `0x35` | `iaload` `laload` `aaload` `baload` `caload` `saload` | `Op::ArrayLoad(MemKind::…)` |
| `0x4f` `0x50` `0x54` `0x55` `0x56` | `iastore` `lastore` `bastore` `castore` `sastore` | `Op::ArrayStore(MemKind::…)` |
| `0xbe` | `arraylength` | `Op::ArrayLength` |
| `0x5a` | `dup_x1` | (stack shuffle, no node) |

`jit/src/ir_lower.rs` — `emit_gpr_array_elem_load`, `emit_gpr_array_elem_store`,
and an `Op::ArrayLength` arm. `ArrayLength` came off `UNLOWERABLE` and joined
`op_defines_result_slot`.

`jit/src/ir_optimize.rs` — `Op::ArrayLength` joined the DCE root set, for one
reason: it throws NullPointerException on a null array through the same deopt
guard, and `int n = a.length;` with `n` unused still has to throw.

The lane went **wider than its own table** in one direction and stopped short in
another, both deliberately:

* **Wider**: `laload`/`saload`/`lastore`/`castore`/`sastore` were not in the
  survey's event list. They are the same node with a different `MemKind`, and
  shipping `caload` without `castore` would have reproduced, in the same file,
  the exact asymmetry this brief was written about. The category-2 pair is
  admitted only under `ir_emit_long`, which `is_category2_opcode` already knew
  about — no gate work was needed.
* **Short**: **`aastore` (0x53) is out of scope and stays out.** A reference
  element store needs the SATB pre-write barrier and the card-mark write barrier
  that the single-pass backend emits around `emit_ref_astore_regs`. A missing
  barrier is invisible until a concurrent collection drops the only path to an
  overwritten-but-live target. The refusal is written down in the builder (next
  to the store arm) and enforced in the lowerer, which latches a bailout rather
  than emitting a barrier-less store.

### The three questions the brief said each load has to answer

1. **The bounds check.** Emitted, by `emit_array_null_bounds_guards` — the same
   guard the FP arms already used, unchanged. One unsigned `CMP ECX, R10D`
   covers both ends of the range, because a negative index has a huge unsigned
   value. On failure it **deopts**: control leaves for the shared stub, and the
   interpreter re-executes the opcode and throws the real
   ArrayIndexOutOfBoundsException with the method's own handler semantics.

   *And what would let a later lane elide it?* The brief says `jit/src/x64/bce.rs`
   is the single-pass answer and "is not reusable as-is". That is true, and the
   reason is specific rather than a matter of effort: **every entry point in that
   file is `pub(super)`** — private to the `x64` module — **and its whole
   vocabulary is bytecode coordinates**. `find_iv_step_provenance(code, …)`,
   `classify_local_kinds(code, code_len, num_locals)`, `local_access_at(code, pc)`
   all reason about *bytecode PCs and JVM local slots*. The IR has neither: its
   array base is a graph node, and its index is a node, not a local. Making
   `bce.rs` public would hand the IR path a set of facts keyed to a coordinate
   space it does not inhabit. The IR's own equivalent already exists and is in
   the right space — `jit/src/range_analysis.rs` gives `Op::ArrayLength` the
   `Range::array_length()` lattice element, and an `Op::ArrayLoad`'s index is a
   node whose range that lattice can already bound. **A `cov-*` lane that wants
   BCE on the IR path should extend `range_analysis`, not export `bce.rs`.**
2. **The null check.** Same guard, same deopt. `arraylength` has no index and so
   has only this one fault — and it is the fault this VM has already shipped
   without: a raw `MOV EAX, [RAX + ARRAY_LENGTH_OFFSET]` dereferences low memory
   and SIGSEGVs, because the crash handler dumps an `hs_err` and re-raises rather
   than throwing.

   *And what would let a later lane elide it?* `jit/src/null_check_elim.rs` is
   in better shape than `bce.rs`: its entry point is `pub fn analyze(code,
   code_len) -> NullCheckInfo`, already crate-visible, so the IR path can call
   it today. But its answer is `is_nonnull(pc, local)` — again keyed to a
   **bytecode pc and a JVM local slot**. An IR `Op::ArrayLoad`'s base is a node.
   The bridge is therefore not a visibility change but a mapping: at the access's
   `bytecode_pc`, which local (if any) does this base node come from? The
   builder knows that when it pops the array off the abstract stack and throws it
   away. **Nothing in this lane needs it; the next lane that does should thread
   the local index onto the node rather than try to recover it afterwards.**
3. **`aaload` is a reference load.** The node is `IrType::Ref`, so
   `emit_safepoint_map`'s scan publishes its spill slot as a rewritable root at
   every later safepoint, and `plan_slots` colours it out of the **reference**
   pool so no primitive can inherit a word an oop map names.

   Delta 2 above says the brief's suggested proof — run it under
   `CRATONVM_MOVING_YOUNG` — cannot work. So the property is asserted where it
   is actually decided, in
   `ir_lower::tests::an_aaload_result_is_reference_typed_and_takes_a_reference_slot`:
   build the real `aload_0; iload_1; aaload; areturn` bytecode, assert the
   resulting node is `Op::ArrayLoad(MemKind::Ref)` typed `IrType::Ref`, and
   assert `plan_slots` gave it `SlotClass::Ref`. The edit that trips it is
   typing the `0x32` arm `IrType::Int` — which compiles, passes every value
   differential in `ir_vs_singlepass.rs`, and loses the element at the first
   relocating collection.

### Three things worth copying rather than re-deriving

* **Every emitted instruction is byte-identical to the single-pass twin** in
  `jit/src/x64/arrays.rs`. That is not tidiness: `jit/tests/ir_vs_singlepass.rs`
  compares the two backends' answers on the same bytecode, so a divergence in an
  extension rule (`baload` sign-extends, `caload` zero-extends — same width,
  opposite rule) is a wrong-code bug the harness exists to catch, and the
  cheapest way not to have one is to emit the same instruction.
* **Eleven instruction encodings, two header-displacement sites.** Each emitter
  materialises `disp::disp8_const(HEADER_SIZE as i64)` once and shares it across
  every element width. The object-header shrink has to visit every place this
  crate bakes a layout constant into an instruction, and `disp8_const` is a
  `const fn` that fails the BUILD above 127 — so these are the first sites in
  `ir_lower.rs` for which "the shrink went the wrong way" is not representable.
  `docs/internal/arch-2026-07-26/{layout-constant-hazards,header-shrink}.md` were
  updated in the same change, as their tripwires demand.
* **`debug_assert!(false, …)` on an unreachable arm is a fail-open**, and both
  emitters shipped one before review caught it. It vanishes in release: the GPR
  load would then emit nothing, and the caller's `store_rax(slot)` would still
  run and spill whatever RAX happens to hold — the array pointer — as the
  element's value; the store would be silently dropped. Both arms are
  structurally unreachable today, which is exactly the claim this directory's
  rule 3 says to *enforce* rather than assert. They latch a bailout now.

## The measurement

Three Spring Boot workloads, default configuration, one run each,
`CRATONVM_DBG=ir-compiles`. `/data/sbrun.sh` on the Azure Linux host, against
`core/spring-boot-autoconfigure`.

| | before | after |
|---|---:|---:|
| admitted to the optimizing pipeline | 975 | 970 |
| **bodies the optimizing backend produced** | **591** | **652** |
| admitted and never lowered | 384 (39%) | 318 (33%) |

Per workload, bodies: `AutoConfigurationSorterTests` 124 → 134,
`ConditionalOnClassTests` 57 → 77, `ConditionalOnPropertyTests` 410 → 441.
All three still pass (18 / 5 / 38 tests, 0 failed).

Per opcode, `[ir] IrBuilder::build has no lowering for opcode 0xNN`:

| opcode | mnemonic | before | after |
|---|---|---:|---:|
| `0xbe` | `arraylength` | 42 | **0** |
| `0x32` | `aaload` | 17 | **0** |
| `0x5a` | `dup_x1` | 6 | **0** |
| `0x33` | `baload` | 6 | **0** |
| `0x34` | `caload` | 4 | **0** |
| `0x2e` | `iaload` | 3 | **0** |
| `0x54` | `bastore` | 1 | **0** |
| | **lane total** | **79** | **0** |

The survey's own totals for these seven were 77 across ten workloads; 79 across
these three is the same population measured again, and is the re-derivation the
directory's rule 6 asks for.

### The gaps behind the ones that closed

Exactly what the c2 README predicts when a refusal is lifted: methods that were
dying at an array opcode now die at whatever they meet next. **These are not
regressions**, they are the same methods failing later.

| opcode | mnemonic | before | after | owner |
|---|---|---:|---:|---|
| `0xb2` | `getstatic` | 91 | 92 | `cov-01` |
| `0x12` | `ldc` | 90 | 92 | `cov-01` |
| `0x13` | `ldc_w` | 7 | 7 | `cov-01` |
| `0xbc` | `newarray` | 1 | 5 | `cov-06` |
| `0x53` | `aastore` | 0 | 2 | **nobody** — see below |
| `0x5c` | `dup2` | 0 | 1 | **nobody** |

`aastore` is this lane's own declined item and is written up above; two events
is the measured cost of declining it. `dup2` is a stack shuffle in the same
family as `dup_x1` and was deliberately left alone: it is one event, it belongs
to no lane's ownership table, and its category-2 form needs a decision about the
builder's one-entry-per-value abstract stack that `dup_x1` did not
(`dup_x1` refuses a category-2 operand outright; `dup2`'s form 2 is a legitimate
category-2 shape that would have to be handled, not refused).

## How it is verified

* `jit/tests/ir_vs_singlepass.rs` — 14 new cases. Same method, both backends,
  same answer, plus a host anchor. Every one asserts `used_ir_backend` first,
  because without that a builder refusal falls back to single-pass and the
  differential compares single-pass with itself — the exact way the `getfield`
  corpus in that file was vacuous before the compact-layout refusal moved.
  `baload` vs `caload` vs `saload` are three separate cases on purpose: same
  address, three different extension rules.
* **The fault paths are covered, and the two backends do not report through the
  same channel** — a third delta against the brief, which asks for "the same
  exception with the same bci from both backends":
  * single-pass out-of-bounds calls `helpers.throw_aioobe(index, length,
    array_ptr, bci)`. The bci is an argument, and it is directly comparable.
  * single-pass null calls `helpers.jit_npe_with_action(action)`. It passes an
    element-type action code and **no bci at all**, so "the same bci" is not
    checkable on that path.
  * the IR side deopts either way; `ir_deopt_entry` stashes a
    `ReconstructedFrame` whose `bci` is the trapping bytecode.

  So the tests assert the sentinel return and the fault *class* on both sides,
  and the bci on both sides for AIOOBE. `fault_helpers()` replaces the two
  panicking "unwired helper" stubs with recorders, which is what makes those
  paths reachable in a unit test at all.
* `probes/IrArrayAccessProbe.java` — the E2E half. Real heap arrays, every
  kernel warmed past tier-up, identity checks on `aaload` results held live
  across enough allocation to force young collections, and the exception cases
  caught as real Java exceptions. **PASS on HotSpot and on CratonVM**, and
  `CRATONVM_DBG=ir-compiles` confirms all 13 kernels reach the optimizing
  backend (`produced a body for IrArrayAccessProbe.{iaload,baload,caload,saload,
  laload,aaload,arraylength,iastore,bastore,castore,sastore,elemPlusLen,
  dupX1Shape}`). A run that prints PASS while that grep is empty proves nothing
  about this lane, which is why the probe's header says so. It also passes with
  `CRATONVM_COMPRESSED_OOPS=1` — the only thing that exercises `aaload`'s narrow
  4-byte element decode, which is otherwise dead code — and under `--nojit`,
  which is the interpreter agreeing with both compiled backends.
* Sixteen further `spring-boot-autoconfigure` test classes, run A/B **interleaved**
  (base binary, then new binary, per class) so a load excursion on the shared
  host hits both arms rather than manufacturing a one-sided red. 16/16 PASS on
  both. With the three survey workloads that is 19 Spring Boot classes green on
  the new binary.
* Three source-scanning gates fired during the change and were all real:
  `the_op_representatives_cover_every_declared_variant`,
  `layout_constant_emission_sites_are_inventoried`, and
  `ir_lower_header_offset_sites_are_inventoried_too`. The first was a false
  alarm caused by the transport (a CRLF copy of `ir.rs` on the Linux build host
  defeats a scan that splits on `"\n}\n"`, and it then silently scans the rest
  of the file — worth knowing, because the failure looks like a code defect);
  the other two were the inventory doing its job.
* `cargo test -p cratonvm-jit --release`: 1,868 lib + 213 integration, 0 failed,
  on the tree merged forward to `dev`.

  The rest of the workspace was **not** a usable gate on 2026-08-03 and the
  reason is worth writing down rather than rediscovering:
  `native-collections/tests/gc_relocation_harness.rs` does not compile on `dev`
  (`gc_overlay_roots_for_collection` gained a second parameter and one call site
  did not follow), and the Azure host was carrying three other sessions' builds,
  so `rustc` was OOM-killed on the LTO'd `cratonvm-vm` integration test binaries
  — which reads as "could not compile", not as "out of memory". Neither is
  related to this change; both are load-bearing context for anyone reading a red
  workspace run from that day.

## What is left

Nothing in this lane. The residuals it exposed are listed in "The gaps behind
the ones that closed" above and belong to `cov-01`, `cov-06`, and — for
`aastore` and `dup2` — nobody yet.

Re-run the survey (`docs/known-issues/c2/ir-coverage-survey-20260803.md`) before
sizing `cov-01` from its numbers: 61 more methods reach the backend than did
when it was written, and the ones that still do not are a different set.
