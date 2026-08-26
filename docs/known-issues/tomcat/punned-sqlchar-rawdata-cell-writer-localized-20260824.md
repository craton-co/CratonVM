# `SQLChar.rawData` holds `Int(1)`: the writer is localized to the IR backend's inline int store

| | |
|---|---|
| **Status** | OPEN — writer localized to one code path, not yet caught in the act (2026-08-24) |
| **Symptom** | a `[C` field (slot 1 of an 8-slot `SQLChar`) holding `Value::Int(1)` |
| **Why it matters** | that cell is what a compiled `arraylength` dereferenced as the pointer `1`, `SIGSEGV addr=0x5` |
| **Rate** | ~0.5–5% per run of `catalina.servlets.TestWebdavPropertyStore`, depending on build |

## What the cell is

```text
class_id=1848 num_slots=8 field_index=1 tag=0 payload32=0x1 payload64=0x1
decoded=Int(1) compact_flag=false gc_flags=0x0
```

`compact_flag=false` matters: the object is genuinely legacy-laid-out, so this
really is a 16-byte `Value` cell at legacy offsets holding `Int(1)`. It is not a
compact body misread at the wrong stride — that hypothesis was tested and ruled
out by adding the flag to the report.

## Two hypotheses tested and killed

**1. `transferTo`'s duck-type.** It writes exactly this shape —
`ctx.set_field(input, 1, Value::Int(count))`, and `rawData` IS slot 1 — and
fixing it moved an A/B from 3 punned cells in 93 runs to 0 in 93. That looked
conclusive. It is not: a store-side watch on a binary that still has the bug
fired **6879 times watching `SQLChar:2`** (`rawLength`, an int — the positive
control) and **zero times watching `SQLChar:1`**, including in a run that
demonstrably produced the punned cell. `ctx.set_field` reaches that watch
(`set_field` → `set_field_no_satb` → `set_field_no_card`), so this native did not
write the cell. The 3-vs-0 A/B was the 1-in-20 coincidence its own p-value
advertised.

**2. A compact-layout misread.** Killed by `compact_flag=false` above.

## Where the write must come from

Nothing that goes through the heap store accessor writes slot 1 — the watch is
live for this class and never sees it. `set_field_volatile` routes through
`set_field`; the `jit_putfield_*` helpers call `heap.set_field`
(`vm/src/jit/helpers.rs`); and the single-pass backend does not even inline
primitive putfields — every one becomes a `putfield_int`/`_long`/`_float`/
`_double` helper call.

That leaves **`jit/src/ir_lower.rs`, `Op::Store(MemKind::Int)`**, which its own
comment describes as "the inline heap write": it emits the `Value::Int` cell
directly — discriminant word, 32-bit payload at `FIELD_CELL_PAYLOAD32_OFFSET`,
high qword cleared — with no helper, no accessor, and therefore no barrier,
no guard and no watch.

Its slot index is derived as:

```rust
let field_index = match self.graph.nodes[offset_node as usize].op {
    Op::Const(v) => v,
    _ => 0,              // silent fallback
};
```

A non-constant offset node yields slot **0**, silently. That is not the slot seen
here (1), so the fallback is not itself the observed defect — but it is the same
hazard class in the same expression, and worth closing regardless.

## The experiment that settles it, and what it needs

JIT-on vs `--nojit` on one binary: with the JIT off there is no inline store, so
the punned cell should vanish. The JIT-side detector lives in
`jit_getfield_impl`, which `--nojit` also disables, so a **read-side arm** of
`CRATONVM_DBG_WATCH_PUN` was added to `ZgcRealHeap::get_field` (the accessor the
interpreter uses). Both arms are positive-controlled on slot 2:

| mode | read-watch | store-watch |
|---|---:|---:|
| JIT on | 8 220 | 6 879 |
| `--nojit` | 51 827 | 14 964 |

First attempt: **102 runs, zero occurrences in either arm — uninformative, not
negative.** The rate on that build is ~0.5–1% per run, so 102 runs expects about
one event. **Budget ~300 runs per arm.** This investigation has already been
misled once by reading a conclusion into a small-n zero; do not repeat it.

## Reproduction

```bash
CRATONVM_DBG_PUNNED_REF=1 CRATONVM_DBG_WATCH_PUN=SQLChar:1 \
  <cratonvm> ... org.junit.runner.JUnitCore \
  org.apache.catalina.servlets.TestWebdavPropertyStore
```

Four concurrent streams plus a few spinners; the cell appears in a minority of
runs. Grep `^\[punned-ref\]` — **not** `punned-ref`, which also matches the
flag-spelling banner and reports a phantom hit on every run.
