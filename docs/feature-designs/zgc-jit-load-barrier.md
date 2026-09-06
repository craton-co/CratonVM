# The ZGC JIT-side load barrier

**Status:** Designed, not built. No barrier emission exists in the JIT — a
search of `jit/src` and `jit-api/src` for ZGC finds only two incidental test
comments.

This document is the correctness argument, the site inventory and the cost
model for the barrier that a compacting ZGC would need. It is a design, and
implementing it is gated on
[`zgc-production-implementation-plan.md`](zgc-production-implementation-plan.md)
adopting the relocating machinery at all. The slot-shape half of the question
is owned by
[`zgc-reference-slot-representation.md`](zgc-reference-slot-representation.md).

## 0. Verdict, up front

**Yes: relocation must refuse to run with the JIT enabled until stage (a) of
this plan lands.** Not "should" — the alternative is a use-after-free with no
error path, and the tree already has the exact refusal machinery to express it
(`vm/src/vm/vm_init.rs:1396-1401`).

Several things in the commissioning framing did **not** survive contact with the
source. §9 has the full list; these three move the estimate most:

| Claimed | Actual |
|---|---|
| "(c) aarch64" is a third stage of real work | **aarch64 needs nothing.** The aarch64 backend has *no lowering at all* for `getfield`, `getstatic`, `aaload`, `aastore`, `new`, `anewarray` or any `invoke*`; a method containing one is refused (`jit/src/aarch64_backend.rs:44-62`, coverage test at `:6614-6640`). Its admissible population is "leaf methods that touch no object, no array, no field, no call, no monitor and no exception" (`:63-65`). There is **no reference load on aarch64 to barrier.** Stage (c) collapses to a one-line assertion + a test. |
| The `CALL jit_getfield` helper arm "is nearly free — the barrier lands in Rust" | **Half true, and the other half is a landmine.** The barrier does land in Rust, but `jit_getfield`'s own compact-reference arm runs the loaded word through `plausible_heap_pointer` and **silently returns `0`** when it fails (`vm/src/jit/helpers.rs:5208-5212` pre-change). A colored word has bit 63 set (`gc/src/zgc/vaddr.rs:264`) and is *designed* to fail that test. There are **6 such value-degrading sites** in `vm/src/jit/helpers.rs` — **7 in the tree**, the seventh being `read_prim_element` (`gc/src/heap.rs:1710-1714`) in a different crate and a different workstream. Of the six, **one is unreachable** and **three want the barrier one call frame upstream**, so only **two** are barrier sites in the literal sense (§2.5). It is on the helper path, not only the inline path. |
| The fast path is "one AND against a mask plus one branch", target ~3-4 instructions | **The colored word is a heap OFFSET, not a machine address**, because `vaddr` is *single*-mapped: `uncolor_unchecked` is `base + (colored & Z_OFFSET_MASK)` and its own doc says "Two instructions: `AND` then `ADD`. This is precisely the cost that OS multi-mapping would eliminate" (`gc/src/zgc/vaddr.rs:1020-1036`). And `Z_NULL` is the all-zero word (`vaddr.rs:279`), so a naive `AND`+`ADD` turns **null into `base`**. The honest x64 fast path is **6-7 instructions**, not 3-4 (§3.1). |

What *did* survive: `inline_card_mark_available()` really does return a
hardcoded `false` (`jit/src/x64/objects.rs:29-45`) for the stated reason, and it
is the single most relevant precedent in the tree. And the slot-representation
study's blast-radius claim survives verbatim — **zero JIT sites need changing
for slot width** (§2.6).

Effort bands: **(a) 3-5 days**, **(b) 3-5 weeks**, **(c) < 1 day**. Only (a) is
required before relocation may be enabled at all.

---

## 1. The correctness argument, and what must gate what

### 1.1 What breaks

Under a relocating ZGC, a reference slot in the heap holds a *colored word*:
a 42-bit heap offset (`gc/src/zgc/vaddr.rs:225`), one of four metadata bits at
42-45 (`:237-251`), and CratonVM's `Z_COLORED_TAG` at bit 63 (`:264`). It is not
a machine pointer, and the object it names may already have been evacuated.

The interpreter-side barrier turns that word into a live machine address, and
heals the slot, on every read (`gc/src/zgc/barrier.rs:968-989`). JIT-compiled
code that skips it does one of four things, in ascending order of how long it
takes to find:

1. **Dereferences a colored word directly.** Bit 63 puts it above the 47-bit
   window `plausible_heap_pointer` and `CompactValue`'s NaN box assume
   (`vaddr.rs:257-264`), so on x86-64 this is a non-canonical address and faults
   immediately. This is the *good* failure — it is exactly what bit 63 exists
   to produce.
2. **Silently degrades it to null.** `jit_getfield`'s compact-ref arm
   (`vm/src/jit/helpers.rs:5208-5212` pre-change), `jit_aaload`'s element read
   (`:4902-4906`), `jit_getfield`'s legacy-`Value` arm (`:5262`),
   `jit_getstatic`'s two object arms (`:6120-6124`, `:6142-6147`) and
   `read_prim_element`'s Reference arm (`gc/src/heap.rs:1710-1714`) all answer
   `0` / `Object(None)` for an implausible word. Java sees a spurious `null`
   where a live object was. **This is the dangerous one**: no crash, no log, a
   wrong answer — and, as of this pass, no *counter* either: the JIT arms feed
   nothing, so `object_degradation_count()` reads `0` through all of it
   (`types/src/compact_value.rs:302`, `:330`; §2.5.4).
3. **Uses a stale from-space address.** If the JIT's inline load bypasses the
   barrier while the word is still *uncolored* (a slot an un-migrated writer
   wrote — the coverage hazard `barrier.rs:634-654` spells out), the fast path's
   bad-mask test passes it as `Good` and hands back its low 42 bits: a machine
   pointer truncated to 42 bits, "garbage, silently" (`barrier.rs:645-646`).
4. **Never heals.** The self-heal CAS lives only in the slow path
   (`barrier.rs:937`). A slot only ever read by compiled code stays stale for
   the whole cycle, so every interpreter read of it also takes the slow path —
   a throughput cliff rather than a correctness bug, but it is how a partially
   barriered VM presents in a profile.

The plan doc's R3 already states the shape of this ("a load barrier that the
interpreter honors and compiled code does not is not a partial barrier — it is
a broken one, because the first tier-up silently drops the invariant",
`zgc-production-implementation-plan.md:295`). This section is that risk, priced.

### 1.2 Why "just don't tier up" is not an available answer

There is no VM-wide switch that makes the interpreter the only reader of the
heap. Even with whole-method compilation disabled, the JIT emits reference loads
from paths the bytecode walker does not gate on `getfield` at all:

* the **String intrinsics** load `String.value` directly into a GPR at nine
  sites (`jit/src/x64/bytecode_walk.rs:7328`, `:7381`, `:7587`, `:7593`,
  `:7736`, `:7744`, `:7875`, `:7976`, `:7984`, all through
  `emit_load_string_value_ptr`, `jit/src/x64/objects.rs:196`);
* **LICM** hoists an `aaload` to a loop header and parks the result in a
  dedicated frame slot (`jit/src/x64/bytecode_walk.rs:696-698`);
* the **SIMD matmul/dot intrinsic** loads a row pointer and parks it in a
  callee-saved register (`jit/src/x64/simd.rs:326`);
* the **`aastore` SATB pre-write barrier** *reads* the old reference before
  storing (`jit/src/x64/bytecode_walk.rs:1804`).

Each of these is a heap reference load that the `0xb4` / `0x32` opcode arms do
not own. A "no JIT reference loads" claim has to cover all of them, which is
what makes the coverage assertion in stage (a) worth building before the
emitter.

### 1.3 The refusal gate — what it should look like

There is direct precedent, and it is load-bearing rather than decorative.
`vm/src/vm/vm_init.rs:1396-1401` refuses compressed oops on any non-generational
backend and **prints to stderr, not just `tracing`**, with the reason stated in
the comment: "a silent fallback … would look identical to a successful run apart
from the footprint, and this gate is experimental enough that the operator must
see which one they got" (`:1403-1407`). `gc/src/compressed_oops.rs:144-146`
calls the same check "load-bearing and must not be relaxed".

The ZGC equivalent has to be a **refusal, not a warning**, because unlike
compressed oops the degraded mode is not "slower but correct" — it is a UAF.
Concretely:

* **ZGC + relocation + JIT, before stage (a) lands** → refuse at VM init.
  Either disable the JIT for the run (with a stderr line), or refuse to enable
  relocation. Refusing relocation is the better default: it degrades ZGC to
  what it already is today (a non-moving mark-sweep, `zgc-production-implementation-plan.md:39-45`)
  rather than degrading the whole VM to the interpreter.
* **ZGC + compressed oops** → already refused today by the same gate
  (`vm_init.rs:1397`), and the slot-representation study recommends hardening it
  with an assertion at heap construction because `narrow_oops_enabled()` is a
  process-global `AtomicBool` anything can set (`zgc-reference-slot-representation.md:551-554`).
  That recommendation applies to this workstream too, because the JIT reads the
  same flag at emission time (`jit/src/x64/licm.rs:284-286`).

**Open question:** should the refusal be at `vm_init` (where the compressed-oops
gate lives) or at JIT *compile* time (where `ir_lower.rs:4733-4741` already
refuses `ArrayStore(Ref)` for a structurally identical reason — "the IR tier
emits no store barrier")? A compile-time refusal is finer-grained and fails
closed per method; an init-time refusal is one line and cannot be forgotten by a
new emitter. §5 recommends **both**, for the reason §6 gives.

---

## 2. Site inventory

Every count below is reproducible with the stated pattern. Counts are against
`dev` at `59f219757`; `jit/src/x64/tests.rs` is excluded from the barrier-site
counts because it emits no production code.

### 2.1 The two lowering arms, confirmed

The framing's "two lowering arms per reference load" is correct and is the
right axis to plan against, but there are **four** inline arms across two tiers,
not one:

| tier | file:line | arm | what it emits for a reference |
|---|---|---|---|
| baseline | `jit/src/x64/bytecode_walk.rs:3937-3949` | scalar-replaced getfield | `emit_load_local(RAX, field_off)` — a **frame slot**, not the heap |
| baseline | `jit/src/x64/bytecode_walk.rs:4034-4036` | compact inline getfield | `MOV RAX, [RAX + cell_off]` (8-byte raw pointer at the cell base) |
| baseline | `jit/src/x64/bytecode_walk.rs:4077-4082` | legacy-cell arm of the same site | `MOV RAX, [RAX + legacy_cell_off + PAYLOAD64]` |
| baseline | `jit/src/x64/bytecode_walk.rs:4236-4240` | legacy-only inline getfield | same shape, separate opcode sub-arm |
| optimizing | `jit/src/ir_lower.rs:2287` `emit_inline_compact_getfield` | compact inline getfield | its own guard chain (`:2328-2366`) and its own raw load |

Plus the helper-CALL arms, which are the barrier's cheap escape hatch:

| Pattern | Scope | Count | Sites |
|---|---|---|---|
| `self\.helpers\.getfield` | `jit/src` | **5** | `bytecode_walk.rs:4135` (compact-arm slow path), `:4280`, `:4312`, `:4336`; `inlining.rs:1107` |
| `self\.helpers\.getstatic` | `jit/src` | **2** | `bytecode_walk.rs:3830`, `inlining.rs:1251` |
| `self\.getfield` (IR tier) | `jit/src/ir_lower.rs` | — | `:2396` (inline arm's slow path), `:4494-4495` |

The claim in the framing that the inliner has an inline arm is **wrong**: the
inlined-callee `0xb4` re-walk at `jit/src/x64/inlining.rs:1088` is
**helper-CALL only** (`:1107`), and bails out when the callee's field cannot be
resolved (`:1123-1127`). It is gated on `narrow_oops_block_inline_fields()` at
`:1162` for the *getstatic* neighbour only. That makes the inliner free under
stage (a).

### 2.2 Reproducible counts

```
# the getfield opcode, all tiers
rg -n '0xb4' jit/src --type rust                       -> 68 across 18 files (tests included)
                                                          1 real dispatch arm: x64/bytecode_walk.rs:3927
                                                          1 IR builder arm:    ir.rs:5575

# the aaload opcode
rg -n '0x32' jit/src --type rust                       -> 32 across 12 files
                                                          real emitters: x64/bytecode_walk.rs:1503 (arm)
                                                                         x64/arrays.rs:71, :96

# every caller of the raw reference-array element loader
rg -n 'emit_ref_aload_regs|emit_narrow_ref_aload_regs' jit/src --type rust
                                                       -> 7 non-test hits across 4 files
                                                          call sites: bytecode_walk.rs:696, :1511, :1804
                                                                      simd.rs:326

# every caller of the String.value loader
rg -n 'emit_load_string_value_ptr' jit/src --type rust  -> 12 across 3 files
                                                          9 call sites, all in bytecode_walk.rs
                                                          (:7328 :7381 :7587 :7593 :7736 :7744 :7875 :7976 :7984)

# inline getstatic, both tiers
rg -n 'try_emit_inline_getstatic|emit_inline_getstatic' jit/src --type rust
                                                       -> 10 across 5 files
                                                          defs: x64/objects.rs:347, ir_lower.rs:2190
                                                          call sites: bytecode_walk.rs:3816, inlining.rs:1241,
                                                                      ir_lower.rs:5117

# the operand-stack oop tag — the proxy for "this slot now holds a reference"
rg -n 'mark_top_as_oop\(\)' jit/src --type rust | rg -v '/tests\.rs'
                                                       -> 33 calls across 4 files
                                                          bytecode_walk.rs 27, inlining.rs 3,
                                                          objects.rs 2, deopt_stubs.rs 1

# the aarch64 spelling
rg -n 'mark_top_operand_as_oop' jit/src --type rust     -> 6, all aarch64_backend.rs; def at :960
                                                           is #[allow(dead_code)]

# the value-degrading plausibility filters on the helper path
# CORRECTED the first draft said "7 in this file". It is 6 here and
# 7 in the tree; the seventh is gc/src/heap.rs. See §2.5 for the full table.
rg -n 'plausible_heap_pointer\(raw\)' vm/src/jit/helpers.rs   (pre-change tree)
                                                       -> exactly 6, all degrading a LOADED VALUE to 0:
                                                             :4902 (aaload element)          [A]
                                                             :5208 (compact ref field)       [B]
                                                             :5237 (compact NON-ref arm)     [C] UNREACHABLE
                                                             :5262 (legacy 16-byte Value)    [D]
                                                             :6120 (getstatic System.in)     [E]
                                                             :6142 (getstatic general)       [F]
rg -n 'plausible_heap_pointer' vm/src/jit/helpers.rs    -> 40 hits; the other ~19 gate a RECEIVER
                                                           (`obj_ptr`/`array_ptr`), not a loaded value

# git blame, all six lines, pre-change tree
git blame -L <each> HEAD -- vm/src/jit/helpers.rs
                                                       -> 6/6 = 6a04b0e3c173e5cb4f47287b013fe302a71c0d77
                                                          (2026-06-29) — one commit, one idiom

# the SEVENTH, differently-shaped filter the first draft missed
rg -n 'fn forward_jit_arg_at' vm/src/jit/helpers.rs     -> :461; the hand-rolled test is at :468
                                                          `raw >= 1<<48` -> RETURN (skip forwarding),
                                                          not degrade-to-null. See §2.5.5.

# the existing pointer-encoding kill switch this design copies
rg -n 'narrow_oops_block_inline_fields' jit/src --type rust
                                                       -> 5: def x64/licm.rs:284;
                                                          call sites ir_lower.rs:2304,
                                                          x64/inlining.rs:1162,
                                                          x64/bytecode_walk.rs:3953, :4432
```

### 2.3 The barrier sites, by category

**Category A — raw heap reference loads that need an emitted barrier (x64 only):**

| file:line | tier | opcode / origin |
|---|---|---|
| `jit/src/x64/bytecode_walk.rs:4036` | baseline | `getfield`, compact arm |
| `jit/src/x64/bytecode_walk.rs:4078-4082` | baseline | `getfield`, legacy arm of the compact site |
| `jit/src/x64/bytecode_walk.rs:4238` | baseline | `getfield`, legacy-only site |
| `jit/src/x64/arrays.rs:76-82` (`emit_ref_aload_regs`) | baseline | `aaload`, reached from `bytecode_walk.rs:1511`, `:696` (LICM hoist), `:1804` (aastore SATB pre-read), `simd.rs:326` |
| `jit/src/x64/objects.rs:222`, `:226` (`emit_load_string_value_ptr`) | baseline | String intrinsics, 9 call sites |
| `jit/src/x64/objects.rs:368-373` (`try_emit_inline_getstatic`) | baseline | `getstatic` of `L`/`[` |
| `jit/src/ir_lower.rs:2287` body | optimizing | `getfield`, compact inline |
| `jit/src/ir_lower.rs:5957-5972` (`emit_gpr_array_elem_load`, `MemKind::Ref`) | optimizing | `aaload` |
| `jit/src/ir_lower.rs:2243` (wide arm of `emit_inline_getstatic`) | optimizing | `getstatic` of `L`/`[` |

**Total: 9 distinct emission points**, reached from ~20 call sites.

**Category B — helper-CALL arms; barrier lands in Rust, but see §2.5:**
`bytecode_walk.rs:4135`, `:4280`, `:4312`, `:4336`, `:3830`; `inlining.rs:1107`,
`:1251`; `ir_lower.rs:2396`, `:4494`, `:5128`. **10 sites.**

**Category C — produces an oop but needs no load barrier:**

* `aload*` / local reads (`bytecode_walk.rs:1391`, `:1483`) — reads a *frame
  slot*, which was written from an already-barriered value. No heap access.
* Scalar-replaced `getfield` (`bytecode_walk.rs:3940`) — same; the object was
  exploded into frame slots, so `emit_load_local` reads a slot, not the heap
  (`:3942-3949` says so explicitly). Correct **provided the slot was written
  from a barriered value**, which is a stage-(a) assertion, not an assumption.
* `new` / `newarray` / `anewarray` / `multianewarray` inline TLAB paths — a
  freshly allocated object carries the allocating color by construction
  (`gc/src/zgc/vaddr.rs:786` `allocation_color`).
* `aconst_null`, `dup*`, `checkcast` — stack shuffles, no load.
* `ldc` interned `String` — comes back from a helper.
* Object-returning `invoke*` return values — the callee produced them.

### 2.4 aarch64: nothing to do

`jit/src/aarch64_backend.rs:44-62` enumerates 39 opcodes with **no lowering at
all**, and the list is the entire object model: `0x2e..=0x35` (every array load,
`aaload` included), `0x4f..=0x56` (every array store), `0xb2`/`0xb3`/`0xb4`/`0xb5`
(getstatic/putstatic/getfield/putfield), every `invoke*`, `new`, `newarray`,
`anewarray`, `arraylength`, `athrow`, `checkcast`, `instanceof`, the monitors.
`0xb8 invokestatic` has an arm but `emit_invoke` unconditionally fails
(`:59-62`), so no method containing any call compiles.

`jit/src/aarch64_backend.rs:6614-6640` is a **test that asserts this**
(`object_model_opcodes_are_all_unsupported`) and tells you to update the header
table if you implement one. The only oops the backend ever produces are
`aconst_null` and `aload*` (`:1784`, `:2263`, `:2268`), and its
`mark_top_operand_as_oop` is `#[allow(dead_code)]` with zero call sites
(`:959-960`).

**Consequence:** stage (c) is a one-line `debug_assert` plus a test that the
refusal list still contains `0xb4`/`0x32`/`0xb2`. If someone later implements
the aarch64 object model, *that* change owns the barrier, and the existing
coverage test is the tripwire.

### 2.5 The helper arm is not free: the plausibility filter must go first

*Rewritten 2026-08-07 after an implementation agent worked this section. The
first draft counted seven sites in this file, treated them as interchangeable,
and called them "correct today". All three were wrong; see §9a.*

`jit_getfield`'s compact-reference arm, the canonical instance:

```rust
// vm/src/jit/helpers.rs:5208-5212 (pre-change) / :5367-5368 (working tree)
let raw = read_ref_slot(ptr);
return if cratonvm_types::plausible_heap_pointer(raw) {
    raw as i64
} else {
    0
};
```

A colored word has bit 63 set by construction (`gc/src/zgc/vaddr.rs:257-264`,
`Z_COLORED_TAG` at `:264`), so `plausible_heap_pointer` rejects it and the
helper hands compiled code a **silent null for a live object**.

#### 2.5.1 The inventory — six here, seven in the tree, and they are not alike

`rg -n 'plausible_heap_pointer\(raw\)' vm/src/jit/helpers.rs` returns **exactly
six**. The seventh instance of the idiom is `read_prim_element`'s Reference arm
at `gc/src/heap.rs:1710-1714` (function at `:1659`) — a **different crate and a
different workstream**, which the slot-representation study already names "the
most dangerous unmigrated read in the tree". Counting it in this file's total
inflated the stage-(a) estimate.

The six are **not interchangeable**. **Two** read a raw slot word and take the
barrier where they stand. **Three** operate on an `ObjectRef` that upstream code
has *already fabricated*, and for those the barrier belongs at the upstream read,
not at the filter. **One** is unreachable:

| | site | pre-change | working tree | what it holds when the filter runs | where the barrier belongs |
|---|---|---|---|---|---|
| **A** | `jit_aaload`, element read | `:4902-4906` | `:5047-5058` | **raw slot word** — `read_ref_slot(elem_ptr)` (`:5057`) | **here**, between the slot read and the filter |
| **B** | `jit_getfield`, compact-reference arm | `:5208-5212` | `:5354-5368` | **raw slot word** — `read_ref_slot(ptr)` (`:5367`) | **here** |
| **C** | `jit_getfield`, compact **non**-reference arm | `:5237` | `:5393-5404` | — | **nowhere: unreachable**, §2.5.2 |
| **D** | `jit_getfield`, legacy 16-byte `Value` slot | `:5262` | `:5424-5436` | `r.as_ptr()` off a `Value` already built by `read_value_atomic` (`:5417`) | at the **slot read** (`:5417`), before the `Value` is reconstituted |
| **E** | `jit_getstatic`, `System.in` | `:6120` | `:6289-6295` | `r.as_ptr()` off `crate::vm::get_static_shared` (`:6286`) | **inside `get_static_shared`** (`vm/src/vm/vm_object.rs:1424`) |
| **F** | `jit_getstatic`, general object arm | `:6142` | `:6312-6321` | `r.as_ptr()` off `get_static_shared` (`:6304`) | **inside `get_static_shared`** |

For **D/E/F** a barrier at the filter would be processing a word that has
already been laundered through an `ObjectRef` — which is precisely the mistake
`gc/src/zgc/vaddr.rs:522` `debug_assert_plain_word` exists to catch ("this word
is about to be treated as a machine pointer, so it had better not be a colored
word"). Putting the barrier upstream also **merges** those three with the
interpreter's own path rather than duplicating it, since `get_static_shared` is
shared. That is a different, larger, and better-placed edit than "fix six
filters", and it is why the staged plan in §5 no longer describes stage (a) as
six filter edits.

#### 2.5.2 Site C cannot fire — it is not a barrier site

Control reaches the `match val` at `helpers.rs:5383` (working tree) **only when
`storage.is_reference()` was false**, because the reference arm `return`s at
`:5368`. `is_reference()` is `matches!(self, Self::Reference)`
(`types/src/field_layout.rs:82`). And `read_compact_field`
(`types/src/field_layout.rs:972`) constructs `Value::Object` **only** in its
`FieldStorageKind::Reference` arm (`:978-993`); every other kind yields
`Int`/`Long`/`Float`/`Double` (`:994-1017`). So the `Value::Object(Some(r))`
arm at `:5388` is dead by construction, kept only to make the `match` total.

**It has never fired and cannot fire.** It is not a barrier site, and counting
it was the second source of inflation in the stage-(a) estimate. It should carry
an `unreachable`-naming comment rather than a barrier.

#### 2.5.3 They are heuristics over an *unfixed* defect, not integrity guards

`git blame` puts all six lines in **one** commit —
`6a04b0e3c173e5cb4f47287b013fe302a71c0d77` (2026-06-29, *"fix(gc/jit): degrade
stale references to null at every decode boundary; +IBM850"*). The first draft
said these filters "are correct *today*". That understates their status, and the
commit's own closing paragraph is the evidence:

> The underlying GC root-coverage gap (live blocked-thread frame objects swept
> by the non-moving young sweep) remains and is the only complete fix for the
> JIT intrinsic paths that have no single decode chokepoint.

So they are **defense-in-depth for a known-live bug that is still open**, not a
settled invariant check on trusted data. Two consequences for this workstream:

* The population they exist to catch (the `0x8D8D..` / `": contex"` class of
  stale reference the commit message describes) is **still arriving**. Removing
  or relaxing them is therefore not a cleanup; whatever replaces them must keep
  the stale-bits arm.
* A ZGC colored word and a stale-bits word must be **distinguished**, not
  merged. `gc/src/zgc/vaddr.rs:507` `is_well_formed` is the discriminator that
  already exists: it requires bits 62-46 clear and exactly one metadata bit set,
  so `0x8D8D8D8D8D8D8D8D` has bit 63 set but fails it. Colored → invariant
  violation (fail loudly, the barrier was missed); stale bits → keep the
  existing degrade-to-null contract.

Under stage (a) each **live** site must become "barrier first, then
plausibility-check the **unmasked** address" — the barrier's output is a bare
offset (`gc/src/zgc/barrier.rs`, `classify_*`'s address-domain contract; the
first draft's `:600-606` has drifted), and `base + offset` is what
`plausible_heap_pointer` should be asked about.

#### 2.5.4 The filters feed no counter — a live bug, independent of ZGC

The interpreter's twin of this filter feeds a process-wide counter and a
one-shot stderr line: `note_object_degradation`
(`types/src/compact_value.rs:330`), read through `object_degradation_count()`
(`:302`), with six interpreter-side call sites in that file
(`compact_value.rs:907`, `:1046`, `:1153`, `:1196`, `:1206`, `:1420`) plus the
`value.rs` SoA decode path the `pub(crate)` comment at `:325-327` names as the
reason for that visibility.

**All six JIT arms fed nothing.** So `object_degradation_count()` reads `0`
while compiled code is handing Java `null` for live objects — and it reads `0`
for precisely the configuration where the root-coverage gap of §2.5.3 is *most*
likely to fire, because JIT'd frames are exactly the ones a deposited root
snapshot misses. **The instrument built to catch this bug class is blind in the
configuration the bug prefers.** That is a defect today, on the default
collector, with no ZGC involved.

A `JIT_REF_DEGRADATIONS` counter plus a `jit_ref_degradation_count()` reader has
been added on the JIT side (working tree, `helpers.rs:4928`, `:4935`). It is a
*second* number, not the same one: unifying them needs
`note_object_degradation` widened from `pub(crate)` to `pub`
(`types/src/compact_value.rs:330`) and called from the JIT arm. **That one-word
visibility change is the remaining work**, and it is in a file this workstream
does not own.

#### 2.5.5 A seventh filter *in this file*, differently shaped: `forward_jit_arg_at`

(Not to be confused with the seventh instance of the *idiom*, which is
`gc/src/heap.rs`'s. This one is a different shape entirely and is why it hid.)

`forward_jit_arg_at` (`vm/src/jit/helpers.rs:461`, test at `:468`) hand-rolls
its own plausibility check rather than calling `plausible_heap_pointer`:

```rust
// vm/src/jit/helpers.rs:468
if raw == 0 || (raw as u64 & 0x7) != 0 || (raw as u64) >= (1u64 << 48) {
    return;
}
```

It degrades to **"skip forwarding"**, not to null — so it does not appear in any
`plausible_heap_pointer` grep and the first draft missed it. Under ZGC a colored
word trips the `>= 1<<48` arm and the argument would silently **not be
forwarded**, i.e. the barrier is skipped on that argument with no diagnostic.

It was deliberately left alone, and the reason is worth recording: JIT arguments
arrive **already barriered** (they came from a caller's barriered load or a
fresh allocation), so no colored word should reach it. That is an *assumption*,
not a proof, and it is the same class of assumption as J2. Listed here so a
future ZGC pass does not have to rediscover it. It is also the natural place to
put the ZGC barrier dispatch for D/E/F, since it is already this file's
`load_and_forward` chokepoint for moving collectors (`:475`).

#### 2.5.6 Net effect on stage (a)

Was: "the seven degrade sites are the work." Now: **six sites, of which one is
unreachable, two (A/B) take the barrier in place, and three (D/E/F) move the
barrier to `read_value_atomic`'s slot read and to `crate::vm::get_static_shared`
(`vm/src/vm/vm_object.rs:1424`)**, plus the counter unification of §2.5.4 and
the `forward_jit_arg_at` assumption of §2.5.5. See §5 for what that does to the
estimate — the band does not move, but its composition does.

### 2.6 Slot width: the study's claim survives

Verified. The JIT branches per object on `GC_FLAG_COMPACT` at every inline field
access (`bytecode_walk.rs:4026-4032`, `:4222-4234`; `objects.rs:203-208`,
`:290-295`) and each arm issues **one aligned 8-byte access**
(`:4034-4036` compact, `:4076-4082` legacy). `arrays.rs:76-82` is one aligned
8-byte `MOV`. `objects.rs:368-373` is one aligned 8-byte `MOV` of the
`FIELD_CELL_PAYLOAD64` word. An aligned 8-byte `MOV` is single-copy-atomic on
both supported targets.

**Zero JIT sites need changing for slot width.** The barrier slots in *after*
the existing address computation in each arm, with no change to the addressing.

---

## 3. The proposed barrier sequence

### 3.1 x64 fast path — 6-7 instructions, not 3-4

The framing's target of 3-4 instructions assumes HotSpot's multi-mapped address
space, where a good-colored word *is* a dereferenceable address. CratonVM's
`vaddr` is **single-mapped**: `uncolor_unchecked(w) == base + (w & Z_OFFSET_MASK)`
(`gc/src/zgc/vaddr.rs:1034-1036`), and its own comment says the `AND`+`ADD` is
"precisely the cost that OS multi-mapping would eliminate" (`:1023-1024`).

Two consequences the pseudo-assembly has to carry:

1. The unmask is a real `AND` plus a real `ADD`.
2. **Null needs a branch after all.** `Z_NULL` is the all-zero word
   (`vaddr.rs:276-279`), and it passes the bad-mask test for free
   (`barrier.rs:612-629`) — but `base + (0 & mask) == base`, which is a live
   heap address, not null. The Rust barrier ducks this by returning an *offset*
   and documenting that `Good(0)` "is ambiguous between null and an object at
   heap offset 0" (`barrier.rs:600-606`); machine code cannot duck it, because
   the next instruction dereferences the result.

Proposed sequence, replacing one `MOV RAX, [<addr>]` at every Category-A site.
`RAX` = destination, `<addr>` = whatever addressing mode that arm already
computed, `R10`/`R11` = scratch:

```asm
    ; --- fast path: 6 instructions, 1 not-taken branch, 0 memory beyond the load
    mov   rax, [<addr>]           ; 1. the load the arm already emitted, unchanged
    test  rax, r13                ; 2. r13 = pinned bad_mask.  ZF=1  => good OR null
    jnz   .slow                   ; 3. not taken on the common path
    and   rax, r14                ; 4. r14 = pinned Z_OFFSET_MASK.  null -> 0
    jz    .done                   ; 5. AND sets ZF: null (and only null) is 0 here
    add   rax, r15                ; 6. r15 = pinned address-space base
.done:
    ; rax = live machine address, or 0 for null
```

Notes, each of which is a design commitment:

* **Step 5 is not optional and is not free.** `AND` sets ZF from its result, so
  the null test costs one `jz` and no compare — the same trick
  `emit_narrow_ref_aload_regs` already uses for the compressed-oop decode
  (`jit/src/x64/arrays.rs:102-103`: `SHL RAX,3` then `JZ +13`). **That emitter is
  the template for this one**, and it is already shipped and tested code.
  Note the ambiguity it inherits: an object at heap **offset 0** is
  indistinguishable from null after step 4. `vaddr`'s own
  `color_offset_roundtrip_many_offsets` asserts offset 0 is a legal non-null
  location (`barrier.rs:459-462`), so **the page allocator must never hand out
  offset 0** — see the open question in §8.
* **Three pinned registers (`r13`/`r14`/`r15`) is a proposal, not a
  requirement.** The alternative is `mov r11, imm64` before each use, which
  costs 10 bytes per site and is what `emit_narrow_ref_aload_regs` does today
  (`arrays.rs:104-106`). Pinning is better for a barrier on *every* reference
  load, but it takes three GPRs away from the allocator, and `bad_mask` changes
  at every phase flip (`vaddr.rs:721`, `:754`) so a pinned register must be
  reloaded at every safepoint — which is a mechanism that does not exist today.
  **Recommendation: ship (b) with `imm64` materialization, measure, and only
  then consider pinning.** The `imm64` form is 9 instructions / ~30 bytes.
* **Flags are clobbered.** `TEST`, `AND` and `ADD` all write EFLAGS. Every
  Category-A site must be audited for a live flag across it. `bytecode_walk.rs:4032`
  already emits a `JZ` off an `AND` immediately before the compact load, so the
  barrier lands *after* that branch, not across it — but the LICM hoist site
  (`:696-698`) sits inside a guard block whose flags are consumed at `:691-694`,
  and `simd.rs:326` is inside a vector loop. Those two need individual reading.
* **`RAX` is already the convention.** Every Category-A arm lands its result in
  RAX and then `push_from_rax()`s it, except the String intrinsics
  (`emit_load_string_value_ptr(dst, ...)` takes an arbitrary `dst`,
  `objects.rs:196-202`) and `simd.rs:326`. The barrier emitter must therefore be
  `dst`-parameterised, not RAX-hardcoded.

### 3.2 x64 slow path — the branch-out/branch-back convention already exists

`.slow` must call `load_barrier_slow` (`gc/src/zgc/barrier.rs:858`), which needs
`(slot_address, observed_word, ctx, kind)` and returns the corrected bare
address. Three properties rule out open-coding it at the site:

1. It is `#[inline(never)]` precisely so it stays out of the caller's
   instruction stream (`barrier.rs:855-856`).
2. It needs the **slot address**, which several arms no longer have in a
   register after the load (the compact arm's `MOV RAX, [RAX + cell_off]`
   clobbers the base with the value, `bytecode_walk.rs:4036`).
3. It clobbers every caller-saved register, mid-expression.

**There is a mature convention for exactly this, and it is not the deopt-stub
one.** The tree has two stub flavours and only one of them returns:

* **Pattern (a) — same-body branch-out / branch-back.** The fast path collects a
  `Vec<usize>` of `rel32` patch offsets, falls through to `JMP done`, and the
  slow arm is laid down immediately after, staging `ARG_REGS` and calling the
  helper before joining at `done`. Reference implementations:
  `emit_inline_body_compact_ref_putfield` (`jit/src/x64/objects.rs:479`), the
  guard emitters `emit_guarded_getfield_receiver_check` (`objects.rs:401`) and
  `emit_trusted_oop_receiver_check` (`objects.rs:442`), the helper-call tail
  `emit_ref_putfield_helper_call` (`objects.rs:456`), and the complete
  fast/slow/join at `jit/src/x64/bytecode_walk.rs:4265-4300`. The inline TLAB
  allocator uses the same shape (`jit/src/x64/emit.rs:1252-1271`).
* **Pattern (b) — a deferred stub block emitted after the whole method body**
  (`jit/src/x64/deopt_stubs.rs:737`, `:812`, `:1076`, `:1141`; invoked in fixed
  order at `bytecode_walk.rs:10641-10653`). **These never return to the fast
  path** — they load `i64::MIN` and run the epilogue. There is currently no
  deferred stub that branches back into the body.

**Use pattern (a).** It is the shape every barriered fast path in this backend
already has, and the primitives (`emit_jcc_rel32_patch`, `patch_rel32_to_here`,
`helper_call_patches`) are all in place. Physically hoisting the slow arm out of
line would mean adding a *third* stub flavour — a deferred list plus a
return-branch patch — for an I-cache locality argument nobody has measured here.
**That reduces stage (b)'s net-new machinery to the barrier emitter and the
map placement**, which is why (b) is weeks and not months.

Register discipline at the slow arm:

* `flush_scratch_registers()` (`jit/src/x64/operand_stack.rs:666`) is what the
  guarded getfield arms already call before their helper call
  (`bytecode_walk.rs:4003`, `:4198`) because the slow path CALLs out. It moves
  every `StackSlot::Scratch(reg)` and every low `StackSlot::Xmm` into a fresh
  frame slot and rewrites `self.stack`. The barrier must do the same, or be
  emitted only at sites that have already flushed. **The former, so the barrier
  is site-agnostic.**
* `SCRATCH_REGS = [R8, R9]` (`jit/src/x64.rs:267`); `ARG_REGS` is
  `[RCX, RDX, R8, R9]` on Windows and `[RDI, RSI, RDX, RCX, R8, R9]` on SysV
  (`jit/src/x64/reg_encoding.rs:90`, `:93`); `R11` is reserved as a free scratch
  and is never a local home or an arg register (`jit/src/x64/safepoint.rs:279-282`).
  Any register the barrier pins (§3.1) must be outside all of those — R13-R15
  are. Windows' 32-byte shadow space is budgeted once in the frame
  (`jit/src/x64.rs:2043`, `:2191`), not per call, so a barrier call inherits it.
* `emit_call_absolute` (`jit/src/x64/emit.rs:731`) picks `E8 rel32` when the
  helper is within ±2 GB and records the patch offset for the unroll duplicator
  (`:745-753`, list at `x64.rs:641`, re-resolution at
  `bytecode_walk.rs:3133-3358`). **A barrier call inside an unrolled loop body
  inherits that machinery for free** — the alternative failure is the "N-Body
  `Body.x` SIGSEGV pattern" the comment names.

### 3.3 The new helper slot — stage (b) only

Stage (a) needs **no** new helper: the barrier goes *inside* the existing
`jit_getfield` / `jit_aaload` / `jit_getstatic` bodies. Stage (b)'s inline fast
path does, because its slow arm must call `load_barrier_slow` directly, and it
reaches it through a new `JitRuntimeHelpers` slot. That struct is `#[repr(C)]`, append-only, and const-asserted
(`jit-api/src/lib.rs:707`, contract at `:679-704`). Adding one field requires
updating **five** places or the build breaks: the struct, the
`helper_fn_slots!` table (`jit-api/src/helpers_abi.rs:~564`), the required-list
(`:~735`), the offset table (`:~933`), and the `size == 63*8` const assertion
(`:826`) plus `JIT_HELPERS_ABI_VERSION`.

Two traps:

* **`jit/tests/*` build `JitRuntimeHelpers` with `..Default::default()`.** A
  *required* slot that is unconditionally `CALL`ed and defaults to `0` is a call
  through a null pointer — the rustdoc says so at `jit-api/src/lib.rs:669-676`.
  The barrier slot must be **optional**, with a zero address meaning "the gate
  is on, take the helper arm" — the same shape `self.getfield == 0` already has
  at `ir_lower.rs:2295` and `:4490`.
* **Every helper must call `note_jit_boundary()` first**
  (`vm/src/jit/helpers.rs:5160` pre-change / `:5312` working tree, in
  `jit_getfield`; 59 such calls in the file), which invalidates the per-thread
  conservative-roots JIT-scan cache. A barrier helper that skips it leaves that
  cache stale across a Rust↔JIT boundary.

### 3.4 aarch64

Nothing to emit (§2.4), and it could not participate even if there were:
`Arm64Backend::emit_oop_map_for_safepoint` (`jit/src/aarch64_backend.rs:1006`)
is **fail-closed** — it sets `self.failed = true` at `:1012` and the map-building
body below it is dead code. Any aarch64 method needing an oop map is refused.

If the aarch64 object model is ever implemented, the sequence is the direct
translation, and aarch64 is *cheaper* than x64 here:

```asm
    ldr   x0, [<addr>]
    tst   x0, x13                 ; bad_mask
    b.ne  .slow
    and   x0, x0, x14             ; offset mask; does NOT set flags (use ANDS)
    cbz   x0, .done               ; null stays 0 — one instruction, no flags needed
    add   x0, x0, x15             ; base
.done:
```

`CBZ` removes the flag dependency of step 5 entirely, and aarch64 has more
spare callee-saved registers for the three pinned constants. The barrier is
**not** the reason aarch64 is behind.

---

## 4. GC-map, safepoint and deopt correctness

### 4.1 The invariant the new call site breaks

`jit/src/ir_lower.rs:3491-3492` states it in one sentence:

> `jit_getfield` reaches no safepoint, so nothing has moved and no oop needs
> copying back.

That is why the existing helper-CALL arms emit **no** safepoint map, do no
post-call frame republication, and use the lighter `emit_helper_sentinel_check`
(`ir_lower.rs:3504`) rather than `emit_call_return_check` (`:5045`).

A ZGC barrier slow path **does not preserve that invariant**, because
`ZBarrierContext::forward` "either finds the existing forwarding entry **or
performs the copy** and installs one" (`gc/src/zgc/barrier.rs:452-454`). A copy
allocates in a to-space page. Allocation can trigger a collection. Therefore:

**Every barrier slow-path call site is GC-capable, and every live reference in
the frame must be described by a stack map at it.**

### 4.2 The two-part call-site protocol, and what a leaf call skips

`jit/src/x64/safepoint.rs` owns this. A GC-capable call site is a **bracket**,
and the two halves are not independent:

```
  emit_pre_safepoint_spill()        safepoint.rs:30 (decl) / :41 (impl)
      flush scratch regs under moving-young          :103-105
      pending_live_frame_hi = next_spill_offset      :112
      store every register-resident local to [rbp-off]  :113-118
      blind-spill the whole GPR file                 :131-153 (emit_blind_reg_spill :192)
      store cur_bc_pc into [rbp - sp_id_slot_off]; insert into safepoint_pcs  :161-175
      emit_shadow_push()                             :180
  <the CALL>
  emit_oop_map_for_safepoint()      safepoint.rs:945
      reload shadow slots                            :956
      assert stack.len() == stack_oop_marks.len()    :969-983
      consume pending_live_frame_hi                  :989
      native_pc = buf.pos()                          :990
      collect oop-marked operand slots               :993-1002
      + local_oop_masks bits                         :1012-1030
      push OopMapEntry                               :1044
      emit_post_safepoint_reload (precise maps)      :1064-1066 (impl :1079)
```

The canonical template is the `emit_safepoint_poll` rustdoc
(`safepoint.rs:264-306`).

**Two facts that change the design:**

1. **There is no register oop map.** `OopMapEntry` (`jit/src/lib.rs:1208`) is
   `{ native_pc_offset, bytecode_pc, frame_slot_offsets: Vec<i16>,
   moving_young_coverage_complete, live_frame_hi }` — frame slots only. The
   `reg_oops` bitmap is still an unimplemented TODO
   (`jit/src/regalloc.rs:1493-1500`). What makes a register-resident oop visible
   is the **blind full-GPR spill** at `safepoint.rs:131-153`, and that is a
   GC-visibility measure, not an ABI one. This is the mechanism that answers J3
   (§7): the in-flight loaded reference becomes visible because the pre-safepoint
   spill puts every GPR in the frame band — *provided the bracket is used*.
2. **Half a bracket is worse than none.** `pending_live_frame_hi` is `mem::take`n
   at `:989`, so a spill without a matching map leaves `0` = "unknown", forces a
   conservative frame scan, **and** breaks whole-method coverage:
   `cm.fully_oop_covered` (`jit/src/x64/driver.rs:2157-2162`) requires
   `safepoint_pcs ⊆ mapped_safepoint_pcs`, and the spill is what inserts into
   `safepoint_pcs`.

**What the existing `jit_getfield` call sites do instead:**
`bytecode_walk.rs:4280`, `:4312`, `:4336` and `inlining.rs:1107` call **only**
`flush_scratch_registers()`. No pre-safepoint spill, no sp-id store, no map.
`getfield` is treated as a leaf helper, and `ir_lower.rs:3491-3492` is the
statement of why.

**So the decision reduces to one question — can the barrier slow path allocate
or block?** If no, the barrier is a leaf helper and copies `jit_getfield`
exactly: `flush_scratch_registers()`, stage args, call, done. If yes, it needs
the full bracket at all 9 sites and §4.3's contract applies. **This is open
question 4 in §8, and it is worth several weeks of the estimate.**

### 4.3 What the map must say, if the bracket is needed

The contract is written down at `jit/src/ir_lower.rs:1445-1464` ("Moving-young
relocation contract"), and it is four conditions:

1. a non-zero `sp_id_slot_off` with the active safepoint's id stored there, so
   the exact map is found and never a union;
2. an `OopMapEntry` whose `bytecode_pc` equals that id and whose
   `moving_young_coverage_complete` is set — **"No matching entry is a refusal,
   not an absence of objections"** (`:1453-1454`);
3. `frame_slot_offsets` naming every frame slot holding a live reference, so
   each can be rewritten in place;
4. a `frame_layout` + `live_frame_hi` precise enough to separate resumable
   storage from dead spill and outgoing-argument scratch.

`emit_safepoint_map` (`ir_lower.rs:1480`) implements this and is **fail-closed by
construction**: it always stores an id and always records a map, because
"skipping either would leave the slot holding the id of an *earlier* safepoint,
and the collector would then match a map describing a different program point
and relocate against it" (`:1473-1479`). A slot it cannot describe sets
`coverable = false` and publishes an incomplete map, which diverts that cycle to
the non-moving sweep.

**Three specific obligations for the barrier stub:**

* It must call `emit_safepoint_map` **before** the stub call and **before**
  allocating the destination slot of the load — the result slot is not written
  until the barrier returns, "so publishing it as a live reference beforehand
  would hand the collector uninitialised memory" (`ir_lower.rs:1468-1472`).
* Reference **parameter homes** must be in the map. `emit_safepoint_map`
  seeds `slots` from `self.ref_param_homes` (`:1501`) for a measured reason:
  omitting them "is not a partial claim, it is a false one", and the two words
  that failed the `IrEscapeProbe` band scan were a parameter home and its node
  copy holding the same reference (`:1495-1500`).
* On the **baseline** tier, the equivalent is the `stack_oop_marks` mechanism
  behind `mark_top_as_oop()`, and the barrier is emitted *between* the load and
  the `push_from_rax()`/`mark_top_as_oop()` pair — so at the slow-arm call the
  loaded value is in a register and **not yet marked**. The blind GPR spill
  (`safepoint.rs:131-153`) is what covers it, which is another reason the
  bracket must be used whole rather than approximated.
* Never grow `self.stack` outside `stack_push`/`push_stack`, or the
  `stack.len() == stack_oop_marks.len()` lockstep assertion at
  `safepoint.rs:969-983` trips and precision silently degrades.
* Set `self.cur_bc_pc` correctly before the bracket — it is the map key on both
  sides, and colliding two distinct safepoints on one bci is a documented hazard
  (`safepoint.rs:308-324`). A barrier at a `getfield` shares its bci with the
  `getfield`'s own guards, which is exactly that collision.

**Precision is additive over a conservative backstop, except under moving-young.**
`vm/src/jit/conservative_roots.rs` still sweeps the frame band, and
`scan_one_frame_precise` adds the register-invisible roots on top; a missing or
empty map degrades precision without being unsound *on the non-moving sweep*.
Under a relocating collector it is unsound, which is the whole reason 1c exists.
See `docs/feature-designs/precise-jit-maps-default.md:149-157`, which also
records that the relocation walker `remap_active_jit_frames` is **implemented
but inert** on the default path — ZGC relocation is what would activate it, and
it has never run in anger. (`CRATONVM_SHADOW_STACK` is superseded and must not
be combined with precise maps, `:140-141`.)

### 4.4 Deopt

`emit_helper_sentinel_check` (`ir_lower.rs:3504-3512`) and its baseline twin
`emit_post_invoke_exception_check` (`jit/src/x64/deopt_stubs.rs:951`) exist for
helpers that can return the `i64::MIN` sentinel. **The barrier needs neither.**
`load_barrier_slow` cannot fail — a broken forwarding table produces a
`tracing::warn!` and the stale address, not an error return
(`barrier.rs:895-901`) — so there is no sentinel, no exception check, no deopt
point, and no `throw_bci` to bake. That is one thing genuinely simpler than the
existing helper arm.

Two constraints survive anyway, because the barrier is emitted *inside* arms
that do deopt:

* Snapshot builders (`emit_deopt_snapshot_at_guard` /
  `build_and_record_deopt_point`) must be called **after**
  `flush_scratch_registers()` and **before** any `pop_stack()` for the operation
  (`deopt_stubs.rs:44-51`). Inserting a barrier that flushes changes where that
  boundary is at every site it touches.
* `DeoptVerifier` rejects an artifact whose frame cannot be rebuilt
  byte-for-byte, and `Undefined` must never be conflated with
  `MaterializationRequired` (`jit/src/deopt.rs:1-37`). A barrier that leaves a
  reference live in a register the frame description does not name is exactly
  that conflation.

---

## 5. The staged plan

### Stage (a) — helper-arm barrier + inline arms disabled behind a gate

**Required before relocation may be enabled at all.**

*Scope.* Route every Category-A site to the helper by making a new gate
(`zgc_blocks_inline_ref_loads()`, §6) return `true` whenever ZGC is selected;
add the barrier inside the Rust helpers; **relocate or replace** the value-
degrading plausibility filters on the load path (§2.5); add a coverage
assertion.

**Corrected.** This paragraph used to read "remove the seven
value-degrading plausibility filters". Three things were wrong with that. (i)
There are **six** in `vm/src/jit/helpers.rs`, not seven (§2.5.1). (ii) One of
the six is **unreachable** and needs a comment, not a barrier (§2.5.2). (iii)
"Remove" is the wrong verb: the filters guard a **still-open** GC defect
(§2.5.3), so the stale-bits arm must survive; and for three of the six (D/E/F)
the barrier belongs **upstream of the filter**, at `read_value_atomic`'s slot
read and inside `crate::vm::get_static_shared`, because by the time those
filters run the word has already been fabricated into an `ObjectRef`. Only
**A and B** take the barrier where the filter is.

*Files touched.* `jit/src/x64/licm.rs` (gate definition, beside
`narrow_oops_block_inline_fields`); `jit/src/x64/bytecode_walk.rs`,
`jit/src/x64/objects.rs`, `jit/src/x64/arrays.rs`, `jit/src/x64/simd.rs`,
`jit/src/ir_lower.rs` (gate consultation at the 9 Category-A sites);
`vm/src/jit/helpers.rs` (barrier at A/B; comment at C; dispatch for D/E/F);
**`vm/src/vm/vm_object.rs`** (`get_static_shared:1424` — the barrier for E/F;
*new to this list*, and shared with the interpreter, so **coordinate**);
**`types/src/compact_value.rs`** (`note_object_degradation:330` `pub(crate)` →
`pub`, the one-word counter unification of §2.5.4; *new to this list*);
`vm/src/vm/vm_init.rs` (the refusal gate, §1.3); `gc/src/heap.rs` (the
`read_prim_element` filter — **contended, coordinate**).

*Effort band.* **3-5 days — unchanged, but for changed reasons.** The first
draft put the work in "the seven filters". Dropping the unreachable site and the
mis-attributed `gc/src/heap.rs` one takes two off; moving D/E/F to
`get_static_shared` and the `read_value_atomic` slot read puts more back,
because `get_static_shared` is a **shared interpreter/JIT chokepoint** and a
barrier there has to satisfy both readers (and runs into open question 5, statics
not being atomic). Net: the band holds, its **centre of mass moves to the upper
half**, and the shape is "two in-place barriers, two upstream relocations, one
comment, one visibility change" rather than "seven filter edits". The gate itself
is still hours. The barriered helpers must each call
`note_jit_boundary()` first (`vm/src/jit/helpers.rs:5160` pre-change / `:5312`
working tree) — they already do, since the barrier goes *inside*
`jit_getfield`/`jit_aaload`/`jit_getstatic` rather than beside them, which is one
reason stage (a) needs no new helper slot at all. **This survives the D/E/F
relocation**: `get_static_shared` is reached *from* `jit_getstatic`, which has
already noted the boundary at its entry, so moving the barrier into it does not
open an unnoted Rust↔JIT crossing. Note that `emit_load_string_value_ptr`
(`objects.rs:196`) and `simd.rs:326` have **no helper fallback today** — they
are intrinsics with no slow path, so the gate for them means refusing the
intrinsic, exactly as the compressed-oops stopgap once did
(`gc/src/compressed_oops.rs:63-65`, "The stopgap that refused
`try_resolve_string_intrinsic` outright under narrow oops"). That precedent
exists and its cost is known.

*What it unblocks.* Phase 3b (concurrent relocation) may be enabled behind its
own sub-flag. Nothing else in the plan is gated on 1c.

*How it is tested.*
1. A **source-witness test** in `jit/src` asserting that each of the 9
   Category-A emission sites is guarded by the gate. Precedent:
   `aarch64_backend.rs:6614-6640` is exactly this shape, and
   `narrow_oops_block_inline_fields`'s own five call sites are the population a
   sibling test should have been guarding — `gc/src/compressed_oops.rs:50-58`
   records a real SIGSEGV from a site that was missed.
   Beware the two known traps: a witness test reads the working tree, and fixed
   line bands go stale.
2. A `--features zgc` unit test that a colored word round-trips through
   `jit_getfield` / `jit_aaload` / `jit_getstatic` to the correct machine
   address rather than to `0`.
3. A negative test that ZGC + relocation + JIT **refuses** at init.
4. **A degradation-counter test, on the default collector.** §2.5.4: a JIT
   read helper handed a stale word must move a counter. Today none does, so the
   existing `object_degradation_count()` assertion passes vacuously under JIT.
   This one is worth landing *before* the rest of stage (a), because it is a
   real defect on the shipping collector and it is what will tell you whether
   the root-coverage gap of `6a04b0e3c1` is live in a given run.

### Stage (b) — the inline fast path with a slow-path stub

*Scope.* §3.1's sequence at the 9 Category-A sites; the out-of-line stub
mechanism (§3.2); the safepoint-map obligations (§4.2); the baseline tier's
not-yet-marked-register problem (§4.2, third bullet).

*Files touched.* All of stage (a)'s JIT files, plus `jit/src/x64/emit.rs` (the
barrier emitter itself), `jit/src/x64/safepoint.rs` (only if the bracket is
needed — open question 4), `jit/src/ir_lower.rs` (`emit_safepoint_map`
placement), and `jit-api/src/{lib.rs,helpers_abi.rs}` (the five helper-slot
registration sites, §3.3).

*Effort band.* **3-5 weeks**, and the width of that band is almost entirely
open question 4. Drivers, roughly in order: the safepoint-bracket decision and,
if it is "yes", its placement at 9 sites; the per-site flag-liveness audit (the
LICM hoist at `bytecode_walk.rs:696-698` and `simd.rs:326` are the two that need
individual reading); `dst`-parameterising the emitter for the String intrinsics
and SIMD; the phase-flip invalidation of any pinned mask register; and the A/B
measurement that says whether the inline path is actually faster than stage
(a)'s helper. **If the barrier slow path turns out to be a leaf (no allocation,
no blocking), §4.2's bracket disappears and this is 2-3 weeks.** Note that the
stub convention is *not* a driver: pattern (a) already exists (§3.2), contrary
to the commissioning framing.

*What it unblocks.* Throughput. Nothing's correctness depends on it — that is
the whole point of staging it second.

*How it is tested.* Encoding tests beside the existing ones in
`jit/src/x64/tests.rs`; a differential run of the compiled and interpreted
answer for the same reference-heavy corpus, with HotSpot as the oracle rather
than the other CratonVM mode; and the Spring Boot 1975-class suite per
`zgc-production-implementation-plan.md` §5a, which is the only measurement in
this tree that has ever caught a JIT/GC interaction at scale.

### Stage (c) — aarch64

*Scope.* A `debug_assert` in the aarch64 backend that the ZGC gate is never
consulted, plus one line added to the `object_model_opcodes_are_all_unsupported`
coverage test (`aarch64_backend.rs:6614`) documenting that its refusal list is
now load-bearing for ZGC as well.

*Effort band.* **< 1 day.**

*What it unblocks.* Nothing. It is a tripwire for whoever implements the
aarch64 object model.

---

## 6. The `inline_card_mark_available()` lesson, applied

`jit/src/x64/objects.rs:29-45` returns a hardcoded `false`, and the comment is
worth reading as a decision record rather than an apology:

> The inline byte-store sequence is **locally sound**, but WildFly's real JIT
> boot audit observed an old `org/jboss/modules/Module` reference to a young
> child on a clean card. A clean card is a correctness failure … Until the
> direct emitter has **end-to-end coverage for every compiled store form and
> card-table lifecycle**, `jit_putfield_object` remains the single source of
> truth.

Three things generalize from it to the load barrier:

1. **"Locally sound" is not the bar.** The card-mark sequence was correct as an
   instruction sequence; what failed was *coverage* — some store form did not
   reach it. A load barrier has strictly more emission sites than a store
   barrier (9 Category-A sites vs. the store arms), and four of them are
   intrinsics and analysis passes that nobody thinks of as "a getfield". The
   failure mode is identical and more likely.
2. **The write side is still on the helper after a real bug, and it was found
   by a boot audit, not by a test.** The inline load barrier should expect the
   same: it will not be a unit test that finds the missed site.
3. **Published metadata must not select the fast path.** `objects.rs:41-43`:
   "Keep the published metadata in `JitRuntimeHelpers` for a future verified
   implementation; merely exposing it must not select the unsafe fast path." The
   ZGC equivalent: publishing `bad_mask` / `base` / `heal_color` addresses into
   `JitRuntimeHelpers` (which stage (a) needs anyway, for the helper) must not
   be what turns the inline emitter on.

### The kill switch

**Name: `zgc_blocks_inline_ref_loads()`**, defined beside
`narrow_oops_block_inline_fields()` in `jit/src/x64/licm.rs` (which is where the
JIT's pointer-encoding gates already live, `:284-286`).

*What it gates.* All 9 Category-A emission sites (§2.3). When it returns `true`,
each site either takes its existing helper-CALL arm or, for the intrinsics that
have none, declines to emit the intrinsic at all.

*Default.* `true` whenever ZGC is the selected backend, for the whole of stage
(a) and for however long stage (b) takes to earn its way off. Flipping it to
`false` is what "stage (b) shipped" means, and it is a **one-symbol revert** —
which is the property `inline_card_mark_available()` bought and then needed.

*Fallback path.* The `jit_getfield` / `jit_getstatic` / `jit_aaload` helpers,
barriered in Rust (stage (a)). Same shape as
`narrow_oops_block_inline_fields`'s fallback: "compressed oops disable the
inline path and `getfield`/`putfield` fall back to `jit_getfield` /
`jit_putfield_object`, which go through the width-aware `read_compact_field` /
`write_compact_field`" (`licm.rs:279-283`).

*Why a named predicate rather than reusing `narrow_oops_block_inline_fields`.*
The two gates disable overlapping but not identical site sets, they can be on
independently, and merging them would make a ZGC bug present as a compressed-oop
bug. `gc/src/compressed_oops.rs:148-151` makes the same distinction for the same
reason ("G1 and ZGC are NOT the same case, and this item used to blur them").

*Escape hatch.* An env override (`CRATONVM_ZGC_INLINE_REF_LOADS=1`) so the
inline path can be A/B'd on the Azure host without a rebuild, in the style of
`CRATONVM_JIT_INLINE_GETFIELD` (`bytecode_walk.rs:3996-4001`). Note the standing
hazard: **declared flags latch**, so an override read through a `OnceLock` must
not be set after the first JIT compile.

---

## 7. Risk register

| # | Risk | Presents as | Mitigation |
|---|---|---|---|
| **J1** | **A missed Category-A site.** The `inline_card_mark_available` failure, replayed. `emit_load_string_value_ptr` was already missed once by exactly this class of gate (`gc/src/compressed_oops.rs:50-58`: an ungated site → "a deterministic wild-pointer SIGSEGV on every inlined `charAt`/`length`/`indexOf`/`hashCode`/`equals`/`compareTo`"). | **Silent heap corruption** if the missed word is degraded to null (§1.1 case 2); a clean SIGSEGV if it is dereferenced. The first is worse. | The source-witness coverage test in stage (a).1. Bit 63 is the tripwire that converts case 2 into case 1 — **do not weaken `plausible_heap_pointer`'s 47-bit check to "fix" a colored-word rejection**; fix the caller. |
| **J1b** | **The degradation counter is blind under JIT** (added §2.5.4). All six `plausible_heap_pointer(raw)` filters in `vm/src/jit/helpers.rs` degrade a live reference to `null` and increment **nothing**; the interpreter's twin feeds `note_object_degradation` (`types/src/compact_value.rs:330`) and a one-shot stderr line. | `object_degradation_count()` reads **`0`** — a clean bill of health — in exactly the configuration where the still-open root-coverage gap of `6a04b0e3c1` is most likely to fire, because JIT'd frames are the ones a deposited root snapshot misses. A silent wrong answer *and* a silent instrument. **This is a live bug on the default collector, not a ZGC risk.** | `JIT_REF_DEGRADATIONS` + `jit_ref_degradation_count()` added on the JIT side (`vm/src/jit/helpers.rs:4928`, `:4935`). Unify with the interpreter's counter by widening `note_object_degradation` to `pub` (`types/src/compact_value.rs:330`) — one word, another crate. Until then read **both** numbers; a counter printed only when non-zero hides "never ran". |
| **J1c** | **`forward_jit_arg_at` skips the barrier silently** (added §2.5.5). Its hand-rolled `raw >= 1<<48` test (`vm/src/jit/helpers.rs:468`) `return`s instead of degrading, so a colored word means "argument not forwarded" with no null, no counter and no diagnostic. It matches no `plausible_heap_pointer` grep. | A stale argument reference surviving a relocation, scoped to the JIT calling boundary. Invisible to every instrument in J1b. | Rests on "arguments arrive already barriered" — an **assumption of the same class as J2**, not a proof. Either prove it or route the check through the same decode chokepoint. It is also the natural home for the D/E/F barrier dispatch, since it already owns this file's `load_and_forward` call for moving collectors (`:475`). |
| **J2** | **The scalar-replacement assumption.** `bytecode_walk.rs:3940` reads a frame slot for a scalar-replaced object's field, which is barrier-free *only if* every write into that slot came from a barriered value. Nothing asserts this. | Silent stale reference, scoped to methods where escape analysis fired — i.e. hot methods. | Assert it in stage (a): a scalar-replaced object never escapes, so its fields can only be written from barriered loads or fresh allocations. **Verify, do not assume** — this is an explicit open question (§8). |
| **J3** | **An in-flight reference in a register at the slow-arm call.** The barrier runs between the load and `mark_top_as_oop()`, and **there is no register oop map** (`jit/src/regalloc.rs:1493-1500`). | A relocating collection during the barrier slow path moves the object whose address is in RAX and nothing rewrites RAX. **Use-after-free, non-deterministic, load-dependent.** | The mechanism already exists: the blind full-GPR spill in `emit_pre_safepoint_spill` (`jit/src/x64/safepoint.rs:131-153`). The risk is *forgetting the bracket*, not lacking one — and half a bracket also silently breaks `fully_oop_covered` (`jit/src/x64/driver.rs:2157-2162`). Moot entirely if open question 4 resolves "the barrier cannot allocate". |
| **J3b** | **A new required `JitRuntimeHelpers` slot defaults to `0`.** `jit/tests/*` construct the struct with `..Default::default()`. | An unconditional `CALL` through a null pointer, in tests only — so it presents as a test-harness crash, not a VM bug, and gets misfiled. | Make the barrier slot **optional**, with `0` meaning "gate is on, take the helper arm", mirroring `self.getfield == 0` (`jit/src/ir_lower.rs:2295`, `:4490`). Five registration sites must be updated together (§3.3) or the const assertion at `jit-api/src/helpers_abi.rs:826` fails the build — which is the good outcome. |
| **J4** | **Flag clobbering at a site with a live flag.** §3.1. | A miscompiled branch: wrong control flow, not a crash. Likeliest at `bytecode_walk.rs:696-698` (LICM hoist, inside a guard block) and `simd.rs:326` (vector loop). | Read those two sites individually in stage (b). An encoding test per site. |
| **J5** | **The phase flip invalidates a pinned mask register.** `flip_to_mark`/`flip_to_remap` (`vaddr.rs:721`, `:754`) change `bad_mask` mid-run. A compiled method holding the old mask in R13 classifies every word wrong until it reloads. | If the stale mask is *narrower*: missed slow paths → stale references → **UAF**. If wider: spurious slow paths → throughput cliff, no corruption. The dangerous direction is the silent one. | Do not pin in stage (b).0 — materialize `imm64` per site (§3.1). If pinning is later measured to be worth it, the reload point is the safepoint poll, and it must be proven, not assumed. |
| **J6** | **Offset 0 is a legal object location.** `barrier.rs:459-462` / `vaddr.rs`'s `color_offset_roundtrip_many_offsets`. The `jz .done` at §3.1 step 5 cannot tell it from null. | A live object at heap offset 0 read as `null` from compiled code — and *only* from compiled code, because the Rust barrier keeps the raw word and can disambiguate (`barrier.rs:600-606`). A JIT/interpreter answer divergence. | Reserve offset 0 in the page allocator (`gc/src/zgc/page.rs`) and assert it. **This is a cross-workstream dependency on 1d and must be raised there** — see §8. |
| **J7** | **Linux-only, host-only.** The Azure build host is where the corpus runs, and the two backends differ in ABI: `ARG_REGS` is 4 registers on Windows and 6 on SysV (`jit/src/x64/reg_encoding.rs:90`, `:93`). A stub that marshals more than four arguments is correct on Linux and clobbers a live register on Windows — or vice versa. | A test suite that is green on the developer's Windows checkout and segfaults on the Azure host, or the reverse. A sibling just found a bit-budget bug with exactly this asymmetry. | Keep the stub's argument count ≤ 4. Build and run the `--features zgc` unit tests on both platforms before landing stage (b) — CI already builds the *binary* under the feature (`.github/workflows/ci.yml:491`, `:495`), which is the gap `zgc-production-implementation-plan.md` §R4 closed after the feature was uncompilable for two days without CI noticing. |
| **J8** | **The barrier never heals, so the slow path never gets cheaper.** If stage (a)'s helper barrier is wired without the heal CAS (or the CAS keeps losing), every load stays slow. | Not a correctness bug. Presents as a **hang** at the 300 s suite ceiling, indistinguishable from the 35 PASS→HANG classes already recorded in the 2026-08-07 baseline. | `ZBarrierStats` already counts `heal_cas_wins` / `heal_cas_losses` / `slow_path_entries` (`gc/src/zgc/barrier.rs:200-300`). Wire them into Phase 0b's metrics and read them before blaming the collector. A counter printed only when non-zero hides "never ran". |
| **J9** | **Deadlock via the barrier slow path.** `mark_live` runs from arbitrary mutator threads and "must not take a process-global lock per call" (`barrier.rs:467-471`). A JIT stub call adds a second entry point to it, from a thread that may already hold a JIT/registry lock. | **Hang**, thread-count-dependent, likelier under the suite's own concurrency than solo. | The stub must call the same `ZMarkQueue`/`z_mark_thread_local_log` path the interpreter uses (`barrier.rs:1313`), never a new one. Registry guards held across a blocking call are a known lock-cycle shape in this tree. |

---

## 8. Open questions

Marked explicitly rather than guessed.

1. **Is offset 0 ever allocated?** §3.1 step 5 and J6 depend on the answer.
   `gc/src/zgc/page.rs` is owned by workstream 1d and was not read for this
   document. If offset 0 *is* allocatable, the JIT fast path needs a different
   null discriminator (e.g. testing `Z_COLORED_TAG` rather than the masked
   result), which costs an instruction and changes §3.1.
2. **Is a scalar-replaced object's field slot only ever written from a
   barriered value?** J2. Requires reading `jit/src/x64/escape_analysis.rs`'s
   materialization rules against every `putfield` arm. Not done here.
3. **Where does the refusal gate belong — `vm_init` or JIT compile time?** §1.3.
   Both have precedent (`vm_init.rs:1396`; `ir_lower.rs:4733-4741`). §5
   recommends both; this has not been argued against a third option (a
   `GarbageCollector`-trait capability query).
4. **Can `ZBarrierContext::forward` allocate, in the concrete Phase-3b
   implementation?** §4.1 assumes it can, from the trait's doc
   ("performs the copy and installs one", `barrier.rs:452-454`). If
   `gc/src/zgc/relocate.rs` in fact never allocates on the mutator's barrier
   path — e.g. it always defers to a relocation worker — then the barrier stub
   is **not** GC-capable and §4's entire safepoint-map obligation disappears.
   **That would move stage (b) from 3-5 weeks to 2-3.** It is the single
   highest-leverage unknown in this document and should be settled with
   workstream 3b before (b) is scheduled.
5. **Does `getstatic` need the barrier at all in stage (a)?** Statics are the
   one shape the slot-representation study found is *not* atomic today
   (`zgc-reference-slot-representation.md`, §0 table). A non-atomic slot cannot
   be healed with a CAS, so `getstatic` may need the non-healing fallback arm
   that study's option (e) provides per-slot. Not resolved here.


## The getfield residual this blocks

Recorded here 2026-08-20, when
`every-jit-getfield-takes-the-helper-FIXED-20260820.md`
was retired. Everything on that page that could be fixed without this barrier
was fixed; what is left is this, and it is a property of the slot
representation rather than of the getfield arms:

| collector | JIT `getfield` helper calls | failing guard clause | primitive misses |
|---|---:|---|---:|
| Generational | **0** | — | 0 |
| G1 | **0** | — | 0 |
| ZGC | **56 930 918** | 100% `outside-published-bounds` | **0** |

`SHA256Digest` x200 000, one binary, counted at execution on the one path every
fall-through crosses. Every one of those calls is a **reference** read; the
inline path is engaged for every primitive field read on all three collectors.

ZGC publishes nothing into `JIT_REGION_BOUNDS`, so the containment clause fails
for every receiver, every time. That is not a bug to fix in the guard: a compact
reference slot on ZGC holds `Z_COLORED_TAG | colour | offset` rather than a
pointer, so an inlined load of it is exactly the use-after-free this design
exists to prevent. **Publishing bounds to make the check pass would be the
defect**, not the fix.

So the ZGC number moves when this barrier lands, and not before. Two things
already landed against the same residual WITHOUT touching the inline path or the
colouring, both by removing redundant validation of the RECEIVER (which the IR
already types `Ref`) rather than by inlining the loaded VALUE:

* stop asking `is_object_address` on a proven-oop receiver — 34 470 791 of
  34 470 791 helper calls take it, ~1.05x on ZGC;
* validate ONCE per native accessor call rather than two or three times —
  64 248 919 -> 38 879 898 walks, 1.07x on ZGC and 1.11x on Generational, 5 of 5
  interleaved pairs each (`CRATONVM_GC_NO_VALIDATE_ONCE=1` A/Bs it in one
  binary).

Neither is a precedent for inlining a coloured load, and the retired page says
so in the same words. After both, `jit_getfield`'s own body is still 16.31% of a
ZGC profile of that kernel, and the membership walk is no longer the largest
single item — per-call native dispatch is
(`try_jit_site_cached_native_dispatch` 8.81% + `safe_native_call_impl` 8.33%).
