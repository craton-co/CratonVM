# `SQLChar.rawData` holds `Int(1)` — RETIRED: the localization was wrong and both instruments were partial

| | |
|---|---|
| **Status** | RETIRED 2026-08-27. Not reproduced in **320 runs** with a live positive control, against a **320-run same-day baseline** that also produced none. The named writer is exonerated on three independent grounds, both watches are fixed, and the residual this page asked to close is closed. |
| **Symptom** | a `[C` field (slot 1 of an 8-slot `SQLChar`) holding `Value::Int(1)` |
| **Why it mattered** | that cell is what a compiled `arraylength` dereferenced as the pointer `1`, `SIGSEGV addr=0x5` |

## The named writer is exonerated, three times over

The page localized the writer by elimination to `jit/src/ir_lower.rs`,
`Op::Store(MemKind::Int)` — "the inline heap write". Every step of that chain
fails.

### 1. The eliminating instrument could not see the population it eliminated

> `ctx.set_field` reaches that watch (`set_field` → `set_field_no_satb` →
> `set_field_no_card`), so this native did not write the cell. […] the
> single-pass backend does not even inline primitive putfields (they all go
> through `putfield_int` -> `heap.set_field`, i.e. through that watch).

**`jit_putfield_int` does not call `heap.set_field`.** Neither do
`jit_putfield_long`, `_float` or `_double`. All four resolve the cell themselves
with `jit_field_cell_ptr` and write it with `write_value_atomic` /
`write_compact_field` — no accessor, no lock, no watch. The collector-side
`CRATONVM_DBG_WATCH_PUN` trap in `ZgcRealHeap::set_field_no_card` is blind to
the entire compiled putfield family, which is exactly the population a
JIT-written punned cell comes from.

Measured on this page's own positive control (`SQLChar:2`, one run of
`TestWebdavPropertyStore`, same binary):

| store watch | hits on `SQLChar:2` |
|---|---:|
| collector-side (`punned store watch`) | 12 548 |
| **compiled-store side (`[punned-store-jit]`, new)** | **2 413** |

Roughly one write in six of the page's own positive control was invisible to the
instrument that declared slot 1 unwritten. The measured `0` was never evidence
of absence.

### 2. The named writer is not emitted in the reported configuration

`Op::Store(MemKind::Int)`'s inline arm is preceded by

```rust
if cratonvm_types::compact_ref_fields_enabled() {
    …  jit_putfield_int helper …
    return;
}
// Receiver → RAX, value → RCX.   ← the inline write the page names
```

and `compact_ref_fields_enabled()` **defaults to true**. The page's own
reproduce command sets no `CRATONVM_COMPACT_REF_FIELDS`, so that inline write
was never emitted in any run this page describes.

### 3. The page's own report contradicts it

The inline arm ends with `MOV qword [RAX + high_off], 0` — it clears the high
qword by construction. The reported cell is `tag=0 payload32=0x1
payload64=0x1`. `payload64` is not 0.

## The READ watch was partial too, and that is why this page's numbers looked clean

The 2026-08-24 read-side arm was added to `ZgcRealHeap::get_field` so the
question would "survive `--nojit`". It does — but it only ever sees the reads
that go through the **accessor**, and a compiled `getfield` does not. Measured
on the pre-fix binary, one run:

| | punned reference reads seen |
|---|---:|
| `jit_getfield_impl` counter (compiled path) | 8 709 |
| `ZgcRealHeap::get_field` read watch (`punned read watch`) | **0** |

Both watches were counting one half of their own question. A zero from either,
on its own, means "not on this path".

*(A note for anyone diffing arms: after the fixes, the same run splits 7 248
helper-side / 1 774 accessor-side — the same ~8 700 reads, routed differently
because 891 call sites changed binding. Cell contents are identical: `tag=0
payload32=0 payload64=0`, the benign never-assigned `rawData` that both paths
correctly return as null. `--nojit` on both binaries reads 0 accessor-side, which
is the control that says the split is routing and not corruption.)*

## The ambiguity in the report, now closed

The page could not decide what a non-zero `payload64` under an `Int` tag meant,
and said so: "either a writer that used the WRONG offset or padding left by the
cell's previous occupant — and the two want completely different searches."

It was padding, but not the *cell's*. `write_value_atomic` was
`transmute::<Value, [u64; 2]>`, and `Value` is `repr(u32)` with a 32-bit payload
at byte 4 and a 64-bit payload at byte 8 — so a narrow variant (`Int`, `Float`,
`ReturnAddress`, `Uninitialized`) leaves bytes 8..16 as **uninitialised padding
of the caller's stack temp**, which the store then committed to the heap cell.
Formally UB; in practice, arbitrary bits.

`value_words` (`types/src/value.rs`) now builds both words explicitly and writes
the unused half as a hard zero. `narrow_variants_zero_the_payload64_word` and
`value_words_round_trips_through_the_atomic_reader` pin it, and ZGC's legacy
`set_field` arm switched from `ptr::write` to `write_value_atomic` so it gets the
same treatment (and the tear-freedom `g1` and `gen_heap` already had).

That word is not inert: it is the word a compiled reference `getfield`
**dereferences**, and the VM's own containment argument for a primitive in a
declared-reference slot is that it is zero — "a reference field of a freshly
allocated object, whose cell is still zero-filled and so decodes as `Int(0)` —
that word is 0, i.e. the correct null, by accident". Garbage padding is what
turned that benign null into the pointer `1` and the `SIGSEGV addr=0x5`.

**Consequence for whoever meets this next:** padding is no longer an available
explanation. If the cell reappears with a non-zero `payload64`, "a writer used
the wrong offset" is now the *only* reading — decided by the report itself
rather than by argument.

## The residual this page asked to close, plus two more of the same kind

> A non-constant offset node yields slot **0**, silently. That is not the slot
> seen here (1), so the fallback is not itself the observed defect — but it is
> the same hazard class in the same expression, and worth closing regardless.

Closed, along with two siblings that were live and unmentioned. Each now
**refuses** rather than substituting, and each is **counted**, so a future zero
is a measurement:

* **`ir_lower.rs`** — `Op::Load`/`Op::Store`'s field index came from
  `match … { Op::Const(v) => v, _ => 0 }`. `lower_inner` now refuses such a
  graph outright; the arm itself deopts rather than guessing.
* **`bytecode_walk.rs`** — four `.unwrap_or((pc, 0, b'I'))` field defaults:
  **slot 0, tagged int**. This exact default has been caught corrupting a heap
  cell before — JDT's `HashtableOfInt.rehash` storing an `int[]` as
  `Value::Int(low32_of_ptr)`, which the next `put()` died on at `arraylength`.
  The fix then populated one table and left the default for every other path.
  Now a counted bail; `CRATONVM_JIT_UNRESOLVED_FIELD_SUBSTITUTE=1` restores it
  for a single-binary A/B.
* **`jit_bridge.rs`** — field resolution matches by **name alone**
  (`find_own_field` / `find_field_recursive` / `locate_field`; JVMS §5.4.3.2 says
  name *and descriptor*), while the JIT takes the type tag from the
  constant-pool descriptor. A site where the two disagree emits one field's tag
  at another field's slot — the punning shape exactly. Now refused and counted.

Also counted, because a dropped store leaves a null that looks like a program
null: `jit_putfield_object` and its three primitive siblings each return without
writing on an implausible receiver or an out-of-bounds slot, and neither exit
recorded anything.

**None of them fired on this workload.** All read 0 across both 320-run arms.

## The measurement

Both arms: `TestWebdavPropertyStore`, four concurrent streams × 80 runs,
`--Xmx 2g`, `CRATONVM_DBG_WATCH_PUN=SQLChar:1`.

| | baseline (`a6e85e87f`, same day) | fixed branch |
|---|---:|---:|
| runs | 320 | 320 |
| `OK (2 tests)` | 320 | 320 |
| non-zero exits / SIGSEGV | 0 | 0 |
| punned cells with non-zero payload | **0** | **0** |
| compiled store watch on slot 1 | (instrument absent) | **0** |
| collector store watch on slot 1 | 0 | 0 |
| accessor read watch on slot 1 | 0 | 567 641 — all `payload32=0 payload64=0` |

Positive control, same binaries, `SQLChar:2`:

| | baseline | fixed |
|---|---:|---:|
| collector store watch | **6 879** | 12 548 |
| compiled store watch | (absent) | **2 413** |
| accessor read watch | 8 211 | — |

The baseline's 6 879 is the page's own recorded number, reproduced exactly, so
the instrument is the same one and it is live.

## Why this is retired rather than fixed

Nothing here fixed the punned write, because nothing here ever saw it. What this
work establishes is:

* the writer named by the page **cannot** be the writer (three independent
  refutations, one of them from the page's own report);
* both watches the elimination rested on were **partial**, and the compiled-store
  half — the only place a JIT-written punned cell can appear — had **no watch at
  all**;
* the report's central ambiguity is **removed** rather than argued;
* the residual is **closed**, and so are two siblings of it;
* and on 2026-08-27 the cell does not appear in 320 runs on the current tree
  **or** in 320 runs on the same-day baseline, with a live positive control on
  both.

A page whose analysis is disproven and whose symptom does not reproduce against
its own control is not an open investigation. If the cell returns, the
instruments below will name the writer on the first occurrence instead of after
another elimination round.

## How to ask again

```bash
CRATONVM_DBG_PUNNED_REF=SQLChar CRATONVM_DBG_WATCH_PUN=SQLChar:1 \
  <cratonvm> --java-home <jdk25> --Xmx 2g -cp <tomcat-suite-cp> \
  org.junit.runner.JUnitCore org.apache.catalina.servlets.TestWebdavPropertyStore
```

Four concurrent streams. Grep `^\[punned-store-jit\]` (the compiled writer, with
class, slot, value, `declared_ref`, JIT callee and ~40 symbolized frames) and
`^\[punned-ref\]` (the reader — filtered by **class substring** now, because the
benign zero-fill population reaches 567 641 hits in 320 runs and would otherwise
spend the whole 32-line budget on noise; `=1` keeps the old dangerous-payload
subset). `CRATONVM_DBG_JIT_METHOD_STATS=1` prints the refusal and dropped-store
counters.

**Arm the same watch on `SQLChar:2` first and confirm both store sides fire.**
That is a six-second positive control, and it is what would have caught the
original blind spot.

## Related

* `bg-compile-off-nulls-a-reference-argument-FIXED-20260827.md` — the sibling
  page whose investigation un-blinded this instrument. Its own defect turned out
  to be a different member of the same "a reference reads as null" family: an
  `invokevirtual` naming a **private** method, dispatched from the receiver
  instead of bound at the resolved owner (JVMS §5.4.6 / JEP 181).
* `G30-1-the-silent-reference-slot-coercion-20260817.md` — the species.
