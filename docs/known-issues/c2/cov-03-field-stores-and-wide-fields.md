# COV-03 — `getfield` learned about references; `putfield` twenty lines below it did not

**Status:** not started. **Independent of every other lane.**
**Owns:** the `0xb4` and `0xb5` arms of `IrBuilder::build` in `jit/src/ir.rs`
(`ir.rs:4995` and `ir.rs:5074` today), and their lowering in
`jit/src/ir_lower.rs`. Not `ir_compatible`.

> **Re-measure before quoting the survey's 43.** `cov-04` closed 2026-08-03,
> and the two terms it removed admitted a population of **constructors** to the
> builder — which is the code that writes reference fields. The `putfield` row
> went **37 → 59**, making it the largest builder refusal in the corpus by a
> wide margin, larger than every other structural refusal combined. The line
> numbers moved as well; re-derive both. See
> [`docs/internal/cov-04-the-invoke-arms-RETIRED-20260803.md`](../../internal/cov-04-the-invoke-arms-RETIRED-20260803.md).

## The finding

The `getfield` arm accepts an int-family tag **or a reference**:

```rust
let is_ref_field = matches!(type_tag, b'L' | b'[');
if !matches!(type_tag, b'I' | b'Z' | b'B' | b'C' | b'S') && !is_ref_field {
    return ir_build_bail(line!(), pc);
}
```

Its own comment records why the reference case was added:

> This was the single largest builder refusal measured on a real workload
> (66 of 141 on a Hibernate class): a reference field read is most of what
> object-oriented Java does.

The `putfield` arm, twenty lines below, has no `is_ref_field`:

```rust
if !matches!(type_tag, b'I' | b'Z' | b'B' | b'C' | b'S') {
    return ir_build_bail(line!(), pc);
}
```

**37 events** on `ir.rs:5085` — the second-largest structural refusal in the
survey — and a reference field *write* is most of what object-oriented Java
does too. This is one fix applied to one of two sites.

`ir.rs:5041`, the `getfield` bail that survives, is `long`/`float`/`double`
fields: **6 events**. Small, and it is in this lane only because it is the same
two arms.

## Why it is not a one-line change

Because the asymmetry is probably not an oversight. A reference **store** needs
things a reference **load** does not:

* a **write barrier**, if the collector wants one for an old→young store. Find
  out what the single-pass `putfield` arm emits and match it exactly. A missing
  barrier is invisible until a concurrent or generational collection, which is
  the worst failure signature this VM has.
* the **compact-layout split**. The `getfield` comment says `ir_lower::lower_inner`
  picks between a layout-naive inline lowering and a compact-aware helper
  (`jit_putfield_int`) and refuses only when compact layout is on and no helper
  address is available. There is no `jit_putfield_ref` in that sentence. Check
  whether one exists before assuming the same shape works.

Write down which of those is the real reason before writing code. If it turns
out to be neither — if the ref case was simply never added — say that in the PR,
because "the fix was applied to one of two sites" is a pattern worth counting.

## The first increment

Reference `putfield` on the **non-compact** path only, refusing when compact
layout is on, exactly as the current code refuses everything. That keeps the
refusal narrower rather than replacing it with a guess, and it is measurable on
its own.

`long`/`float`/`double` fields (both arms) second. They are 6 events and they
carry the category-2 slot question, which the IR's parameter model has already
been burned by once — see the `optimize=false` guard's comment about
`boolean eq(long, long)` truncating its second parameter.

## How to verify

* `CRATONVM_DBG=ir-compiles` on `ConditionalOnPropertyTests`: the
  `refused at ir.rs:5085` count falls, `optimizing backend produced a body`
  rises. Both numbers, not one.
* `jit/tests/ir_vs_singlepass.rs`: a method that stores a reference field,
  reads it back, and returns it — same answer from both backends.
* **A generational/moving test.** A reference stored into an old object,
  pointing at a young one, surviving a young collection. If the barrier is
  missing this is the only test that fails, and it fails as a lost object, not
  as a crash at the store.

## What to refuse

A reference store whose barrier this lane cannot place, and any field access on
the compact path without a helper to serve it. Both are already refused today;
the lane's job is to make the refusal smaller, never to make it a guess.
