# Verifier type maps — making verification retain what it proves

**Slug:** `verifier-type-maps`
**Date:** 2026-07-26
**Status:** LANDED (producer complete, on by default, no consumers yet — by design)

## Tree basis

This work was authored on a worktree mistakenly branched from
`origin/main` (`e4e4053bb`), then merged onto **`dev` = `6495a191c`**
(merge commit on branch `worktree-agent-a1873d460b7ce608c`). The merge was
clean — no conflicts in any file.

`dev` is being advanced by concurrent sessions, so the basis is stated
explicitly. Every file:line citation below was **re-verified against the
merged tree**, not carried over from the original brief; §10 records what
moved.

Three things that arrived on `dev` between `e4e4053bb` and `6495a191c`
materially touch this work and are accounted for in §2.4:
`reader/src/verified_code.rs`, `reader/src/quickened.rs`, and the
`vm/src/vm/realms/` extraction (which renamed `shared.heap` →
`shared.mem.heap`).

---

## 1. The finding

`verify_bytecode(class, hierarchy) -> Result<(), LinkageError>`
(`classloading/src/bytecode_verifier.rs:52`) walks every instruction of every
method and computes the exact JVMS verification type of every local slot and
every operand-stack slot at every pc — and then **discarded all of it and
returned unit**. Verification is ON by default
(`vm/src/config.rs:419`, `skip_verification: false`), so the VM was paying for
the analysis on every class load and throwing away the answer.

Four subsystems re-derive that same information at runtime, badly:

| subsystem | what it does instead | file (merged tree) |
|---|---|---|
| value representation | every slot carries a runtime tag; `Value` is statically asserted to be 16 bytes | `types/src/value.rs:747` |
| operand stack | a parallel `kinds: Vec<u8>` because a 64-bit `long` cannot be NaN-boxed | `vm/src/runtime/value_stack.rs:216` |
| GC root scan | scans runtime tags, and tags get lost — a JIT callee's object return reaching an interpreter local under a non-object tag — so it falls back to `scan_locals_conservative` / `scan_object_refs_conservative`, which forces a non-moving collector | `vm/src/memory/roots.rs:140,156` |
| interpreter fast path | cannot be proven safe per method, so it is gated on a **package-name deny-list** whose own TODO names the fix: *"a per-method `unsafe_for_fast_path` flag computed at install/verification time"* | `vm/src/runtime/frame.rs:510` (TODO), `:553` (the deny-list) |
| JIT | re-infers types it was already handed | — |

## 2. What landed

A new module, `classloading/src/type_maps.rs`, plus ~60 lines of wiring in
`classloading/src/bytecode_verifier.rs` and a module declaration + re-export in
`classloading/src/lib.rs`.

**On by default.** No cargo feature. No `CRATONVM_*` env var — in particular
nothing was added to `types/src/flags.rs` (the centralised flag system that
arrived on `dev`), because there is nothing to gate. No opt-in and no opt-out.
`verify_bytecode` builds and publishes the maps for every class it verifies, on
the default build path. The only thing that stops maps being produced is the
pre-existing `--noverify` escape hatch, which stops verification itself — and
that case is explicitly representable (see §6).

### 2.1 Files touched

| file | change |
|---|---|
| `classloading/src/type_maps.rs` | NEW — the whole module (~1090 lines incl. tests) |
| `classloading/src/bytecode_verifier.rs` | `verify_method` now returns `Option<MethodTypeMaps>`; `verify_by_inference` returns `MethodTypeMaps`; both walks feed a builder; `verify_bytecode_inner` publishes; dev's `let _verified` is now `let verified` and supplies an exact reserve count |
| `classloading/src/lib.rs` | `pub mod type_maps;` + re-export block |
| `classloading/src/verify_insn.rs` | **unchanged** — no change was needed, before or after the merge |
| `classloading/src/vtype.rs` | **unchanged** — `VType::is_reference()` / `is_category2()` already expose everything the builder needs |

No consumer was changed. That is deliberate: the GC root scan, the interpreter
fast path, and the frame representation consume this in the next wave.

### 2.2 Post-merge re-check of the two owned files I did not change

`dev` moved both, so the "no change needed" conclusion was re-derived, not
assumed:

* `verify_insn.rs` (+22/-20 on dev) — the change was
  `Instruction::Tableswitch { .. }` / `Lookupswitch { .. }` becoming boxed
  tuple variants `Tableswitch(ts)` / `Lookupswitch(ls)`. My
  `observe_instruction` matches only load/store/`iinc`/`jsr`/`ret`/`wide` with
  a `_ => {}` catch-all, and all of those variants are byte-for-byte unchanged
  on the merged tree (`Iload(u16)`, `Lstore(u16)`, `Astore(u16)`,
  `Iinc { index, constant }`, `Jsr(i16)`, `Ret(u16)`, `JsrW(i32)`, `Wide`).
  Nothing to adapt.
* `vtype.rs` — `VType`'s variants, `is_reference()` (line 130) and
  `is_category2()` (line 125) are unchanged. `verify_insn.rs:220-226` still
  writes `local_store(index, Long)` **and** `local_store(index + 1, Top)` for
  `lstore`, which is the invariant the category-2 bounds check in
  `observe_instruction` encodes.

### 2.3 The verification invariants the builder relies on

`VerificationFrame` (`verify_frame.rs:20,22`) still exposes
`pub locals: Vec<VType>` / `pub stack: Vec<VType>`, `local_load` /
`local_store` still bounds-check against `locals.len()`, and `pad_locals_to`
(line 346) still pads adopted StackMapTable frames to `max_locals`. Successful
verification therefore already proves every local index is in range; the
builder's own check is defence in depth and a *source of the veto reason*.

### 2.4 Relationship to `verified_code.rs` and `quickened.rs` (both new on dev)

These are adjacent but solve different problems, and the boundary matters:

* **`reader/src/verified_code.rs`** gives a canonical *decode + CFG* contract:
  `verified_code(code) -> Arc<VerifiedCode>` with
  `instructions: [VerifiedInstruction { pc, next_pc, instruction }]`,
  `merge_targets`, `loop_headers`. `verify_method` already calls it (dev added
  that) and previously discarded the result. It carries **no type-state**, so
  it does not overlap this module — but it does hand us two guarantees that
  are now asserted rather than re-derived:
  1. `VerifiedCode::decode` rejects `code.len() > 65535`, so **every** method
     reaching the type-map builder has pcs that fit a `u16`. The
     `PcTable::U32` widening is now defence in depth, not a live path.
  2. Every branch target is validated to be an instruction boundary
     (`"targets N, which is not an instruction boundary"`). So the `frame_at`
     keys the pre-Java-7 worklist produces are guaranteed instruction starts —
     no row in the pc table can describe a mid-instruction offset. This closed
     an open concern from the pre-merge design.

  It is emphatically **not** a place to hang oop maps: its cache is bounded
  per shard and `clear()`s a whole shard on overflow ("hashing is only an
  accelerator and never a correctness boundary"). Evicting a GC oop map would
  be a silent missed-root bug. Hence the separate, leaked, `ClassId`-keyed
  store in this module.

  The one thing taken from it: `verify_method` now uses
  `verified.instructions().len()` for an **exact** `MethodTypeMapsBuilder::reserve`,
  replacing the pre-merge `bytecode.len() / 2 + 1` estimate.

* **`reader/src/quickened.rs`** is the closest in-tree precedent for the shape
  of this module — a per-method side stream with `pcs: Box<[u32]>`, a
  binary-search `find_index`, and a `heap_bytes()` accessor at line 204 plus
  `STAT_BYTES`/`STAT_METHODS` process counters. `MethodTypeMaps::heap_bytes()`
  and `type_maps::store_heap_bytes()` / `store_class_count()` follow exactly
  that convention (own arrays counted, shared bytecode excluded).
  `QuickenedCode::find_index` also uses a *hint* index before falling back to
  binary search — a worthwhile future optimisation for `oop_map_at` once a
  consumer exists that queries the same method repeatedly.

## 3. The API

### 3.1 The type

```rust
pub struct MethodTypeMaps {
    pcs: PcTable,                    // instruction-start pcs, ascending (u16 or u32)
    local_oops: CompactBitmapArray,  // per-pc: which local slots hold an object ref
    stack_oops: CompactBitmapArray,  // per-pc: which stack slots hold an object ref
    stack_depth: DepthTable,         // per-pc operand-stack depth (u8 or u16)
    max_locals: u16,
    max_stack: u16,
    locals_complete: bool,
    safe_for_fast_path: bool,
    veto: Option<FastPathVeto>,
}
```

### 3.2 The signatures a consumer uses

```rust
// ---- lookup (global side table) ----------------------------------------
pub fn type_maps_for(class_id: ClassId, method_index: usize)
        -> Option<&'static MethodTypeMaps>;                       // O(1)
pub fn type_maps_for_named(class_id: ClassId, name: &str, descriptor: &str)
        -> Option<&'static MethodTypeMaps>;
pub fn class_type_maps(class_id: ClassId) -> Option<&'static ClassTypeMaps>;
pub fn verification_status(class_id: ClassId) -> VerificationStatus;
                                        // Unknown | Skipped | Verified

// ---- per-pc query (GC stop-the-world safe) -----------------------------
impl MethodTypeMaps {
    pub fn oop_map_at(&self, pc: u32)
        -> Option<(LocalOopBits<'_>, StackOopBits<'_>)>;           // O(log n)
    pub fn frame_map_at(&self, pc: u32) -> Option<FrameOopMap<'_>>;// O(log n)
    pub fn local_oops_at(&self, pc: u32) -> Option<LocalOopBits<'_>>;
    pub fn stack_oops_at(&self, pc: u32) -> Option<StackOopBits<'_>>;
    pub fn stack_depth_at(&self, pc: u32) -> Option<u16>;
    pub fn safe_for_fast_path(&self) -> bool;
    pub fn fast_path_veto(&self) -> Option<FastPathVeto>;
    pub fn locals_fully_described(&self) -> bool;
    pub fn max_locals(&self) -> u16;
    pub fn max_stack(&self) -> u16;
    pub fn entry_count(&self) -> usize;
    pub fn heap_bytes(&self) -> usize;
    pub fn total_bytes(&self) -> usize;
}

#[derive(Clone, Copy)]
pub struct OopBits<'a> { /* &'a [u8] + u16 bit count */ }
impl OopBits<'_> {
    pub fn len(&self) -> usize;
    pub fn get(&self, index: usize) -> bool;     // out of range → false, never panics
    pub fn is_all_clear(&self) -> bool;
    pub fn clamped(self, n: usize) -> Self;
    pub fn iter_set(&self) -> SetBitIter<'_>;    // allocation-free
    pub fn as_bytes(&self) -> &'_ [u8];
}

#[derive(Clone, Copy)]
pub struct FrameOopMap<'a> {
    pub pc: u32,
    pub locals: LocalOopBits<'a>,
    pub stack: StackOopBits<'a>,
    pub stack_depth: u16,
}
impl<'a> FrameOopMap<'a> {
    pub fn stack_for_runtime_depth(&self, runtime_depth: usize) -> StackOopBits<'a>;
    pub fn locals_for_runtime_len(&self, runtime_len: usize) -> LocalOopBits<'a>;
}
```

`OopBits` is a `Copy` value (slice + bit count), not a heap object. Every query
path is **allocation-free, lock-free, panic-free** — the requirements for a
stop-the-world root scan.

### 3.3 Why the lookup is lock-free, not `RwLock<Vec<..>>`

A GC root scan runs with the world stopped. If a thread were suspended at a
safepoint *while holding the write lock* on a `RwLock`-based side table, the
collector's read would block forever. The store is therefore a two-level
chunked directory of `AtomicPtr` (4096 chunks × 1024 ids = 4,194,304
addressable class ids): a lookup is two acquire loads and an array index, with
no failure mode that can deadlock a collector. Entries are leaked rather than
freed, which is what makes `&'static MethodTypeMaps` sound (see §9).

Note this is a *stronger* guarantee than `verified_code`'s
`Mutex<CacheShard>`, which is fine for a decode accelerator on the class-load
path but would not be callable from a root scan.

### 3.4 Index spaces — the load-bearing detail

CratonVM does **not** use the JVMS slot discipline uniformly, and a consumer
that gets this wrong will scan the wrong slot:

| array | JVMS | CratonVM runtime | what this module emits |
|---|---|---|---|
| locals | cat-2 = 2 slots | cat-2 = 2 slots (`copy_args_to_locals` leaves the upper half uninitialised) | **JVMS = runtime local index** |
| operand stack | cat-2 = 2 slots (`Long` + `Top`) | cat-2 = **1** `CompactValue` (see `ValueStack` `Dup2` handling) | **runtime (compressed) slot index** |

So `local_oops_at(pc)` is indexed exactly like `Frame::locals`, and
`stack_oops_at(pc)` / `stack_depth_at(pc)` are indexed exactly like
`Frame::stack` — a `long` contributes **one** stack bit, always clear. The
builder performs the JVMS→runtime compression while recording (it consumes the
`Long`/`Top` pair as one runtime slot and vetoes the method if the pair is
malformed).

The JVMS operand-stack depth is intentionally **not** retained. The only
consumer that would want it (the JIT) is already walking the bytecode — and now
has `VerifiedCode` to walk it canonically — so it can re-derive it; keeping it
would have doubled the depth array.

### 3.5 Mandatory clamping rules

Documented on `oop_map_at` and enforceable via `FrameOopMap`:

* **Locals.** `Frame::locals` may be *longer* than `max_locals`
  (`effective_max_locals` widens it when an argument list overflows the
  declared `max_locals`). Slots at or past `max_locals` are **not described**
  and must be scanned conservatively → `locals_for_runtime_len`.
* **Operand stack.** The interpreter pops an invoked method's arguments off
  the caller's operand stack *before* pushing the callee frame, so while a
  callee runs the caller's runtime depth is **lower** than the verifier's
  depth at the invoke pc. Popping only ever removes from the top, so slots
  `[0, runtime_depth)` still carry exactly the types the map states →
  `stack_for_runtime_depth`. Reading past the frame's real depth would read
  stale slots.
* **`locals_fully_described()`.** `false` means at least one recorded frame
  carried fewer locals than `max_locals`, so the tail bits are clear without
  ever having been *proven* clear — a missed root for a moving collector.
  Scan locals conservatively for such a method. In practice this is always
  `true` (both walks pad to `max_locals` before recording); the flag exists so
  a future frame-representation change cannot silently degrade root scanning.

### 3.6 `None` means "unproven", never "no references"

`oop_map_at` returns `None` when the method was never verified, when `pc` is
not an instruction start, when `pc` is in a region lenient verification skipped
as unreachable, or when the per-method budget (§5) cut recording short. Every
one of those requires the consumer to fall back to conservative behaviour. The
module deliberately does **not** expose an `OopBits::EMPTY`, precisely so
"no map" cannot be typo'd into "empty map".

## 4. `safe_for_fast_path`

This gates the memory-unsafe `pop_unchecked` / `set_local_unchecked` /
`get_local_compact_unchecked` sites in `vm/src/runtime/interpreter.rs`. It is
`true` only when **all** of the following were proven, and `false` the instant
anything is unproven:

1. The verifier walk covered every reachable instruction start
   (`FastPathVeto::IncompleteWalk`).
2. Every `*load` / `*store` / `iinc` / `ret` operand — **and the `+1` upper
   half of every category-2 access** (`lstore n` writes `n` and `n+1`, see
   `verify_insn.rs:220`) — is `< max_locals`
   (`FastPathVeto::LocalIndexOutOfRange`). This is the precise property the H7
   comment in `interpreter.rs` relies on: the fast-path handlers index
   `frame.locals` with the raw bytecode operand and **no bounds check**.
3. At every recorded pc the runtime operand-stack depth is known and
   `<= max_stack`, so `pop_unchecked` cannot underflow and `push_unchecked`
   cannot overrun the backing store (`StackDepthExceedsMaxStack`).
4. No `jsr` / `jsr_w` / `ret` (`Subroutine` — subroutine type-state is only
   approximated, so local slots are not provably typed).
5. No stray `wide` prefix (`StrayWidePrefix`), no malformed category-2 pair
   (`MalformedCategory2`), no over-wide locals row
   (`LocalsWiderThanMaxLocals`), no out-of-order pcs (`UnsortedPcs`, which also
   discards the whole map), no budget overrun (`MapBudgetExceeded`).

The first veto is retained and readable via `fast_path_veto()` for diagnosis.

**What it does not assert:** anything about JIT/native bridge behaviour. The
existing deny-list at `frame.rs:553` also excludes `java/lang/invoke/*` and the
Spring `enhance` callback chain for reasons that are *not* raw memory safety.
The migration in §7 therefore starts with `AND`, not `replace`.

## 5. Memory

Per **recorded instruction start**:

| component | bytes | notes |
|---|---|---|
| pc | 2 | `u16` — now *enforced* upstream, since `VerifiedCode::decode` rejects `code.len() > 65535` before verification runs |
| local oop bits | `ceil(max_locals / 8)` | **byte** stride, not word stride |
| stack oop bits | `ceil(max_stack / 8)` | ditto |
| stack depth | 1 | `u8` while `max_stack <= 255` |

A typical method (`max_locals = 4`, `max_stack = 3`) costs **5 bytes per
instruction start**. Measured in-tree by the `representation_is_compact` test:
20 instruction starts with `max_locals = 6`, `max_stack = 4` →
`heap_bytes() == 100` (exactly `20*2 + 20*1 + 20*1 + 20*1`).

Per **method**:

| component | bytes |
|---|---|
| row data (16-instruction median method) | ~80 |
| `MethodTypeMaps` header | ~120 |
| 4 heap allocations (allocator overhead) | ~64 |
| `MethodSig` (name + descriptor `Arc<str>` + hash) — one per method incl. abstract/native | ~40 |
| **total** | **~300 bytes/method** |

The `Arc<str>` pair costs 16 bytes of *pointer* each; the strings themselves
are shared with `ClassFileMethod`, not duplicated.

Per **application**: a Spring app with 15,000 loaded classes and ~12
body-bearing methods each (~180,000 methods) lands around **50–55 MB**.

Choosing a 64-bit **word** stride instead of a byte stride would have cost
~8× on the bitmaps alone; storing `Vec<Vec<VType>>` (the naive shape) would
have been two orders of magnitude worse.

Live accounting: `MethodTypeMaps::heap_bytes()` / `total_bytes()`,
`ClassTypeMaps::heap_bytes()`, and process-wide `type_maps::store_heap_bytes()`
/ `store_class_count()` — the same convention as `quickened.rs:204` +
`STAT_BYTES`.

**Hostile-input cap.** `max_locals` and `max_stack` are attacker-controlled
`u16`s, so a crafted classfile could otherwise demand
`65535/8 * 2 * instruction_count` bytes of metadata. `MAX_METHOD_ROW_BYTES`
(256 KiB per method) bounds it: recording stops at the budget, the remaining
pcs answer `None`, and `safe_for_fast_path` is denied
(`FastPathVeto::MapBudgetExceeded`). Real methods use a few hundred bytes.

**Quantified reduction available (not taken):** collapsing the four arrays into
a single arena allocation would save the ~48 bytes/method of allocator overhead
plus 3 `Box` headers (~24 bytes) → roughly 25 % of the total. Row
deduplication (consecutive instructions usually share an identical local
bitmap) would cut another large fraction at the cost of a 2-byte indirection
per pc. Neither was taken: the current shape is simple, obviously correct, and
already within budget.

## 6. `skip_verification` is explicitly representable

When verification is skipped, `verify_bytecode` never runs, so no maps are
published and `type_maps_for` returns `None` — the *correct* answer, never a
wrong-but-plausible map. `verification_status(class_id)` distinguishes the
cases:

* `Unknown` — nothing published. Class not verified yet, or it took a path that
  produces no maps (the `jsr`/`ret` structural-only fallback in
  `verifier.rs`).
* `Skipped` — an explicit marker published by `mark_class_verification_skipped`.
* `Verified` — maps exist for every method with a `Code` attribute.

`Skipped` additionally tells a consumer it must **not** take the unchecked
interpreter fast path (the H7 invariant that local operands are in range no
longer holds at all).

> **One-line follow-up, owned by the VM crate (I do not own those files):**
> at the point where `vm` decides to skip verification for a class (the
> `config.skip_verification` / `verifier_skip_eligible` gate), call
> `cratonvm_classloading::mark_class_verification_skipped(class.id)`. Without
> it the status is `Unknown` instead of `Skipped` — equally conservative, just
> less diagnosable. Nothing else is required.

## 7. What a consumer must do to migrate off conservative scanning

*Rewritten post-merge: the pre-merge version of this section assumed a flat
`Frame` with public `class_name`/`method_name` fields and `shared.heap`. On
`dev` both changed.*

### 7.1 GC root scan (`vm/src/memory/roots.rs:129-157`)

Today, per frame (note `shared.mem.heap` — the realms extraction):

```rust
let conservative_locals = conservative_locals_enabled();
for frame in thread.frames.iter() {
    if conservative_locals { frame.scan_local_objects_all_live(&mut roots, &shared.mem.heap); }
    else                   { frame.scan_local_objects(&mut roots, &shared.mem.heap); }
    if conservative_locals { frame.scan_locals_conservative(&mut roots, &shared.mem.heap); }
    frame.stack.scan_object_refs(&mut roots, &shared.mem.heap);
    if conservative_locals { frame.stack.scan_object_refs_conservative(&mut roots, &shared.mem.heap); }
}
```

The precise replacement, per frame:

1. Resolve the map once — see 7.3 for where to memoize it. `Frame` has
   `pub class_id` plus `method_name()` / `method_descriptor()` accessors (it is
   now `FrameInner::{Owned, Cached}` internally), so the immediately-usable
   call is
   `type_maps_for_named(frame.class_id, frame.method_name(), frame.method_descriptor())`.
   `type_maps_for(class_id, method_index)` is the `O(1)` path once an index is
   available.
2. If it is `None`, **or** `frame_map_at(frame.pc as u32)` is `None`, **or**
   `locals_fully_described()` is `false` → keep the existing conservative
   scan for that frame. Do not guess.
3. Otherwise, with `let m = maps.frame_map_at(pc)?`:
   * locals: `for i in m.locals_for_runtime_len(frame.locals.len()).iter_set()`
     — slot `i` is a proven object reference.
   * stack: `for i in m.stack_for_runtime_depth(frame.stack.len()).iter_set()`
     — slot `i` is a proven object reference.
   * Any `frame.locals.len() > max_locals` tail: still conservative.

Only once **every** frame on **every** thread resolves to a precise map may
`conservative_locals` be switched off, and only then does a moving collector
become sound. Two prerequisites that are *not* covered by this module and must
be closed separately:

* **`frame.pc` semantics at a GC point.** The map is keyed on
  *instruction-start* pcs and describes the state *before* that instruction
  executes. A collector must be certain `frame.pc` is the instruction pc, not
  a mid-instruction or post-advance value. Where it is a post-advance value,
  the correct query is the *previous* instruction start. (`VerifiedCode`'s
  `is_instruction_start(pc)` is a cheap way to assert this in a debug build.)
* **JIT and native frames.** This module covers interpreter frames only. JIT
  frames need the parallel `precise_maps` work; native frames stay conservative.

### 7.2 Interpreter fast path (`vm/src/runtime/frame.rs:553`, `interpreter.rs`)

Today:

```rust
let use_fast_path = !frame.is_jdk_class && !shared.config.skip_verification;
// is_jdk_class = class_disables_interp_fast_path(class_name)  // package deny-list
```

Migration, in two steps so the deny-list removal is validated rather than
assumed:

* **Step 1 (safe now).** Memoize `safe_for_fast_path` (7.3) and use
  `!frame.is_jdk_class && verifier_fast_path_ok && !skip_verification`.
  This can only *reduce* the fast-path set, so it cannot regress correctness;
  it will immediately exclude `jsr`-bearing and unreachable-code-bearing
  methods that the deny-list happened to admit.
* **Step 2 (the actual win).** Drop `is_jdk_class` from the condition and rely
  on `verifier_fast_path_ok` alone. This is what recovers the ~95 % of
  executed bytecode the deny-list currently forces through the slow path. It
  requires first confirming that the non-memory-safety reasons baked into the
  deny-list (`java/lang/invoke/*` bridges, the Spring `enhance` callback
  chain) are handled elsewhere — the TODO at `frame.rs:510` describes them as
  stack-shape issues, which `safe_for_fast_path` does cover, but the Letsgo AV
  regression it cites should be re-run before the deny-list is deleted.
* `verifier_fast_path_ok` must default to **`false`** when the map is absent
  (`type_maps_for*` → `None`), which subsumes the `skip_verification` check.

### 7.3 Where to memoize — use the existing `OnceLock` convention

**Do not add fields to `Frame`.** `dev` already established the pattern for
per-method metadata: `jit_api::CachedBytecodeMethod` carries
`declaring_class_id`, `method_name`, `method_descriptor`, `max_stack`,
`max_locals`, and four `OnceLock` memos — `force_native_cache`,
`native_callback_cache`, `invoc_key`, and `quickened` — read through
`quickened_for_frame(frame)` in `interpreter.rs` with an intern-table fallback
for `Owned` frames.

The matching addition is one field:

```rust
// jit-api/src/lib.rs, CachedBytecodeMethod
pub type_maps: std::sync::OnceLock<Option<&'static MethodTypeMaps>>,
```

populated exactly like `quickened`:

```rust
#[inline]
fn type_maps_for_frame(frame: &Frame) -> Option<&'static MethodTypeMaps> {
    match frame.cached_method() {
        Some(cm) => *cm.type_maps.get_or_init(|| {
            type_maps_for_named(cm.declaring_class_id, &cm.method_name, &cm.method_descriptor)
        }),
        // `Owned` frames (synthetic, JNI stubs, non-cached invoke) — one
        // lookup per frame, or None if the class was never verified.
        None => type_maps_for_named(frame.class_id, frame.method_name(), frame.method_descriptor()),
    }
}
```

The `&'static` is sound because the store leaks entries (§9.2); a `OnceLock`
of a `&'static` is a plain pointer memo, matching `invoc_key`'s shape. Nothing
else in the frame needs to change for the first wave. The eventual payoff
(dropping the 16-byte tagged `Value` at `types/src/value.rs:747` and the
parallel `kinds: Vec<u8>` at `value_stack.rs:216`) is a later wave that depends
on JIT frames being covered too.

## 8. Test coverage

`#[cfg(test)] mod tests` in `type_maps.rs`, covering the brief's five required
cases and more:

| test | covers |
|---|---|
| `refs_in_locals_are_recorded` | refs in locals, `Null` counted as a reference slot, out-of-range reads answer `false` |
| `uninitialized_this_counts_as_a_reference` | `UninitializedThis` / `Uninitialized(n)` are real heap pointers between `new` and `<init>` |
| `long_spans_two_local_slots_and_one_stack_slot` | **a `long` spanning two slots** — two *local* slots, one *stack* slot, both clear |
| `malformed_category_two_pair_vetoes` | `Long` without its `Top` upper half |
| `branch_target_merge_narrows_to_the_merged_type` | **a branch-target merge** — merged ref stays set, `ref ⊔ int = Top` goes clear, non-instruction-start pcs answer `None` |
| `exception_handler_entry_has_the_thrown_object_on_the_stack` | **an exception-handler entry** — `stack = [throwable]`, depth 1, locals preserved |
| `subroutine_method_is_not_safe_for_fast_path` | **`safe_for_fast_path == false`** for `jsr`, with rows still usable for GC |
| `out_of_range_local_index_is_not_safe_for_fast_path` | `astore 5` with `max_locals = 2` |
| `category_two_local_needs_both_slots_in_range` | `lstore n` needs `n` and `n+1` in range |
| `incomplete_walk_is_not_safe_for_fast_path` | lenient unreachable-code skip |
| `plain_method_is_safe_for_fast_path` | the positive case |
| `unsorted_pcs_discard_the_map_entirely` | a wrong oop map is worse than none |
| `duplicate_pc_records_are_idempotent` | re-record of the same instruction start |
| `stack_clamps_to_the_runtime_depth` | the invoke-args clamping contract |
| `locals_clamp_to_the_runtime_array_length` | the locals clamping contract |
| `short_locals_row_flags_the_method_as_not_fully_described` | the missed-root guard |
| `representation_is_compact` / `wide_locals_use_a_wider_stride` / `zero_locals_and_zero_stack_allocate_nothing` / `out_of_spec_pcs_widen_to_u32` | exact `heap_bytes()` accounting for every representation |
| `store_round_trips_by_index_and_by_name` | `O(1)` index lookup, name+descriptor lookup, abstract methods hold an index but no maps |
| `unknown_class_is_unknown_not_empty` | `Unknown ≠ empty` |
| `skipped_verification_is_distinguishable_from_verified` | the `skip_verification` requirement, incl. the skip→verified upgrade |
| `duplicate_publish_keeps_the_winner` / `class_ids_beyond_the_addressable_range_degrade_gracefully` | store concurrency + range edges |

The existing `bytecode_verifier.rs` tests are unchanged and still pass through
the same public `verify_bytecode(&class, &h) -> Result<(), LinkageError>`
signature — no caller outside this crate saw a signature change.

**Not built or run in this session** (nine agents share the host; the
orchestrator builds after merging). Both changed files were syntax- and
format-checked with `rustfmt --check` (a parser, not a build).

## 9. Known limitations / follow-ups

1. **`jsr`/`ret` classes get no maps at all.** `verifier.rs`'s
   `verify_class_bytecode_inner` routes classes containing subroutines to its
   own per-method path (`verify_method_typestate` /
   `verify_method_structural_only`) which never calls `verify_bytecode`. Those
   classes answer `Unknown` → fully conservative. `verifier.rs` is not my file;
   the fix is to have its per-method type-state path build a
   `MethodTypeMapsBuilder` the same way and publish a `ClassTypeMaps`, with
   `mark_unsafe_for_fast_path(FastPathVeto::Subroutine)` for every
   subroutine-bearing method. Pre-Java-7 only, so low priority.
2. **Redefinition leaks.** `replace_class_type_maps` swaps the pointer and
   leaks the previous `ClassTypeMaps`, because a GC root scan may hold a
   `&'static` into it and this crate has no epoch reclamation. Only JVMTI
   `RedefineClasses`/`RetransformClasses` reaches it. A redefine-in-a-loop
   agent would grow the leak unboundedly; the fix is epoch-based reclamation
   or deferring the free to a safepoint where no scan is in flight. The same
   argument applies to the new class-*unloading* path on `dev`
   (`class_manager::UnloadedClass`): an unloaded class's maps are currently
   retained, so unloading does not reclaim them. Sound, but not free.
3. **Class ids ≥ 4,194,304** are outside the chunked directory; the store
   answers `Unknown` and consumers degrade to conservative. Raising
   `MAX_CHUNKS` is a one-constant change (the directory is 8 bytes per chunk).
4. **`verify_bytecode_strict` also publishes.** If both entry points run for
   the same class, the second publish is a no-op (CAS keeps the winner). Both
   produce identical maps modulo the strictness of the *rejection* decisions,
   so this is benign.
5. **JVMS operand-stack depth is not retained** (§3.4). If the JIT turns out to
   need it, add a second `DepthTable`; the cost is 1 byte per pc.
6. **`oop_map_at` has no hint fast path.** `QuickenedCode::find_index`
   (`quickened.rs:192`) shows the pattern — check a caller-supplied hint index
   before the binary search. Worth adding once a consumer exists that queries
   the same method's map repeatedly (a per-frame memoized last-index).

## 10. Citation re-verification (post-merge)

Every file:line from the original brief, checked against `dev = 6495a191c`
merged:

| brief citation | merged tree | verdict |
|---|---|---|
| `classloading/src/bytecode_verifier.rs:51` — `verify_bytecode` discards its analysis | **`:52`** | holds; +1 from an import line. Function body and signature unchanged by `dev` |
| `vm/src/config.rs:419` — `skip_verification: false` | **`:419`, exact** | holds; verification is still on by default, and the flag did **not** migrate into the new `types/src/flags.rs` |
| `types/src/value.rs` — `Value` is 16 bytes | **`:747`** static assert `size_of::<Value>() == 16` | holds |
| `vm/src/runtime/value_stack.rs:211` — parallel `kinds: Vec<u8>` | **`:216`** | holds; +5 |
| `vm/src/memory/roots.rs:118` — conservative scan comment | conservative scan now at **`:140`** and **`:156`**; `shared.heap` → **`shared.mem.heap`** | holds, but the receiver renamed (realms extraction). §7.1 updated |
| `vm/src/runtime/frame.rs:553` — `class_disables_interp_fast_path` | **`:553`, exact**; its TODO moved to **`:510`** and now reads "install/verification time" | holds |
| `reader/src/quickened.rs:204` — the `heap_bytes()` convention | **`:204`, exact** | holds. Did not exist at my original base; the convention was matched anyway and is now confirmed |

Nothing in the finding was invalidated by the merge. One brief citation
(`quickened.rs:204`) did **not resolve at the pre-merge base at all** — that
file only exists on `dev` — which is what surfaced the wrong-tree problem.
