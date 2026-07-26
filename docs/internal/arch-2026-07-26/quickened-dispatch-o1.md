# O(1) quickened dispatch: pc → index in constant time

Status: **landed** (default-on, no gate)
Basis: `dev` @ `6495a191c` (worktree branched from `6b3e47591`, merged forward)
Owner slug: `quickened-dispatch-o1`
Files: `reader/src/quickened.rs`, `reader/src/instruction.rs`

---

## 1. The residual this closes

Bytecode quickening is already landed. `vm/src/runtime/interpreter.rs:7872`
(`quickened_for_frame`) memoises a decoded instruction stream per method and
`reader/src/quickened.rs` interns it per code allocation, so the interpreter
dispatches from a borrowed `&Instruction` instead of re-running
`Instruction::decode` on every bytecode.

What remained was the **pc → stream-index lookup**, which runs on *every*
dispatch. Before this change (`reader/src/quickened.rs:186`, pre-merge):

```rust
if hint < n && self.pcs[hint] == pc32 { return Some(hint); }
match self.pcs[..n].binary_search(&pc32) { ... }
```

The caller only ever sets `quick_hint = idx + 1`
(`vm/src/runtime/interpreter.rs:10400`), i.e. the hint covers **straight-line
fall-through and nothing else**. Every taken branch, every loop back-edge,
every exception-handler entry and every `switch` target therefore fell through
to an O(log n) binary search — a chain of dependent, poorly-predicted loads,
in exactly the control-flow-heavy code where the interpreter already hurts
most.

The design deliberately keeps pc-keyed indirection rather than rewriting
bytecode in place, because pc values must stay observable: exception tables,
stack maps, line-number tables, JVMTI single-step and JIT/OSR entry points all
key on the original bytecode pc. That contract
(`reader/src/quickened.rs:19-40`) is preserved verbatim; this change only
makes the lookup constant-time.

---

## 2. Measured method-size distribution

The representation was chosen from measurement, not intuition. A standalone
class-file parser (scratch C#, static analysis only — no VM build, no VM run)
walked every method in three corpora and applied **the exact validation rules
`Instruction::decode` applies** (unknown opcodes, `invokeinterface` /
`invokedynamic` reserved bytes, legal `wide` sub-opcodes, `tableswitch`
`high >= low`, `MAX_SWITCH_ENTRIES`):

| corpus | jars | classes | methods |
|---|---|---|---|
| Local Maven repository | 1,963 | 274,891 | 2,130,461 |
| JDK 25 (`jmods`, all 70 modules) | 70 | 27,140 | 210,769 |
| This repo (`test_classes/`, `vm/tests/resources/`) | 1 | 527 | 3,784 |
| **total** | **2,034** | **302,558** | **2,345,014** |

Aggregates:

```
total bytecode bytes = 114,032,544 (108.7 MB)
total instructions   =  57,730,915
mean code_len = 48.63   mean insns = 24.62   bytes/insn = 1.98
```

`code_length` percentiles:

| p50 | p75 | p90 | p95 | p99 | p99.9 | p99.99 | max |
|---|---|---|---|---|---|---|---|
| 13 | 36 | 92 | 160 | 453 | 2,204 | 19,534 | 60,399 |

Cumulative share:

| `code_len <=` | methods | bytecode bytes |
|---|---|---|
| 16 | 56.11% | 9.22% |
| 64 | **85.26%** | 29.09% |
| 128 | 93.33% | 43.96% |
| 512 | 99.18% | 71.38% |
| 65,536 | 100.00% | 100.00% |

Two facts drive everything below:

1. **Methods are tiny.** The median method is 13 bytes / ~7 instructions;
   85.3% fit in 64 bytes.
2. **No method comes close to the u16 ceiling.** Max `code_length` is 60,399
   and max instruction count is 35,760. Zero methods exceed 65,535 on either
   axis — consistent with JVMS §4.7.3, which requires `code_length < 65536`.

---

## 3. Representations evaluated

Baseline for the percentages: the quickened stream already costs
`16 B/instruction` (records) + `4 B/instruction` (pc table) ≈ **496 B/method
mean**, 1,110 MB across the corpus.

| # | representation | corpus cost | vs stream | mean B/method | lookup |
|---|---|---|---|---|---|
| A | dense `u32` per code byte | 435.0 MB | +39.2% | 195 | 1 load, O(1) |
| B | dense `u16` per code byte | 217.5 MB | +19.6% | 97 | 1 load, O(1) |
| C | `u16` page table / 16 code bytes | 20.3 MB | +1.8% | 9 | **not O(1)** |
| **D** | **start bitmap + block popcount** | **52.8 MB** | **+4.76%** | **23.6** | **2 loads + popcount, O(1)** |

**A (dense `u32`)** — the obvious approach, and the one the brief flagged as
suspect. Rejected: 4 bytes per bytecode byte is a 39% surcharge on a stream
that is already the dominant metadata cost, to store a value that never needs
more than 16 bits.

**B (dense `u16`)** — halves A and is provably sufficient (§2 fact 2), but is
still ~100 B/method to encode information with an entropy of well under a
byte per code byte. Rejected in favour of D, which is 4x cheaper for the same
asymptotics.

**C (page table + in-page scan)** — cheapest on paper, but it is **not
actually O(1)**: resolving a pc requires a linear scan of up to `page_size`
instructions within the page. At 16-byte pages that is up to 16 compares,
mean ~8. The median method has only ~7 instructions *in total*, which the old
binary search resolves in ~3 compares — so C would be a **pessimisation for
the majority of methods**. Rejected. This is precisely the trap the brief
warned about: picking on memory alone without checking the size distribution.

**D (chosen) — instruction-start bitmap + per-block cumulative count.**

```rust
#[repr(C)]
struct PcBlock {
    starts: u64,   // bit k set <=> pc (block*64 + k) is an instruction start
    cum:    u32,   // instruction starts in all preceding blocks
}                  // size 16, align 8
```

Lookup is branch-free on the data:

```rust
let block = blocks.get(pc / 64)?;
let bit = (pc % 64) as u32;
if (block.starts >> bit) & 1 == 0 { return None; }
let below = block.starts & ((1u64 << bit) - 1);
Some(block.cum as usize + below.count_ones() as usize)
```

Two loads from **the same 16 bytes** (one cache line holds 4 blocks = 256
code bytes of coverage), one `POPCNT`, no search, no data-dependent branching.
85.3% of methods need exactly one block, so their entire index is 16 bytes.

---

## 4. Memory delta

Per-method cost is `16 * ceil(code_len / 64)` bytes.

| | value |
|---|---|
| mean per method | **23.6 B** (+4.76% on the existing 496 B/method stream) |
| median method (13 B code) | 16 B (one block) |
| p90 method (92 B code) | 32 B |
| p99 method (453 B code) | 128 B |
| largest method in corpus (60,399 B) | 15,104 B |
| structural worst case (`code_len` 65,535) | 16,384 B |
| whole corpus (2.35 M methods, 108.7 MB bytecode) | **52.8 MB** |
| amortised | 0.486 B per bytecode byte |

Whole-application figure for the 15k-class Spring app in the brief, using the
corpus mean of 7.75 methods/class and 48.6 code bytes/method:

| | |
|---|---|
| methods | ~116,000 |
| bytecode | ~5.7 MB |
| existing quickened stream | ~57.7 MB |
| **dense index added by this change** | **~2.7 MB** |
| dense `u32` would have added | ~22.6 MB (8.3x more) |

These are **upper bounds**: `intern()` is called lazily from
`quickened_for_frame`, so only methods that actually execute are quickened and
only they pay for an index.

`heap_bytes()` now includes the index, and `index_bytes()` reports it on its
own. `report_stats()` (`CRATONVM_QUICKEN_STATS=1`) additionally prints
`index_bytes`, `index_bytes_per_method`, `no_dense_index` and `truncated`.
`quicken_index_stats()` exposes the same three counters programmatically.

---

## 5. Correctness

The contract at `reader/src/quickened.rs:19-40` is unchanged and every clause
still holds:

* **`index_of_pc` only ever returns `i` with `pcs[i] == pc`.** The bitmap is
  built *from* `pcs[..ops.len()]` — one set bit per recorded start, nothing
  else — so it indexes exactly the same set the binary search did. It cannot
  make a lookup succeed that the search would have failed, or vice versa.
  Because the two agree exactly, a dense miss is authoritative and returns
  `None` without falling through to a search; that is what makes the miss path
  O(1) as well.
* **`next_pc(i)` still returns exactly what `Instruction::decode` returned.**
  Untouched: it is still `pcs[i + 1]`, written by the build walk.
* **A pc that is not an instruction start still cleanly reports "not found".**
  Interior bytes of multi-byte instructions, `wide` operand bytes,
  `tableswitch` alignment padding, dead bytes and out-of-range pcs all have a
  clear bit (or no block at all) and return `None`, so the caller falls back
  to full decode. `pc / 64` cannot overflow and `blocks.get()` bounds-checks,
  so `usize::MAX` is handled without a panic.
* **No observable pc changes.** No pc is renumbered, reordered or synthesised.

### Size escape hatch

The dense index is built only when the highest instruction start pc is
`<= MAX_DENSE_START_PC` (65,535); above that the stream keeps the binary
search. This is a **structural** bound, not a tunable and not an env gate:
JVMS §4.7.3 caps `code_length` below 65,536, so the highest legal start pc is
65,534 and *every* method from a valid classfile qualifies with a byte to
spare. The measured maximum across 2.35 M methods was 60,399. The bound exists
solely to keep the worst-case index allocation bounded at 16 KiB for
synthesised or malformed bytecode. `has_dense_index()` reports which path a
stream took; `STAT_NO_INDEX` counts the fallbacks (expected: zero).

---

## 6. Switch-table allocation: gap found, mostly closed, residual specified

**Quickened path: confirmed allocation-free.** `Instruction::Tableswitch` /
`Lookupswitch` hold `Arc<TableSwitch>` / `Arc<LookupSwitch>`, so an
`Instruction` is a fixed-size 16-byte record with no `Vec` in any variant. The
interpreter borrows the payload (`vm/src/runtime/interpreter.rs:14796` and
`:14807`) and never clones the `Arc`. Executing a switch from the stream
allocates nothing. Test `both_switch_kinds_resolve_without_reallocating`
pins this by asserting repeated `resolve()` calls hand back the *same* payload
allocation.

**Fallback path: it did allocate.** `Instruction::decode` builds a fresh `Vec`
+ `Arc` every call, so any switch dispatched through the fallback allocated
per execution. That path is taken when `build()` returned `None` — previously
the case for a method whose linear walk hit *any* decode error, at *every* pc
in that method, for the life of the process.

**Closed:** `build()` now **salvages the decodable prefix** instead of
discarding the whole method. Records up to the failure point are kept; the
failing pc and everything after it report "not found" and route through
`Instruction::decode`, which raises the identical error at the identical pc.
This is sound under the existing contract precisely because a retained record
is, by construction, exactly what `decode` returned at that pc. Only a method
that cannot decode its *first* instruction is now un-quickened. Test:
`decode_failure_salvages_the_prefix`.

**Residual (specified, deliberately not closed):** a switch located *after*
the first undecodable byte in a method still re-decodes and re-allocates per
execution. Closing it fully would require a resynchronising walk that retries
at successive byte offsets after a failure. That is *correct* under the
contract (any pc that decodes may be recorded, since a lookup at that pc would
have produced the same answer), but it is a denial-of-service hazard: a
crafted 64 KB body of `0xAA` bytes would let a resync decode up to 65,535
bogus `tableswitch` records of up to 16,384 offsets each — gigabytes. The
trade is not worth it, because the case is empirically absent: **0 of
2,345,014 methods** failed a strict linear walk. `STAT_TRUNCATED` counts
prefix-salvaged methods so the assumption stays observable in production.

**Also hardened** (`reader/src/instruction.rs`): `decode` now checks that a
switch table actually fits in the remaining bytes *before* reserving for it.
A malformed header within `MAX_SWITCH_ENTRIES` could previously reserve 64 KB
(`tableswitch`) or 128 KB (`lookupswitch`) that the very next read failed on.
The reads already returned `UnexpectedEndOfData`, so this raises the identical
error variant slightly earlier (the reported position moves to the head of the
table). Tests: `tableswitch_header_larger_than_the_code_is_rejected`,
`lookupswitch_npairs_larger_than_the_code_is_rejected`.

---

## 7. API: what exists now, and what the interpreter should adopt next wave

`vm/src/runtime/interpreter.rs` is **not** edited by this change and does not
need to be — it compiles and gets the O(1) win for free.

### Kept, source-compatible (no caller change needed)

```rust
pub fn index_of_pc(&self, pc: usize, hint: usize) -> Option<usize>
```

Same signature, same semantics. It still checks `hint` first (one predictable
compare that resolves straight-line fall-through), then goes to the O(1) path
instead of a binary search. A stale or nonsensical hint is now harmless rather
than a search trigger.

### New, and what to adopt

```rust
// Hint-free O(1) resolution. `hint` is no longer load-bearing.
pub fn index_of_pc_direct(&self, pc: usize) -> Option<usize>

// Recommended: one call replaces index_of_pc + op + next_pc
// and their three separate bounds checks.
pub fn resolve(&self, pc: usize) -> Option<(&Instruction, usize)>

// Observability
pub fn has_dense_index(&self) -> bool
pub fn index_bytes(&self) -> usize
pub fn quicken_index_stats() -> (usize, usize, usize)  // (index_bytes, no_index, truncated)
```

**Recommended shape for the interpreter's next wave.** The dispatch site
(`vm/src/runtime/interpreter.rs:10370-10409`) currently does:

```rust
let quick_hit = quick.as_deref()
    .and_then(|q| q.index_of_pc(saved_pc, quick_hint).map(|i| (q, i)));
// ... later
thread.frames[frame_idx].pc = q.next_pc(idx);
quick_hint = idx + 1;
q.op(idx)
```

It should become:

```rust
let quick_hit = quick.as_deref().and_then(|q| q.resolve(saved_pc));
// ... later
let (instruction, next) = quick_hit;
thread.frames[frame_idx].pc = next;
```

Concretely: **delete the `quick_hint` local entirely** (declaration at
`:7943`, reset at `:10368`, update at `:10400`). It is now dead weight — it
saves one popcount on the fall-through path at the cost of a compare on every
other path, and it is the only reason the two-argument form still exists.
Dropping it also removes a piece of mutable loop state from the dispatch loop.
`resolve()` additionally collapses three bounds-checked slice indexes
(`pcs[hint]`, `ops[idx]`, `pcs[idx + 1]`) into one bounds check.

`index_of_pc` should be kept as a thin wrapper for any other caller until the
interpreter migration lands, then can be retired.

---

## 8. Tests added (`reader/src/quickened.rs`)

| test | covers |
|---|---|
| `random_access_pc_lookup_matches_linear_walk` | every pc in a fixture with `wide`, `tableswitch`, `lookupswitch` and a back-edge, probed in reverse with six wrong hints, against an independent linear-walk ground truth |
| `wide_prefixed_instruction_is_one_record` | `wide iload` / `wide iinc` are single records; all 8 interior bytes report not-found |
| `interior_pc_is_not_an_instruction_start` | mid-instruction pc → `None` |
| `cumulative_counts_span_blocks` | 200 instructions over 7 blocks — the cross-block `cum` arithmetic a single-block fixture cannot reach |
| `oversized_method_falls_back_to_binary_search` | the size escape hatch, plus the largest method that still qualifies |
| `decode_failure_salvages_the_prefix` | partial quickening; failing pc still reports not-found and still errors identically |
| `both_switch_kinds_resolve_without_reallocating` | repeated `resolve()` returns the same interned payload allocation |
| `pc_block_is_sixteen_bytes` | layout assumption behind every figure in §4 |
| `heap_bytes_accounts_for_the_dense_index` | accounting |

Per the wave rules this agent did not build or run anything; the orchestrator
builds after merging.

---

## 9. Reproducing the measurement

No VM build or run is involved — it is static analysis of class files:

1. Parse every `.class` in a corpus (jars, or `.jmod` files with their 4-byte
   header stripped to yield a plain zip).
2. For each `Code` attribute, record `code_length` and walk the bytecode with
   the exact opcode-length and validation rules of
   `reader/src/instruction.rs::Instruction::decode`.
3. Histogram `code_length`; cost model D is `16 * ceil(code_len / 64)` per
   method.

At runtime the same figures come from `CRATONVM_QUICKEN_STATS=1`, which prints
`index_bytes`, `index_bytes_per_method`, `no_dense_index` and `truncated`
alongside the existing stream totals.
