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

`cov-04` **closed 2026-08-03** and has already moved this lane's numbers —
`ldc` 90 → **100**, `getstatic` 91 → **94**, `ldc_w` 7 → **8** — because the
methods it unblocked reach their next gap here. Still 69% of the opcode bucket,
still the largest, but re-derive before quoting a count. Details in
[`docs/internal/cov-04-the-invoke-arms-RETIRED-20260803.md`](../../internal/cov-04-the-invoke-arms-RETIRED-20260803.md).
