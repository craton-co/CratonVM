# frame-arena — stable frame addresses, pooled frame buffers, per-method bytecode

Wave: `arch-2026-07-26`. Slug: `frame-arena`.
Files owned: `vm/src/runtime/frame.rs`, `vm/src/runtime/value_stack.rs`,
`vm/src/threading/jvm_thread.rs`.

**Basis: `dev` = `6495a191c34fcf701c7c168517e6a13f462fc3dc`** (merged clean, no
conflicts). Every line number below was re-verified against that tree. `dev`
moves under concurrent sessions in this repo — if the citations do not match,
re-grep rather than trusting them.

---

## 1. What landed

| # | Task | Status |
|---|------|--------|
| 1 | Frames get stable addresses (`FrameStack` replaces `Vec<Frame>`) | **Done** |
| 2 | `Frame::new` / `Frame::new_from_arcs` route through a buffer pool | **Done** |
| 3 | Padded bytecode interned per *method* rather than per frame | **Done in this file's reach**; one-line adoption left for the `interpreter.rs` owner (§5) |
| 4 | Collapse `ValueStack::kinds` | **Deliberately not done — proven unsound in isolation** (§4) |
| 5 | Preserve `Frame`'s hot/cold field ordering | **Done** — `Frame`'s field declaration order is untouched |

Every existing `.frames` call site in the repo compiles unchanged (§3).

---

## 2. `FrameStack` — the address-stability contract

`JvmThread::frames` was `Vec<Frame>`. `Vec::push` reallocates on capacity
exhaustion and **memcpy's every live frame**, so no `&mut Frame` and no
`*mut Frame` could survive a call. The interpreter therefore re-indexed
`thread.frames[frame_idx]` on every access — **622** sites in
`runtime/interpreter.rs` (652 counting all `thread.frames[…]` forms), **eight**
of them inside the single dispatch site at `interpreter.rs:10364-10400` that
executes one bytecode. Each is a bounds check plus an `imul` by
`size_of::<Frame>()` (not a power of two) plus a pointer chase.

(The count is eight, not six, on the current `dev`: the quickening work added
`thread.frames[frame_idx].code.as_ptr()` and `quickened_for_frame(&thread.frames
[frame_idx])` at the top of the dispatch block. The 622 figure is unchanged.)

`vm/src/runtime/frame.rs` now defines `FrameStack`, a thin wrapper over
`Vec<Frame>` that turns capacity management into an explicit, observable part
of the API:

```rust
pub struct FrameStack {
    buf: Vec<Frame>,
    reloc_epoch: u32,
}
```

### Guarantees

1. A frame's address never changes while it is on the stack **unless** the
   backing buffer grows.
2. Growth is the only thing that moves frames. It happens only inside
   `reserve_stable` (which `push` calls), and it **always** increments
   `reloc_epoch()`.
3. `reserve_stable(n)` guarantees the next `n` pushes cannot grow, hence cannot
   move anything. `stable_headroom()` reports how many pushes are currently
   guaranteed relocation-free.
4. `pop`, `truncate` and `clear` never move a surviving frame and never bump the
   epoch. (`clear` also keeps the reserved capacity, so a thread that unwinds
   and re-enters does not re-reserve.)
5. `FrameStack::new()` does **not** allocate. The first `push` reserves
   `FRAME_STACK_INITIAL_STABLE_CAP` (= 256) frames; growth after that doubles.
   A thread that never runs bytecode pays exactly what it paid before.

Why 256 rather than `config.max_stack_depth` (default 8192): reserving the full
depth up front is ~2.4 MB of committed memory *per executing thread*, which
WildFly/Elasticsearch-scale thread counts cannot absorb. 256 covers essentially
every real Java stack; deeper stacks pay at most ~5 doublings over the whole
life of the thread, each announced via `reloc_epoch`.

Growth cannot be removed entirely: the "S-bytebuddy r1" comment at
`interpreter.rs:4979` documents that a `Method.invoke` re-entry pushes frames on
a path *not* bounded by `max_stack_depth`, so a hard capacity would be a new
panic surface. The epoch is the honest answer instead.

---

## 3. Drop-in compatibility (the hard constraint)

`interpreter.rs` was off-limits, and it holds 652 `thread.frames[…]` index
sites plus `.iter()`, `.push`, `.pop`, `.last()`, `.last_mut()`, `.get_mut()`,
`.truncate()`, `.len()`, `.is_empty()`. The rest of the VM adds
`for f in &thread.frames`, `for f in &mut thread.frames`, and — the awkward
one — `&self.thread.frames` passed where `&[Frame]` is expected
(`vm_exec.rs:2193`, `:2493`, `:7538` → `stackwalker::capture_full_trace` /
`capture_frames_no_lines`, both `fn(… , frames: &[Frame])`).

The repo-wide method surface on the merged tree, re-derived after the
+3,919-line interpreter drift, is exactly:

```
702 [   80 .len   77 .iter   22 .push   15 .last   5 .last_mut
  4 .pop   4 .is_empty   2 .get_mut   2 .get   1 .truncate   1 .insert
  1 .clear
```

(`.clone` and one `.insert` in that tally belong to unrelated `frames` fields —
`debug/ids.rs`'s `HashMap`, `native-builtins/src/xml_stax.rs`'s namespace
scopes, `vm_init.rs`'s `trace.frames` — not `JvmThread::frames`.) No method
outside the provided surface appears.

That last requirement is why `FrameStack` is a **contiguous** wrapper and not a
chunked arena: a chunked arena cannot implement `Deref<Target = [Frame]>`, and
`stackwalker.rs` / `vm_exec.rs` were not mine to change.

Provided surface:

* inherent `len`, `is_empty`, `iter`, `iter_mut`, `first`, `last`, `last_mut`,
  `get`, `get_mut`, `as_slice`, `as_mut_slice`, `push`, `pop`, `truncate`,
  `clear`, `insert`
* `Index<I>` / `IndexMut<I>` for any `I: SliceIndex<[Frame]>` — so `frames[i]`
  **and** `frames[a..b]` both work
* `Deref` / `DerefMut` to `[Frame]` — so `&thread.frames` still coerces to
  `&[Frame]`
* `IntoIterator` for `&FrameStack`, `&mut FrameStack`, and `FrameStack`
* `Default`, `Debug`, `From<Vec<Frame>>`, `From<FrameStack> for Vec<Frame>`,
  `FromIterator<Frame>`

`iter()` returns `std::slice::Iter`, so the existing
`.iter().enumerate().rev().take(n)` chains (which need
`DoubleEndedIterator + ExactSizeIterator`) keep working.

Audited for slice-only / `Vec`-only uses that would break: no `mem::take`,
`mem::replace`, `mem::swap`, `as_ptr`, `drain`, `retain`, `swap_remove`,
`split_off`, `sort*`, `extend`, `append`, `resize`, `reserve`, `capacity`,
`to_vec`, `clone` on `JvmThread::frames` anywhere in the repo (the two
`mem::take` hits are `VirtualThread::frozen_frames`, a different field; the new
`vm/src/vm/realms/` tree does not touch `.frames` at all). The
`Vec<Frame>` signatures in `threading/virtual_threads.rs` (`freeze_frames` /
`thaw_frames` / `park_with_frames`) have no callers that pass
`JvmThread::frames`, so they are untouched; `From<Vec<Frame>>` /
`From<FrameStack>` are provided in case a future caller wants to bridge.

The `frames` field moving from `Vec<Frame>` (24 bytes) to `FrameStack`
(32 bytes) shifts later `JvmThread` field offsets. That is safe: every
JIT-visible offset (`JvmThread::tlab_offset()`,
`JvmThread::shadow_stack_offset()`) is computed at runtime from the live
struct, and `jvm_thread.rs` already has a test
(`tlab_offset_matches_field_address`) that pins it.

---

## 4. `ValueStack::kinds` — why it survived

**It survived, and it must.** The parallel `kinds: Vec<u8>` is not vestigial SoA.

`CompactValue` is exactly 8 bytes and NaN-boxes its tag in the high bits
(`NANBOX_BITS = 0xFFFC_0000_0000_0000`, 3-bit sub-tag at bit 47, 47-bit
payload). A `long` uses all 64 bits, so `CompactValue::long(v)` is literally
`Self(v as u64)` — **there is nowhere to put a tag**. Consequences:

* A `long` whose top bits land in the tag space reads back with the *wrong*
  `tag()`. The BouncyCastle `LongArray` `0xfffd_…` words produced by
  `lxor`/`lshl`/`lushr` over `long[]` tag as `SUB_OBJECT`, so a context-free GC
  scan would treat the primitive as a heap reference and relocate it.
* Worse, the `SUB_INT` case is *provably* ambiguous. `CompactValue::int(0)` is
  `make_tagged(SUB_INT=0, 0)` = `0xFFFC_0000_0000_0000`, and
  `CompactValue::long(0xFFFC_0000_0000_0000)` is the same 64 bits. A real
  `Value::Int(0)` and that `long` are **bit-identical**. No in-slot encoding can
  separate them; `pop_long`'s own comment already says so.

The only in-band alternative is widening the slot past 8 bytes. That is a
pessimisation, not an optimisation (it doubles operand-stack and locals memory
traffic), and it breaks the `Vec<u64> ↔ Vec<CompactValue>` `repr(transparent)`
transmute relied on by the frame pool, `ValueStack::into_inner` /
`from_pooled`, `snapshot_raw` / `from_snapshot`, and the GC root scanners in
`memory/gc.rs`, `memory/roots.rs` and `jit/conservative_roots.rs` — three of
which are outside this wave's file ownership.

Getting this wrong is silent heap corruption (a mis-rooted primitive, or a
dropped root), so the correct outcome is not to do it.

**The real fix is out-of-band and has a named dependency.** Another agent is
producing `MethodTypeMaps` in `classloading/src/type_maps.rs` this wave, with
per-pc oop bitmaps. Once the interpreter consumes those, the kind of every slot
at every pc is a *static* fact from the verifier, the runtime tag becomes
redundant, and both `ValueStack::kinds` and `Frame::local_kinds` can be deleted
together. Until then they stay.

What *was* fixed is the cost the array implied. `ValueStack::new` allocates and
zero-fills two `Vec`s of `max_size`; after §5 no `Frame` constructor reaches it
in the steady state.

Locked in by new `#[cfg(test)]` tests in `value_stack.rs`:
`long_round_trips_bit_exactly_through_operand_stack`,
`long_round_trips_through_value_api`,
`compact_long_push_marks_kind_and_round_trips`,
`collision_long_and_int_zero_are_bit_identical_and_need_the_kind_mark`,
`from_pooled_clears_stale_kind_marks`.

---

## 5. Buffer pooling and per-method bytecode

### 5.1 Non-pooled constructors now pool

`Frame::new_pooled` / `Frame::new_pooled_cached` take the owning `JvmThread`'s
`locals_pool` / `stacks_pool`. `Frame::new` and `Frame::new_from_arcs` had no
thread handle, so each ran `vec![…; n]` twice for the locals pair *plus*
`ValueStack::new`, which zero-fills two more `Vec`s. Four allocations and four
zero-fills per frame — and `Frame::new_from_arcs` is the hot **uncached** invoke
path (`interpreter.rs:6975`).

`frame.rs` now owns a per-OS-thread `FrameSoaTls` pool
(`TLS_SOA_POOL_CAP = 32` pairs per role, buffers over
`TLS_SOA_MAX_RETAINED_SLOTS = 4096` slots declined). Both constructors drain it
via `init_locals_from_parts` / `value_stack_from_tls_pool` and fall back to the
exact previous allocating path when it is empty.

It is fed by `JvmThread::recycle_frame`, from the branch that used to simply
**drop** a popped frame's Vecs once `locals_pool` was at `MAX_POOL_SIZE`. So
this is a strict improvement over the previous behaviour and never competes with
the thread-owned pools.

That last point is deliberate. `interpreter.rs:7197` records a prior attempt
("NOTE (P4 reverted)") that routed the uncached path through the *thread* pools
and hung the BouncyCastle suites (bc-asn1/crypto/crypto-prng, rc 0 → 124).
Nothing here touches those pools' dynamics: the cached invoke path's pool
behaviour is bit-for-bit unchanged.

`init_locals_pooled` is now a one-line wrapper over `init_locals_from_parts`, so
the pooled and unpooled arms cannot drift; the redundant `init_locals` was
removed.

### 5.2 `padded_bytecode` per-frame copy

`padded_bytecode` copies the whole method body into a fresh `Arc<[u8]>` on every
call. `Frame::new` did that per frame; so does `interpreter.rs:7208` on the
uncached invoke path.

**Content-addressed interning would be unsound here** and was rejected:
`runtime/local_liveness.rs:96` caches a per-method `LivenessTable` under the key
`(Arc::as_ptr(code), code.len())`, validated by `Arc::ptr_eq`, and computes it
from `analyze(code, exception_table)` — which depends on the method's
**exception table**, not just the bytes. Two distinct methods can have
byte-identical `Code` and different handler ranges; merging their Arcs would
serve method B's liveness from method A's handler edges, under-approximating the
live-locals mask and dropping a GC root.

There is now a *second* consumer of `Frame::code` pointer identity, landed on
`dev` after this wave's brief was written: `reader/src/quickened.rs::intern`
keys the quickened instruction stream on `(code.as_ptr(), code.len())`
(`interpreter.rs:7872 quickened_for_frame`, dispatch site `:10364-10366`). That
one is content-only — `QuickenedCode::build` is a pure pre-decode of
`Instruction::decode` over the bytes, with no constant-pool dependence — and its
`Some` entries pin a strong `Arc<[u8]>`, so address recycling cannot produce a
wrong hit. Method-identity keying is therefore safe for it too, and is in fact
**complementary**: two frames of the same method now share one code allocation,
so the quickened table hits where it previously rebuilt and re-interned under a
fresh address. (Content-addressed interning would also have been safe for
*this* table — it is `local_liveness` that rules it out.)

So the memo is keyed on **method identity**:

```rust
pub fn padded_bytecode_for_method(
    class_id: ClassId,
    method_name: &str,
    method_descriptor: &str,
    code: &[u8],
) -> Arc<[u8]>
```

Key `(class_id.as_u32(), fnv1a64(name ++ descriptor))`; a hit is additionally
verified against the actual bytes, which covers both a same-class hash collision
and class redefinition/retransformation rewriting the body under an unchanged
identity (a mismatch mints a fresh Arc and replaces the entry — old frames keep
the old Arc, and the liveness cache's `Weak` + `ptr_eq` guard sees a different
pointer and recomputes). Bounded at 16384 entries / 32 KiB bodies, mirroring
`local_liveness.rs`'s bounded-cache policy.

`Frame::new` already uses it. **`interpreter.rs` adoption is one line** (§6.2) —
that call site is not mine.

---

## 6. Spec for the next-wave `interpreter.rs` owner

### 6.1 Hoisting the 622 re-indexes

The API to adopt, in the order it should be used:

```rust
// ── once, before the dispatch loop for `frame_idx` ────────────────────
thread.frames.reserve_stable(1);          // next push cannot relocate
let epoch = thread.frames.reloc_epoch();
let fp: *mut Frame = thread.frames.frame_ptr(frame_idx);   // null iff OOB
debug_assert!(!fp.is_null());

// ── per opcode, replacing `thread.frames[frame_idx]` ──────────────────
// SAFETY: see the four rules below.
let frame: &mut Frame = unsafe { &mut *fp };
```

| Method | Signature | Meaning |
|---|---|---|
| `reserve_stable` | `fn(&mut self, additional: usize)` | after this, `additional` pushes cannot move any frame |
| `stable_headroom` | `fn(&self) -> usize` | pushes currently guaranteed relocation-free |
| `reloc_epoch` | `fn(&self) -> u32` | bumped iff frames moved |
| `frame_ptr` | `fn(&mut self, idx: usize) -> *mut Frame` | address of frame `idx`; null if `idx >= len()` |
| `current_ptr` | `fn(&mut self) -> *mut Frame` | address of the top frame; null if empty |
| `current_mut` | `fn(&mut self) -> Option<&mut Frame>` | safe top-frame borrow (cannot be held across a push) |
| `as_mut_ptr` | `fn(&mut self) -> *mut Frame` | base pointer, for re-deriving cheaply |

**Aliasing rules — all four must hold at every dereference:**

1. **No overlapping reference to the same frame.** While `fp` is in use there
   must be no live `&Frame` / `&mut Frame` for that frame — that includes
   `thread.frames[idx]`, `.last()`, `.last_mut()`, `.iter()`, and the `&[Frame]`
   view that `Deref` produces. So no
   `stackwalker::capture_full_trace(&thread.frames)` while `fp` is live.
   *Other* frames are unaffected: `thread.frames[caller_idx]` alongside a
   pointer to `frame_idx` is fine.
2. **Re-derive after a `&mut` reborrow of the stack.** Under Stacked/Tree
   Borrows a raw pointer derived from one `&mut FrameStack` is invalidated by a
   later `&mut` reborrow of the same place. The *address* is unchanged (that is
   this wave's guarantee), so re-deriving with `frame_ptr(idx)` costs one load
   plus one add and still removes the bounds check and the `imul`. Caching the
   pointer across a whole dispatch iteration — rather than across the whole
   `execute_frame` — captures most of the win with the least risk.
3. **Epoch unchanged.** `debug_assert_eq!(epoch, thread.frames.reloc_epoch())`
   before reusing a cached pointer across anything that could push. Cheap enough
   to keep in release on the invoke path if wanted.
4. **Frame still live.** `frame_idx < thread.frames.len()`.

Suggested landing order, smallest blast radius first:

1. `interpreter.rs:10364-10400` only — one `let frame = …` hoisted across the
   eight re-indexes in the single dispatch site (`code.as_ptr()`,
   `quickened_for_frame`, the two in `Instruction::decode`, the three in its
   error-message closure, and the `pc` write-back). Measure.
2. The straight-line opcode arms that already do
   `let frame = &mut thread.frames[frame_idx];` (e.g. `interpreter.rs:8153`,
   `:12384`) — pure win, no aliasing change.
3. The invoke path around `interpreter.rs:7176-7213`, using `reserve_stable(1)`
   + epoch assert.

Do **not** hoist across `capture_stack_trace`, GC root scans
(`scan_frame_roots`, `interpreter.rs:3347/3365/3831`), exception unwinding, or
JVMTI callbacks — they all take `&thread.frames` or iterate it, which violates
rule 1.

### 6.2 One-line bytecode fix on the uncached invoke path

`interpreter.rs:7208` — inside the `Frame::new_from_arcs(...)` call that starts
at `:7202`, in `pub fn execute` (`:4956`) — currently reads:

```rust
crate::runtime::frame::padded_bytecode(&code_attr.code),
```

`class_id: ClassId`, `method_name: &str` and `method_descriptor: &str` are all
`execute`'s own parameters and are in scope at that line (verified on
`6495a191c`). They are only *borrowed* by the adjacent
`Arc::from(method_name)` / `Arc::from(method_descriptor)` arguments, so
evaluation order is not a problem. Replace with:

```rust
crate::runtime::frame::padded_bytecode_for_method(
    class_id,
    method_name,
    method_descriptor,
    &code_attr.code,
),
```

This removes a full method-body memcpy plus an allocation per uncached method
entry. Semantics are identical: same bytes, same `>= 2` trailing zero bytes,
same `Arc<[u8]>`, and distinct methods still get distinct Arcs (which is what
`local_liveness.rs` requires — see §5.2).

The other `padded_bytecode` call sites (`jit/helpers.rs`, and
`interpreter.rs:5854/23591/31228/31288/32238/35300/36186/36762/41510`) run once
per method *resolution*, not per frame, and should stay as they are.

### 6.3 Optional: pooling the uncached invoke path further

`Frame::new_from_arcs` still builds four `Arc::from(&str)` per frame
(`interpreter.rs:6976-6980`). Those, not the bytecode, are now the dominant
per-frame allocation on that path. The fix is a resolved-method cache
(`CachedBytecodeMethod` already does exactly this for the cached path), which
lives in `classloading/`, not here.

---

## 7. Tests added

`vm/src/runtime/frame.rs`:

* `frame_addresses_are_stable_across_a_reserved_deep_push_sequence` — 4096
  pushes after `reserve_stable(4096)`; every recorded address must still match,
  and `reloc_epoch` must not move. Also checks addresses survive pops.
* `frame_ptr_stays_valid_across_pushes_and_pops` — writes through a raw pointer
  to the bottom frame, pushes and pops 48 frames above it, checks the pointer
  and the written value; checks the null contract for OOB / empty.
* `growth_is_the_only_relocation_and_always_bumps_the_epoch` — first push
  reserves without a bump (nothing to move); filling to capacity does not bump;
  the overflowing push does; `truncate`/`clear` keep capacity and do not bump.
* `frame_stack_is_a_drop_in_for_vec_frame` — `&FrameStack → &[Frame]` coercion,
  range indexing, `iter().enumerate().rev()`, `for f in &frames`,
  `for f in &mut frames`, `last`/`last_mut`/`get`/`get_mut`/`pop`.
* `tls_soa_pool_is_reused_and_leaves_no_stale_state` — a dirtied frame's buffers
  are pooled, reused by the next `Frame::new`, and carry no stale locals, no
  stale kind marks and an empty operand stack.
* `pooled_and_unpooled_frames_are_equivalent` — same method built with an empty
  pool and with a dirty pooled buffer must produce identical locals.
* `tls_soa_pool_declines_oversized_buffers`, `tls_soa_pool_is_capped`.
* `padded_bytecode_memo_returns_the_same_arc_for_the_same_method`,
  `padded_bytecode_memo_never_merges_distinct_methods`,
  `padded_bytecode_memo_detects_a_redefined_body`,
  `frame_new_shares_bytecode_across_frames_of_the_same_method`.

`vm/src/runtime/value_stack.rs`: the five `kinds`-invariant tests listed in §4.

---

## 8. Not done / follow-ups

* **`ValueStack::kinds` / `Frame::local_kinds` removal** — blocked on the
  interpreter consuming `classloading/src/type_maps.rs::MethodTypeMaps` (§4).
  A cheaper interim that does *not* change soundness, if someone wants it: keep
  the semantics but store the marks as two inline `u64` bitmasks
  (`long_mask` / `double_mask`) instead of a `Vec<u8>`. For `max_size <= 128`
  — essentially every method — that removes the second allocation and the second
  cache line entirely. It touches ~38 `self.kinds[…]` sites and was judged too
  large to land blind in a no-build wave; it should be measured first.
* **`interpreter.rs` adoption** of §6.1 and §6.2 — cannot be done from this
  wave's file set.
* **Uncached-invoke `Arc::from(&str)` churn** (§6.3) — needs a resolved-method
  cache in `classloading/`.
* **Sizing `FRAME_STACK_INITIAL_STABLE_CAP`** is a guess (256). If a profile
  shows relocations happening in steady state on a real workload, raise it or
  derive it from `config.max_stack_depth` with a per-thread cap.
* **Quickening interaction, worth measuring.** `Frame::new` now shares one code
  allocation across frames of the same method, which should raise the hit rate
  of `reader/src/quickened.rs::intern` on `Owned` frames. Adopting §6.2 extends
  that to the uncached invoke path, where it matters most. Nobody has measured
  it; the mechanism is sound either way.

---

## 9. Merge / basis note

This branch was originally cut from `e4e4053bb` (= `origin/main`, 2026-07-23),
not from `dev`. It was merged onto `dev` = `6495a191c34fcf701c7c168517e6a13f462fc3dc`
(clean, no conflicts) and every survey and citation above was **re-derived on
the merged tree**, not on the original base. Specifically re-verified after the
merge:

* the repo-wide `.frames` method surface (§3) — unchanged set, so the
  `FrameStack` API is still a complete drop-in across the +3,919 lines of
  `interpreter.rs` drift;
* the 622 `thread.frames[frame_idx]` count (identical) and the dispatch-site
  re-index count (6 → 8, because of quickening);
* the uncached invoke path moving `6981 → 7208`, and that `class_id`,
  `method_name: &str`, `method_descriptor: &str` are all still in scope there;
* `local_liveness.rs:96` (unchanged) and the newly-landed
  `reader/src/quickened.rs::intern` — the second code-pointer-keyed cache, which
  did not exist when the brief was written and which this design had to be
  re-checked against (§5.2);
* `dev`'s own edits to the two owned files (`frame.rs`: the
  `class_disables_interp_fast_path` doc rewrite plus the new
  `Frame::cached_method()`; `jvm_thread.rs`: the `handle_slots` /
  `handle_scope_bases` fields) — neither conflicts with anything here.
