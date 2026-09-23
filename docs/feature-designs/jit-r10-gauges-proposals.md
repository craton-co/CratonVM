# JIT round 10, lane `gauges` — proposals

Written 2026-09-21 after wiring the code-cache allocation-failure ledger
(`jit/src/lib.rs`, `jit/src/platform.rs`, `vm/src/jit/code_cache_lifecycle.rs`)
and reviewing those three files. Everything below was established by **reading**
the sources; this lane may not build, test or probe, so no figure here is
measured unless it is quoted from a comment that claims to have measured it.

Ordered by expected value per unit of risk.

---

## 1. A gauge-liveness gate for `cratonvm-jit`, modelled on the VM's

**Problem.** Four lanes in round 10 independently found the same defect class:
a counter or public entry point with no production consumer, reading zero
forever and indistinguishable from "it never happened". This lane found four
more instances in two files:

| instance | shape |
| --- | --- |
| `failed_allocations` / `_bytes` / `_by_reason` | entry point existed, no crate could call it (FIXED this round) |
| `POISONED_JIT_BYTES` | written on every poisoned free, read by nothing (FIXED this round) |
| `UNPINNED_JIT_ENTRIES`, `UNROOTED_DIRECT_CALLEES`, `REBOUND_DIRECT_CALLEES`, `RETIRED_DIRECT_CALLEES`, `IC_RETIRED_INSTALL_ROLLBACKS` | written by production paths, no accessor at all (FIXED this round) |
| `ATOMIC_INTRINSIC_SITES`, `ATOMIC_LONG_INTRINSIC_SITES`, `OBJECT_HASHCODE_STRING_GUARD_SITES` | written by the compile paths, no accessor at all, while eleven siblings are printed by `tiered.rs` (FILED) |
| `log_jit_code_arena_census` | a `pub fn` emitter with zero callers, asserted by a comment in the same file to fire (FILED) |

`vm/tests/no_test_only_public_api.rs` already ratchets one half of this for
`vm/src`: `pub` items whose only references are from tests. It does not cover
`jit/src` at all, and it cannot see the shape that actually dominates here —
an atomic that is **written but never read**, which has plenty of production
references and so reads as live.

**Proposal.** A `jit/tests/gauge_liveness_ratchet.rs` in the style of the
existing ratchets, scanning `jit/src/**` for:

* `static` items whose declared type mentions `Atomic*` or `StripedCounter`,
  and classifying every workspace reference as a read (`load`, `sum`,
  `compare_exchange`) or a write (`fetch_*`, `store`, `swap`). A static with
  writes and zero reads is an offender.
* `pub fn` items whose name matches the census/stats/log shape
  (`*_stats`, `*_census`, `*_count`, `*_counts`, `log_*`) and that have no
  reference outside their own declaration and `#[cfg(test)]`.

Frozen count, `assert_eq!` ratchet, same "do NOT raise the baseline" message.

**Cost and risks.** Three traps, all of which were walked into while preparing
this document, and all of which a first version of the gate will hit:

1. **Multi-line accesses.** `X.fetch_add(\n 1,\n Relaxed)` is common in this
   codebase and a line-at-a-time classifier misreads it; a naive version of the
   scan produced ~30 false "read-only" results for exactly that reason. A
   three-line context window was enough to clear them, but the right answer is
   to join the statement before classifying.
2. **Comment lines and `impl` headers.** Both are documented in
   `no_test_only_public_api.rs` as traps that cost a real miss — its own module
   header names the dead types while explaining that they were dead, which
   pushed each of them above the threshold. Reuse that file's `split_regions`
   rather than rewriting it.
3. **Bare-identifier matching is too lenient to be the whole gate.** A
   reference scan over all 550 `pub` items in `jit/src/lib.rs` and
   `jit/src/platform.rs`, matching by bare identifier, reported **zero** items
   with no production reference — while a targeted read/write classification of
   the same files' atomics found eight genuine write-only counters, five of
   which this round fixed. The reason is the limitation
   `no_test_only_public_api.rs` already documents for itself: short, generic
   names (`new`, `stats`, `census`, `alloc`) collide with unrelated
   declarations in other crates and mask the item being checked. A "0
   offenders" result from that shape of scan is evidence the scan is lenient,
   **not** evidence the crate is clean, and a gate built on it would pass
   vacuously — the exact failure `MIN_DECLARATIONS_SCANNED` exists to prevent
   one level down. Resolve references by path (`crate::X`, `platform::X`,
   `self::X`) or scope the match to the declaring crate; and pin the gate
   against a known answer — the eight counters named in this document's table —
   before trusting it, because a scanner nobody has watched fail is a scanner
   that passes vacuously.

**Why it is worth it.** The class has now cost five separate lanes an
investigation each in one round, and every instance was a diagnostic whose
whole purpose was to answer a question that it silently answered wrong.

**Interaction with the existing statics ratchet.**
`jit/tests/process_global_statics_ratchet.rs` already freezes the NUMBER of
`static` declarations under `jit/src`, on the (correct) grounds that a static
in `cratonvm-jit` is shared by every VM in the process. That gate pushes new
counters onto existing counter structs rather than into new statics — this
lane's own ledger lives on `RECLAMATION`'s fields for exactly that reason. A
liveness gate must therefore work on FIELDS of counter structs, not only on
top-level statics, or it will miss everything the other gate forces into a
struct. That is the single most important design constraint on proposal 1 and
the easiest to get wrong.

---

## 2. Give the code-cache cap gate a byte figure

**Problem.** `note_jit_code_cache_cap_refusal` now records a
`CAP_EXCEEDED` allocation failure with `requested_bytes = 0`, because the cap
gate in `compile_gate::admit` refuses before codegen computes a buffer size.
`failed_allocation_bytes` is therefore a strict lower bound, and on a
cap-pressured run it is near-zero while `failed_allocations` climbs — a report
that says "many failures, no bytes" and makes the mean meaningless.

**Proposal.** `admit` already holds the `CachedBytecodeMethod`. The sizing
heuristic (`code_buffer_hint` / the single-pass estimate) is a pure function of
the bytecode length and is cheap. Pass the estimate into the refusal so the
gauge carries "the bytes this compile would have asked for". Two shapes, both
small:

* widen `note_jit_code_cache_cap_refusal()` to take `estimated_bytes: usize`
  and have `compile_gate::admit` compute it from `cached.code.len()`; or
* keep the counter's signature and record the failure from `admit` itself,
  which is the site that knows both the cap verdict and the method.

The first is one extra argument at one call site. `jit/src/compile_gate.rs` was
another lane's file this round, which is why it was not done here.

**Watch out for.** The estimate is a guess, not a request an allocator saw. Say
so in `JitCodeAllocFailureStats::failed_bytes`' doc, or the number acquires a
precision it does not have — the same mistake as labelling an OOM
`NO_EXTENT_LARGE_ENOUGH`.

---

## 3. Bound the arena's reservation at the code-cache cap

**Problem.** `JitCodeArena::alloc` never refuses for want of space: when its
free lists and every region's bump room miss, it maps a new region, and failing
that falls back to a standalone mapping. So

* the code-cache cap (`COMMITTED_JIT_CODE_BYTES` vs
  `jit_code_cache_cap_bytes()`) is enforced only at the *compile gate*, one
  level above the allocator, and only for whole compiles — a single oversized
  body still maps whatever it needs; and
* `alloc_failure::NO_EXTENT_LARGE_ENOUGH` is structurally unreachable, which is
  filed as `r10-gauges-no-extent-large-enough-is-unreachable-20260921.md` — since retired to the internal
  tree as `r10-gauges-no-extent-large-enough-is-unreachable-20260921-RESOLVED-20260922.md`, DECIDED: keep the
  reserved code, do not make it reachable.

**Proposal.** Teach `map_region` to refuse once
`COMMITTED_JIT_CODE_BYTES + region_bytes` would exceed the cap, and let `alloc`
distinguish its two refusals: "the OS said no" (`OS_REFUSED`) from "the arena
may not reserve more and no extent fits" (`NO_EXTENT_LARGE_ENOUGH`). The
external-fragmentation arithmetic in `code_cache_lifecycle.rs` §3 then predicts
a refusal that can actually happen, which is what it was built for.

**Watch out for.** This makes the arena able to fail where it previously always
succeeded, and the eviction hook (`ask_for_eviction`) is unset in this
workspace, so the first effect is more declined compiles rather than more
eviction. Land it behind the existing `CRATONVM_JIT_CODE_ARENA` flag and
measure with REVIEW-NOTE 3's list before considering a default.

---

## 4. Emit one end-of-run JIT census, and route every summary through it

**Problem.** The JIT's diagnostic surface is a scatter of accessors with
different consumers and different channels: `jit_code_reclamation_stats`
(pulled by the VM report), `jit_code_arena_census` (pulled for three fields
only), `log_jit_code_arena_census` (no caller at all — filed as
`docs/internal/fixed-bugs/r10-gauges-arena-census-logger-has-no-caller-FIXED-20260922.md`),
`code_ptr_memo_stats`, `buffer_session_census`, `scoped_memory_census`,
`spill_cursor_counts`, `stack_walk_dedupe_counts`, `compiled_frame_line_counts`
and roughly two dozen more, several of which are `pub` accessors whose only
caller is a test. Each new gauge has to invent its own way out, which is how
this round's four instances happened.

**Proposal.** One `pub fn jit_process_census() -> JitProcessCensus` in
`jit/src/lib.rs` that aggregates the existing accessors, and one call at VM
shutdown that emits it (plus `log_jit_code_arena_census` when the arena is on).
New counters then have an obvious destination and the liveness ratchet in
proposal 1 gets a single place to check against.

**Watch out for.** Do not make the census a `Display` impl that a caller might
format without emitting. The bug being designed out is "the number exists but
nothing consumes it"; a formatter with no emitter is the same bug wearing a
different hat.

---

## 5. Make `ExecutableBuffer` carry its arena back-pointer

**Problem.** `free_executable` dispatches on the ADDRESS, via a process-wide
span table (`ARENA_SPANS` / `ARENA_SPAN_COUNT`), because when the arena landed
`ExecutableBuffer` and its `Drop` lived in `jit/src/lib.rs`, which that change
was not allowed to edit. The file's own `REVIEW-NOTE` says the field is the
intended shape. `ExecutableBuffer` has since moved to `jit/src/exec_memory.rs`,
so the blocker is gone.

**Proposal.** Add the `Option<JitArenaHandle>` field, set it in
`ExecutableBuffer::new_in`, and have `Drop` consult it. Keep the span table:
`free_executable` is `pub` and is called with raw pointers from elsewhere, and
the doc is explicit that reaching `platform_free` with an interior address
would `munmap` through the middle of a live region on Unix. The field makes the
common path a field read instead of a locked table lookup; the table stays as
the fallback for raw callers.

**Watch out for.** `free_executable`'s cost argument in the default
configuration ("one relaxed load of a counter that is zero") must survive —
the win is on the arena-on path only, and a change that makes the default path
slower to speed up a default-off mode is a regression.

---

## 6. Retire the second cap-refusal counter, or state why there are two

**Problem.** A code-cache cap refusal is now counted twice:
`JIT_CODE_CACHE_CAP_REFUSALS` (read by `jit_code_cache_cap_refusals()`) and the
`CAP_EXCEEDED` bucket of the new ledger. They are not equal — the ledger's
bucket excludes refusals attributed to a latched executable-memory policy
denial, which `jit_code_cache_at_capacity` also answers `true` for and which
the older counter silently folds in.

**Proposal.** Either fold the older counter into the ledger's bucket and make
`jit_code_cache_cap_refusals()` read `jit_code_alloc_failure_stats().by_reason[CAP_EXCEEDED]`,
or keep both and document the divergence at both declarations. Today the
comment at `note_jit_code_cache_cap_refusal` explains it; the older accessor's
doc does not, and an operator comparing the two numbers will find them
disagreeing with no explanation at the place they are reading.

**Cheapest version.** Make the older accessor a view over the ledger. One line,
one counter deleted, and the two figures cannot drift again.
