# Architecture review A1–A9 — findings, fixes, and two corrections

**Slug:** `architecture-review-a1-a9`
**Date:** 2026-08-04
**Status:** LANDED for A1, A3, A4a, A6, A7, A8. **A5 WITHDRAWN** (the finding
was wrong; see §A5). **A2 and A4b NOT DONE** — scoped below with the specific
blocking detail each needs, deliberately not half-landed. **A9 advisory.**

## Tree basis

Authored on `fix/arch-review-a1-a9-20260804`, branched from `origin/dev` at
**`3db59eb9b`**. Every file:line citation below was re-verified against that
tree, and every number was produced by running something, not by reading.

Files changed:

- `types/src/value.rs`, `types/src/heap_types.rs` (A1)
- `vm/src/vm/vm_exec.rs`, `native-api/src/native_ring.rs`,
  `vm/src/runtime/memwatch.rs` (A3)
- `vm/src/runtime/interpreter.rs`,
  `vm/src/runtime/interpreter/exception_dispatch.rs` (A4a)
- `native-builtins/src/aot.rs`,
  `native-builtins/tests/lock_discipline_ratchet.rs` (A6)
- `vm/src/runtime/lockfree_resolve.rs`,
  `vm/tests/no_test_only_public_api.rs` (A7, A8)
- `.github/workflows/ci.yml` (A6, A7 gates)

---

## Summary

| | Finding | Status |
|---|---|---|
| A1 | JIT bakes a `repr(Rust)` enum layout; the comment justifying it is false | **LANDED** |
| A2 | Statics use 16-byte `Value` cells while instance fields are tagless | **NOT DONE** — §A2 |
| A3 | 13 diagnostic gates inline on the native-call funnel | **LANDED** |
| A4a | Two byte-identical exception-unwind copies in the dispatch loop | **LANDED** |
| A4b | Dual interpreter dispatch; per-bytecode safepoint poll | **NOT DONE** — §A4b |
| A5 | "`SharedVm` has 458 methods" | **WITHDRAWN — wrong** |
| A6 | Lock hierarchy has 0% adoption in `native-builtins` | **LANDED** (gate + first 2) |
| A7 | Dead resolution tiers kept alive by their own tests | **LANDED** |
| A8 | Second-tier invoke cache is one process-wide lock | **LANDED** |
| A9 | 45% of the tree is inline test code | Advisory — §A9 |

Three CI gates were added, all injection-tested rather than inspected:
`no_test_only_public_api` (A7), `lock_discipline_ratchet` (A6), and the
compile-time layout assertions in `value.rs` (A1).

---

# A1 — The JIT baked an unguaranteed enum layout — LANDED

## What was true

The JIT does not treat `Value` as an opaque Rust enum. It emits machine code
against the type's in-memory layout:

- `jit/src/ir_lower.rs:3896` — `MOV dword [rax + FIELD_CELL_TAG_OFFSET], 0`,
  the `Value::Int` discriminant, as a literal.
- `jit/src/x64/bytecode_walk.rs:8274` — the same for inline `putfield`.
- `jit/src/x64/objects.rs:761` — recognises an `Object` cell by the literal
  word `4`.
- `try_emit_inline_getstatic` loads out of a `StaticsBlock` at a baked address.

Four facts have to hold: tag is a `u32` at byte 0, 32-bit payload at byte 4,
64-bit payload at byte 8, discriminants 0..=6 in declaration order.

`Value` was `#[repr(Rust)]`. The only compile-time guards were `size_of == 16`
and `align_of <= 8` (`types/src/value.rs`, replicated at `jit/src/lib.rs:213`),
**none of which pins the tag's position, its width, or its values** — all three
of which rustc is free to change for a `repr(Rust)` enum. The only real pin was
a runtime unit test in `heap_types.rs`, which catches drift for whoever runs
`-p cratonvm-types` and nowhere else.

## The comment was wrong, and it was load-bearing

The header on `Value` said:

> Adding `repr(C)` would change size to 24 bytes and break JIT slot layout.

Measured on rustc 1.97.1 / x86-64 with an equivalent enum shape: `repr(Rust)`,
`repr(u32)` and `repr(C)` all produce **byte-identical** results —

```
repr(Rust) size=16 align=8  tags Int=0 Long=1 Float=2 Double=3 Object=4 RetAddr=5 Uninit=6
repr(u32)  size=16 align=8  tags Int=0 Long=1 Float=2 Double=3 Object=4 RetAddr=5 Uninit=6
repr(C)    size=16 align=8  tags Int=0 Long=1 Float=2 Double=3 Object=4 RetAddr=5 Uninit=6
                            p32@4=0x12345678  p64@8  Object(None)@8=0x0
```

`Object(None)` still zeroes the pointer word under every repr, because the
`Option<ObjectRef>` niche is internal to the payload type and not the enum's
own tag. The 24-byte claim had no basis.

## What landed

`Value` is now `#[repr(u32)]` with explicit `= 0 ..= 6` discriminants, and
`value.rs` `const`-asserts all four facts via `value_tag_word` /
`value_payload32` / `value_payload64`. Const-eval can read those words directly:
both are plain integer bytes with no provenance, and `Object(None)` is the
all-zero niche rather than a real address.

The asserts reference `heap_types::FIELD_CELL_*_OFFSET` rather than repeating
the numbers, so the JIT's constants and the layout cannot drift apart.

**Zero codegen impact** — the layout is byte-identical before and after. This
converts an unwritten assumption into a language guarantee.

## Both guards were injection-tested

- `Int(i32) = 9` → `error[E0080]: evaluation panicked: Value::Int tag must be 0`.
- Deleting `#[repr(u32)]` → `error[E0732]: #[repr(inttype)] must be specified
  for enums with explicit discriminants and non-unit variants`.

The second is the useful one: the explicit discriminants make the `repr`
**structurally impossible to drop silently**. A future edit cannot quietly
return this type to the state it was in.

`heap_types.rs` no longer claims `Value` has no `repr`; its runtime test is
retained as a second line of defence (it covers the non-null `Object` case,
which const-eval cannot express) and extended from two discriminants to all
seven — the JIT bakes `0` and `4` as *positions in that sequence*, so a reorder
leaving `Int` at 0 while moving `Object` would still miscompile.

---

# A2 — Statics still use 16-byte `Value` cells — NOT DONE

## The finding stands

Instance fields got the compact tagless treatment (`CompactLayout`, 1/2/4/8-byte
cells plus a precise `ref_offsets` GC map). Statics did not: `StaticsBlock` is
an array of 16-byte `Value` (`vm/src/vm/realms/class_realm.rs:43`).

That is 2× the cache footprint of the equivalent instance field, and every
JIT'd `getstatic` loads a tag word it structurally cannot need — the field's
declared type is known at compile time. The doc trail records static reads at
~35 ns against HotSpot's ~1, with the helper CALL since deleted; this is part
of what remains.

## Why it is not landed, and the specific blocker

It is a real project, not a patch: ~30 read/write sites in `vm`, two JIT
emitters (`x64/bytecode_walk.rs:3774` `try_emit_inline_getstatic` and
`ir_lower.rs:4326`), the GC's statics root scan (`vm/src/memory/roots.rs:1067`),
and the `putstatic` helper path.

The blocking design detail, which is worth writing down because it is not
obvious from the call sites:

> With `Value` cells the offset is `field_index * SLOT_SIZE` — a **uniform
> stride** the JIT computes from the index alone, with no knowledge of the
> class. Under a compact layout the offset is per-field and depends on the
> class's static layout, so the compile-time resolver must return
> `(base_cell_addr, byte_offset, width)` instead of just a base.

That is a straightforward extension of the existing `set_static_base_resolver`
seam — the resolver already runs at compile time inside the backend and already
declines classes it cannot serve — but it changes the emitted shape for every
inline `getstatic`, and a wrong width or offset is a **silent wrong value**, not
a crash. It needs its own change with its own differential run against HotSpot.

**A1 was its prerequisite and is now done**: the cell layout the emitters read
is a language guarantee rather than an observation, so the compact work starts
from a pinned baseline.

---

# A3 — 13 diagnostic gates on the native-call funnel — LANDED

## What was true

`safe_native_call_impl` (`vm/src/vm/vm_exec.rs`) is the choke point for
essentially every native dispatch in the VM. Prior measurement puts the
per-call floor at ~8.4 ns against HotSpot's ~1. It was **671 lines** carrying
thirteen independent diagnostic subsystems interleaved with the real work: the
pin-underflow guard, dispatch tracing, VM-state publication, the native ring,
the straystack thread-local, EC watch, memwatch, the young scan, the linkage
dump, stale-objref naming, remap tracing, altrace, and the unpin ring.

Every gate was individually cheap — a memoized `OnceLock` read, a lesson already
learned here when uncached `getenv` probes measured ~7% of a HashMapOnly-4M run.
Thirteen of them are not: thirteen loads, thirteen branches to predict, and a
671-line body whose cold half sits in the middle of the hot one.

## What landed

All thirteen collapse into one `u32` (`native_diag`). The common case — every
diagnostic off, which is every production run — tests one value once before the
callback and once after, and never reaches the cold code. The two cold halves
are `#[cold] #[inline(never)]` (`native_diag_pre_call` / `native_diag_post_call`).

**The split is by mutability and it is not cosmetic.** Eleven gates are
startup-static memoized env reads and are computed once. Two are not:
`dispatch_trace::enable()` is called at runtime by the stack-dump watchdog so a
hang in native code still leaves a breadcrumb, and `native_ring::enable()` is
likewise toggleable. Memoizing either would silently disable a diagnostic at
exactly the moment someone switched it on to chase a hang, with nothing in the
funnel's behaviour to reveal it. Those two are re-read every call — which is
what they already cost.

Two accessors were added rather than approximated:

- `native_ring::any_recording_enabled()` — `record_enter` has **two**
  independent gates (the ring, and `CRATONVM_TRACK_NATIVE` via the private
  `track_enabled`). Folding in only `is_enabled()` would have silently disabled
  native tracking.
- `memwatch::is_watching()` — `addr()` is memoized from `CRATONVM_DBG_MEMWATCH`
  and never changes, so this gate is static even though `ARMED` is not.

## How it was verified, and why not end-to-end

These diagnostics are **all fire-on-anomaly** — they print nothing on a healthy
run. An armed-flag end-to-end run therefore cannot confirm the wiring, and this
is worth recording because the obvious verification does not work here.

(The first attempt also hit the known trap: `CRATONVM_DBG_LETSGO=1` is rejected
at startup with *"1 per-flag variable(s) set directly; the supported spelling is
now: `CRATONVM_DBG=letsgo`"*. That run proved nothing, and would have read as a
clean pass.)

Both halves of the transcription risk are covered by tests instead:

- **mask composition** — `every_diagnostic_bit_is_distinct` catches a duplicated
  shift, which would alias two diagnostics so arming one armed the other;
  `the_memoized_half_never_holds_a_runtime_toggleable_bit` and
  `the_ring_bit_tracks_runtime_toggling` pin the static/dynamic split in both
  directions.
- **bit consumption** — `pre_call_acts_only_on_the_bits_it_is_given` drives
  `native_diag_pre_call` with synthetic masks and asserts each bit produces its
  own effect and no other.
- `a_default_process_arms_nothing` asserts the mask is zero with nothing
  configured, so a future default-on gate cannot quietly put every native call
  in the VM on the cold path.

The three ring-toggling tests share a mutex. libtest runs them in parallel and
`native_ring`'s `ENABLED` is process-global, so without it one test's
`enable(false)` lands between another's `enable(true)` and its assertion. That
failure looked exactly like the bug the assertion exists to catch — the kind
that gets "fixed" by weakening the assertion.

## Not measured

No A/B throughput number is claimed. Doing it properly needs interleaved arms in
both orders against a HotSpot control on a quiet box, per this project's own
benchmarking rules, and that was not run. The structural claim — 13 gates and
13 branches become 1 — is verifiable by reading the diff; the nanoseconds are
not claimed.

---

# A4a — Two identical exception-unwind copies — LANDED

The dispatch loop carried **two byte-identical copies** of the handler walk —
one draining `pending_java_exception`, one draining `pending_runtime_error`
after `throw_runtime_error` produced a throwable. Both sat in the loop
*prologue*, ahead of the opcode fetch, so their ~40 lines each were in the hot
loop's instruction footprint on every bytecode despite running only when an
exception is in flight.

Duplication was the more expensive problem. The GC pin they share is subtle and
load-bearing: the walk can pop many frames, and `find_exception_handler` →
`find_exception_handler_impl` lazily loads an unresolved catch-type class on a
cache miss (`load_class_concurrent`, which runs `<clinit>` and can allocate,
hence collect). Once a frame is popped it no longer roots the propagating
exception. So the throwable is pinned in `native_pin_roots` for the whole walk
**and re-read from there each iteration**, because a moving collector rewrites
the pin slot in place and a local copy taken before a GC is stale.

A fix applied to one copy and not the other is a use-after-free reproducing on
only one of the two throw paths. The two had already drifted in comments.

Both now call `exception_dispatch::unwind_to_handler`, which owns the walk, the
pin, and the reasoning.

Verified: `exception_tests` 11 passed, `exception_edge_tests` 19 passed,
`interpreter_tests` 924 passed.

---

# A4b — The dual interpreter dispatch — NOT DONE

## The finding stands

`vm/src/runtime/interpreter.rs` has a raw-byte fast path and a decoded fallback
path, and `--noverify` switches which one runs
(`let use_fast_path = !shared.config.skip_verification`). Two implementations of
overlapping opcode sets means every opcode fix must land twice, and a divergence
is a bug that appears under one flag only. The project's own architecture doc
records a prior instance of this shape causing both a 2.7× throughput cliff and
different semantics.

The per-bytecode prologue also still contains an `Acquire` load of
`stw_requested` on every instruction, and re-derives `code_ptr` / `code_len` /
the frame pointer each iteration.

## Why it is not landed

This is the largest item in the review and the one with the most upside, but it
is a rewrite, not a refactor: a token-threaded dispatch over pre-decoded
instruction words built on the existing `QuickenedCode`, retiring the raw/decoded
split and moving the safepoint poll to a dispatch-table swap.

Two notes for whoever takes it, both learned while scoping A4a:

1. **The loop-invariant hoist is not separable.** `code_ptr` / `code_len` look
   like an easy independent win, but hoisting them correctly requires
   invalidation on every frame push/pop, and frame *address* equality does not
   imply same method — the frame arena recycles slots. The token-threaded
   rewrite gets the invalidation for free because the instruction stream is
   addressed per-method rather than per-frame. Attempting the hoist standalone
   buys two loads from an already-hot cache line at the cost of a correctness
   hazard. It was deliberately dropped from A4a for this reason.
2. **The safepoint poll's ordering is load-bearing.** The `Acquire` is what
   orders the reads that follow it. Moving the poll is safe; relaxing it in
   place is not.

---

# A5 — WITHDRAWN: the "458 methods" claim was wrong

The review asserted:

> **God object:** `SharedVm` with 458 methods in one impl block …
> 458 `fn` in the single `impl SharedVm` block in `vm_exec.rs` (25K lines).

**This is false, and the conclusion drawn from it does not survive.** Corrected
by measurement:

- `impl SharedVm` has **53 methods**, across 12 blocks — 11 in `vm/src/vm/vm_init.rs`
  and 1 in `vm/src/debug/mod.rs`.
- **`vm_exec.rs` contains no `impl SharedVm` block at all.**
- The 458 I counted were indented `fn` lines in `vm_exec.rs`. They are
  **449 methods of `impl NativeContextImpl<'_>`** (`vm_exec.rs:3437`) — the
  `NativeContext` trait surface, which is legitimately one interface — plus
  **132 functions inside `#[cfg(test)]`**, which I had not excluded, and 130
  top-level free functions.

The error was counting `^    fn ` across a file and attributing it to the
nearest type I had in mind, without checking which `impl` the lines belonged to.

The real coupling metric is different and much less alarming: **401 free
functions in `vm/src` take `&SharedVm`**, and that coupling is confined to the
`vm` crate — `jit` has 1, `gc` 0, `classloading` 0, `native-builtins` 2. For a
runtime crate whose whole job is operating on the VM, passing the VM is
idiomatic, and the realm split (`classes` / `natives` / `mem` / `threads` /
`debug` / `jit`) already gives the *state* the structure the finding asked for.

**No change was made, and none is warranted.** Distributing 53 methods across
six realms would be churn in a tree with 80+ live worktrees where, by this
project's own notes, "splitting a file breaks every gate that scans its text"
and "merging dev into a split needs hunk replay". The realm refactor did this
work already.

---

# A6 — The lock hierarchy has no adoption where it matters — LANDED (gate + first two)

## What was true

The workspace has a designed hierarchy: `LockLevel` L0..L10 in
`types/src/lock_order.rs`, with `OrderedPlMutex` / `OrderedPlRwLock` wrappers
asserting strictly-decreasing acquisition order. Adoption:

| Crate | Ordered | Raw `Mutex::new` / `RwLock::new` |
|-------|--------:|---------------------------------:|
| `vm` | 116 | 319 |
| `native-builtins` | **0** | **440** |

(A plain `grep` reports 481 for `native-builtins`; 440 is the figure excluding
`#[cfg(test)]` regions and comment lines, and is the production number.)

`native-builtins` is the largest crate in the workspace **and** the one that
re-enters the VM: a native callback calls back into Java, which takes heap locks
and the L10 class-manager lock. That is precisely the shape a lock hierarchy
exists to police, and it had no coverage at all.

Note also that `tracking::check_and_acquire` is `cfg!(debug_assertions)`-or-env
gated (`types/src/lock_order.rs:260`), so release builds do not check by
default. The hierarchy is a convention in shipping builds and an assertion in
debug/CI ones.

## Why this ratchets instead of converting

Converting all 440 mechanically would be **worse** engineering than the gate.
`OrderedPlMutex::new` takes a `LockLevel`, and a level is a *claim*: "no lock at
or below this level is ever held when this one is taken". Stamping 440 locks
with a level nobody reasoned about makes the checker assert something
unverified — it would either fire constantly on correct code or, worse, pass
while encoding a false hierarchy. **A wrong level is more dangerous than no
level, because it reads as audited.**

## What landed

`native-builtins/tests/lock_discipline_ratchet.rs`, wired into CI beside
`stub_ratchet`. It counts raw lock constructions outside `#[cfg(test)]` and
fails if the number rises; a conversion must lower the baseline in the same
change. It also reports the *ordered* count, so "is this crate under discipline
yet?" is answerable from CI output rather than from a grep.

Plus the first two conversions as the pattern: `AOT_CACHE_INPUT_PATH` and
`AOT_CACHE_OUTPUT_PATH` (`native-builtins/src/aot.rs`) at `LockLevel::Scratch`.
They were chosen because the claim is *checkable by reading six call sites* —
`init_aot_runtime`, `reset_aot_globals`, the training-flush path, and the two
`leyden_get_aot_cache_*_path` natives all acquire, clone the `Option<String>`,
and drop the guard **before** touching `ctx`. The natives call
`ctx.create_string` only after the clone, so the guard is never held across a
re-entry into the VM. `Scratch` is correct for a reason, not by default.

This also proves the path works from this crate at all — `OrderedPlMutex::new`
is `const fn`, so the statics convert in place — so the backlog is not blocked
on an unknown.

Baseline **440 → 438**, ordered **0 → 5**.

Injection-tested: adding one raw `static Mutex` to `native-builtins/src/lib.rs`
moves the count to 441 and fails the gate; `grep -c` confirmed the injection
landed, and confirmed its removal.

## Remaining backlog

438 locks. The unit of work is per-lock: decide whether it can be held across a
re-entry into the VM, and pick the level from the answer. Most are leaf caches
belonging at `Scratch`; **the interesting minority are those held across a
`NativeContext` callback, and those are the actual latent deadlocks this program
should surface.** The heaviest files are `t27_tls.rs` (33), `lib.rs` (27),
`xnio_io_thread.rs` (16), `net_phase_e.rs` (16).

---

# A7 — Dead resolution tiers kept alive by their own tests — LANDED

## What was true

`vm/src/runtime/lockfree_resolve.rs` was 1,634 lines describing a three-level
resolution cache. **One tier was wired.** The rest —
`ThreadLocalResolveCache`, `SharedResolutionState::resolve_method` /
`cache_method` / `resolve_field` / `cache_field`, their `global_methods` /
`global_fields` maps, the `ResolutionKey` / `ResolvedTarget` / `ResolvedField` /
`CacheStats` types, and an `invalidate_all` that took three write locks to clear
two permanently-empty maps — had **no production callers whatsoever**.

389 lines, including a security hardening pass on `ResolutionKey` (full
interned-string identity validation to defeat `FxHasher` collision forgery, plus
loader-epoch isolation) applied to a cache nothing consulted.

A 2026-07-26 audit found this and documented it accurately in the module header.
It was still there on 2026-08-04. **Prose in a header does not delete code.**

It survived `dead_code` because its own unit tests referenced it. That is the
general failure mode, so the general case is now enforced rather than remembered.

## What landed

The dead region and its tests are gone. Two tests were retargeted rather than
dropped, because their subject matter survived even though their premise did not:

- `t10_invalidate_all_clears_promoted_cache` → `t10_invalidate_promoted_clears_promoted_cache`
- `invalidate_promoted_matches_invalidate_all_for_the_live_cache` →
  `wholesale_clear_leaves_no_memoized_negative` (its second half covers what
  `promoted_invoke_round_trip_is_the_only_live_path` does not: a key cleared by
  a *wholesale* sweep re-misses instead of sticking as a negative)

New gate: `vm/tests/no_test_only_public_api.rs`, wired into CI. For every `pub`
item declared in `vm/src` production code it censuses references across all 22
workspace members and flags those with no production reference beyond their own
declaration but at least one test reference. Ratcheted at **322** with zero slack.

## The scanner needed two corrections, both found against a known answer

Neither was found by reading the scanner. Both were found by checking it against
the A7 items and asking why it did not flag them.

1. **A doc comment is not a use.** The first version counted every line, so
   `lockfree_resolve.rs`'s own header — which *named* all the dead types while
   explaining that they were dead — pushed each to `prod >= 2`. The audit's
   honesty was hiding the corpse.
2. **`impl Foo` is not a use of `Foo`.** It is part of `Foo`'s definition.
   Counting it meant `ThreadLocalResolveCache` (three `impl` blocks, zero call
   sites outside tests) read as live.

Injection-tested: restoring the deleted file takes the count 322 → 327 and fails
the gate. Diffing the two offender lists shows the five-item difference is
exactly `ThreadLocalResolveCache`, `cache_method`, `resolve_method`,
`resolve_field`, `invalidate_all`.

**Known limitation, deliberately lenient.** Matching is by bare identifier, so
an unrelated same-named item in another crate masks an offender. The other six
A7 items (`ResolutionKey`, `ResolvedTarget`, `ResolvedField`, `cache_field`,
`method_count`, `field_count`) are masked by live, unrelated declarations in
`cratonvm-classloading`. The gate caught five of eleven. That error direction is
chosen: it must never fail on code that is actually used.

`MIN_DECLARATIONS_SCANNED = 2000` guards the vacuous pass — a future edit that
breaks the declaration parser or mis-roots the walk fails loudly instead of
reporting zero offenders and passing.

**Backlog: 322 offenders.** Each is either dead code to delete or an item whose
sole real caller is a test, which should usually be `#[cfg(test)]` itself.

---

# A8 — The second-tier invoke cache was one process-wide lock — LANDED

## What was true

`get_promoted_invoke` read-locked **one** process-wide `RwLock<FxHashMap>`. A
`parking_lot` read acquire is an atomic read-modify-write on the lock word, so
every dispatching thread wrote the same cache line on every consult — plus a
second shared line for `promoted_hits`, incremented on every hit.

This is invisible on a single-threaded benchmark and a scaling ceiling on a
loaded one. Because the tier is only reached on a `JvmThread::invoke_cache`
miss, it does not present as lock *contention* in a profile; it presents as
cache-line traffic.

## What landed

16 independently-locked shards. Two details carry the win:

- **`repr(align(64))` per shard.** Sharding a map without separating the shards
  leaves every lock word on the same one or two lines, so the cores ping-pong
  exactly as before and the split buys nothing measurable. The padding is the
  point. `shards_do_not_share_a_cache_line` also checks the size is a whole
  number of lines, so shard N+1 cannot start inside shard N's line.
- **The hit/insert counters moved *inside* the shard.** Leaving two
  process-global `AtomicU64`s incremented on every hit would have relocated the
  contention rather than removed it — the exact mistake this module's own header
  calls out about the pre-A8 design.

Shard selection hashes the **whole key**, not just `caller_class`. Sharding on
the caller alone would put every call site of one hot class — precisely the
class being consulted in a tight loop — on a single shard, reproducing the
original contention under a new name. `one_caller_class_spreads_across_shards`
pins this: 256 call sites of one caller must reach all 16 shards, which a single
map (the degenerate one-bucket case) fails.

## The security bound survives

`shared_cache_cap` exists so a key-minting workload (a class emitting fresh
lambda / proxy / hidden-class names per call) cannot grow the cache until the
process dies. Splitting one capped map into 16 uncapped ones would have quietly
deleted that property. `per_shard_cap` divides it with `div_ceil`, so the
aggregate ceiling is at most `cap + 15`, and the bound is now per-shard — i.e.
*tighter*, never looser.

The ceiling direction and the `max(1)` are load-bearing: rounding down would
turn the documented minimum cap of 1 — which `parse_shared_cache_cap` enforces
so a fresh insert always survives — into a per-shard cap of 0, and
`evict_to_fit` with `cap == 0` evicts the entry it was called to make room for,
turning a tuning knob into a silent total disable.
`a_cap_of_one_still_admits_an_entry` covers exactly that.

`shard_of_is_stable_for_a_key` guards the failure with no symptom: `get` and
`insert` compute the shard independently, so a non-pure shard function would
send every promotion and its lookup to different shards — a pure slowdown,
silently.

## Not measured

No multi-core A/B is claimed. The right measurement is a loaded Spring/Tomcat
run against a HotSpot control with interleaved arms, and it was not run. This
project already has an open "SmokeTests concurrency throughput ceiling" and an
"every AQS handoff is 13-26× HotSpot" note; whether this contributes is
**untested**.

---

# A9 — Test code placement — advisory, no change

Roughly 600K of the tree's 1.32M lines are `#[cfg(test)]` modules inside
production files: `vm` 197K/320K, `native-builtins` 232K/565K, `classloading`
39K/61K, `jit` 85K/183K. That is what produces a 76K-line `vm/src/vm.rs` and a
41K-line `native-builtins/src/lib.rs`.

The review's original concern that these tests do not run was **checked and is
wrong** — `.github/workflows/ci.yml:297` runs
`cargo test -p cratonvm-vm --lib --features synthetic-jdk`. That gap is closed.

The residual point is coupling, not coverage: inline test modules reach private
items, which couples tests to internals and makes refactors like A4b more
expensive. No bulk move is proposed — moving 600K lines would collide with every
live worktree for a benefit that is real but diffuse. The suggestion is
opportunistic: when touching a file, move the tests that only use the public
surface out to `tests/`, and keep inline only what genuinely needs private
access.

---

# Verification summary

| Check | Result |
|---|---|
| `cargo build --workspace` | clean |
| `cargo test -p cratonvm-types --lib` | 488 passed |
| `cargo test -p cratonvm-vm --lib` | 2376 passed, **1 pre-existing failure** |
| `cargo test -p cratonvm-vm --test exception_tests` | 11 passed |
| `cargo test -p cratonvm-vm --test exception_edge_tests` | 19 passed |
| `cargo test -p cratonvm-vm --test interpreter_tests` | 924 passed |
| `cargo test -p cratonvm-vm --test no_test_only_public_api` | 1 passed (322) |
| `cargo test -p cratonvm-native-builtins --test lock_discipline_ratchet` | 1 passed (438) |
| Release `HelloWorld`, `DistinctEquals` (5/5), `MapEqRepro` (15/15), `StringNativeAllocationChurn`, `LicmHoistBench` | all pass |

The one failure — `native_override::redefine_immunity_tests::layout_immunity_is_not_open_coded`
— **was baselined against an unmodified `dev` worktree and fails there
identically.** It is not caused by this branch.

No performance A/B was run for A3 or A8. Both are argued structurally and the
nanoseconds are explicitly not claimed; §A3 and §A8 say what would have to be
measured and under what conditions.
