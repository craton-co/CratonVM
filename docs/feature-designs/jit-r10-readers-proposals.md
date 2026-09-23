# JIT round 10, lane `readers` — proposals

Written 2026-09-21 after closing four round-10 pages on the same defect class
(an instrument written by production and read by nothing) and reviewing
`jit/src/lib.rs`, `jit/src/platform.rs` and `vm-cli/src/main.rs`'s diagnostic
dump. Everything here was established by **reading**; this lane may not build,
test or probe, so no figure below is measured unless it is quoted from a comment
that claims to have measured it.

Ordered by expected value per unit of risk.

---

## 1. Make the orphan gate see STATICS, not just functions — it currently cannot see the defect it was built for

**Problem.** `scripts/check-orphan-instruments.sh` censuses `pub fn record_*` /
`note_*` (C1), zero-argument `pub fn` returning integers (C2), and `*_EVENTS`
rows (C3). It does **not** census `pub static ...Atomic...` items at all.

That is a real hole, and this lane walked straight into it. The three counters of
`r10-gauges-three-intrinsic-site-counters-have-no-reader-20260921.md` —
`ATOMIC_INTRINSIC_SITES`, `ATOMIC_LONG_INTRINSIC_SITES`,
`OBJECT_HASHCODE_STRING_GUARD_SITES` — are the purest instances of the round's
defect class the round has found: written by production, **no accessor at all**,
so not merely unread but *unreachable*. None of the three was in the frozen
allowlist, and none could have been, because the gate does not look at statics.

The gate's own header calls the accessor-with-no-caller shape "the same defect one
level up". It is right, and it catches that one. The level *below* — a counter
with no accessor — is invisible to it, and is strictly worse, because a future
consumer cannot even read the number without first editing `jit/src/lib.rs`.

**Proposal.** A fourth check, C4: a `pub static` whose declared type mentions
`Atomic`, with at least one `fetch_*` / `store` / `swap` reference anywhere, and
**zero** `load` / `compare_exchange` references anywhere in the workspace. That
is the written-but-never-read shape, and it is decidable with the same machinery
C1/C2 already use.

**Cost and the three traps, all of which this lane hit while doing it by hand.**

1. **Multi-line accesses.** `X\n    .fetch_add(1, Relaxed)` is the dominant
   formatting in `jit/src/lib.rs` — `ATOMIC_INTRINSIC_SITES` and
   `OBJECT_HASHCODE_STRING_GUARD_SITES` are both written that way, with the
   static on one line and the method on the next. A line-at-a-time classifier
   calls those unwritten, i.e. exactly backwards. Join the statement first, or
   use a context window and attribute the op to the nearest preceding identifier.
2. **Addresses taken by emitted code.** `SHADOW_OVERFLOW_COUNT` and
   `SHADOW_OVERFLOW_LABEL` have no `.load(` anywhere and are not orphans: JIT
   machine code writes them through `(&crate::SHADOW_OVERFLOW_COUNT as *const _)
   as usize`, from `jit/src/x64/safepoint.rs` and `jit/src/ir_lower.rs`. Any
   `as *const` / `as *mut` reference to a static must count as both a read and a
   write, or C4 will report the two counters the VM's crash handler depends on.
3. **The counter written in one crate and read in another.**
   `FINAL_DEVIRT_NATIVE_SHADOW_REFUSED` is declared in `cratonvm-jit`, written by
   `cratonvm-vm`, and read by an accessor in `cratonvm-jit`. The search has to be
   workspace-wide in both directions; a per-crate rule gets it wrong twice.

**Expected yield, stated so it can be checked.** Applied by hand to
`jit/src/lib.rs` alone, this rule found five written-never-read counters beyond
the three already filed — the `VARHANDLE_WRITE_SITES_*` pair, the
`LONG_LONG_VALUE_INTRINSIC_SITES` / `INTEGER_INT_VALUE_INTRINSIC_SITES` pair, and
`FINAL_DEVIRT_NATIVE_SHADOW_REFUSED` — all five fixed in this lane's commit. One
file, one rule, five instances. `jit/src/` has 60-odd such statics and the
workspace has many more files.

**Risk.** The allowlist grows before it shrinks. That is the point of a ratchet,
but the gate's own §L7 warning applies with force: a gate that is red on arrival
gets disabled. Land C4 behind `--check-statics` for one cycle, freeze, then make
it unconditional.

---

## 2. The inverse check: an instrument that is READ but never WRITTEN

**Problem.** C4 above catches write-with-no-read. The mirror image is worse,
because the zero is on a screen. This lane found one:
`LONG_LONG_VALUE_DIRECT_SITES` was read and printed by
`vm::runtime::interp_census` as `[direct-binds] Long.longValue: sites_bound=`,
and its only `fetch_add` in the workspace sat inside
`note_long_long_value_direct_site()`, which had no caller — while the
`served`/`declined` figures on the *same printed line* came from a counter that
*is* fed. So the line read `sites_bound=0 served=N` with N non-zero: calls served
by a bind the same line says was never made, directly beneath a correct
`Integer.intValue` line. A reader concludes the `longValue` recognition is
broken. (Filed and half-fixed:
`r10-readers-orphaned-jit-accessors-FIXED-20260922.md` §5.)

**Why this is the more valuable half.** A write-with-no-read is silent — nobody
is misled, the number just is not there. A read-with-no-write is *actively
misleading*, and it survives review because the printing code is obviously
correct and the missing `fetch_add` is somewhere else entirely.

**Proposal.** C5: a `pub static ...Atomic...` with at least one `load` reference
and **zero** write references. Same machinery, same traps, inverted predicate.
Cheap once C4 exists, since the reference classification is shared.

**Sharper variant, and the one that would have caught this specific bug
earlier:** flag any counter that is read by a *printer* and whose writes all live
inside a single function that itself has no caller. That is the exact shape here,
and it is two hops rather than one — which is why no one-hop rule found it.

---

## 3. Stop the `(sites: …)` tails from being optional

**Problem.** `vm-cli`'s dump prints, for three VarHandle families, a
`served=N declined=N` pair. Two of the three also print a
`(sites: singlepass=N osr=N)` tail; the third did not, and nothing said so. The
missing tail was invisible by inspection — the line looked complete — and only a
three-way comparison revealed it. Its own block comment explains why the tail
matters ("a non-zero bind count [...] with `served=0` here is the specific
failure this exists to name"), which is the argument for the tail being
mandatory, not optional.

**Proposal.** A tiny typed helper in `vm-cli` — `bind_line(family, served,
declined, sites_sp, sites_osr)` — so that a family reported without its bind
counts does not compile. This is cheaper than a gate and it makes the omission
impossible rather than detectable.

The general form of the lesson is worth stating: this dump is ~600 lines of
hand-written `eprintln!` whose *conventions* (print at zero; print the
denominator; split by cause not by total) are argued at length in comments and
enforced nowhere. Every one of this round's finds is a place a convention was
stated and not followed. A handful of small constructors for the recurring line
shapes would convert the most-repeated conventions into types.

---

## 4. Make `REVIEW-NOTE`s expire

**Problem.** `jit/src/platform.rs` ends in ~400 lines of `REVIEW-NOTE` whose
headline assertion was **false**: "the arena above is complete, tested and
reachable, but NOTHING CALLS IT: no `ExecutableBuffer::new_in` call site exists,
so with or without `CRATONVM_JIT_CODE_ARENA` the running VM behaves exactly as it
did before." Three production `new_in` sites exist, in `ir_lower.rs`,
`aarch64_backend.rs` and `lambda_adapter.rs`. Corrected in this lane's commit.

Three sub-notes were stale the same way: `REVIEW-NOTE 2(b)`'s four line numbers
were all wrong by thousands of lines, `REVIEW-NOTE 9`'s three "passages that need
fixing" have all since been fixed, and `REVIEW-NOTE 6` told the reader to set
`BASELINE` to 721 when `jit/tests/process_global_statics_ratchet.rs` reads 796 —
a number that would have turned that test **red** if acted on, because the
ratchet panics in both directions.

That last one is the shape worth naming: a stale note that instructs someone to
write a number into a gate is worse than no note.

**Proposal.** Two mechanical halves, both cheap:

* **Ban absolute counts in prose that names a gate.** A text gate over
  `jit/src/**` for a `REVIEW-NOTE` paragraph containing both a ratchet's file
  name and a bare integer. Notes should give the *procedure* ("take the count on
  the merged tree"), never the answer. `REVIEW-NOTE 6` now does.
* **Ban line numbers in `REVIEW-NOTE` prose.** Every single one in this file's
  notes was wrong. A gate that rejects `\.rs:\d+` inside a `REVIEW-NOTE` block
  forces the searchable-symbol form the rest of this repo already prefers, and
  the `r10-gauges` page's own instruction ("grep by name") is the same advice
  arrived at independently.

**Risk.** Both are text gates over comments, so the blast radius is a CI message.
The second would flag existing prose; land it with an allowlist or as a warning
for one cycle.

---

## 5. A "documented reading procedure" gate

**Problem.** Three of this lane's finds share a shape that is not "no reader" but
"the doc describes a two-number comparison and only one number is obtainable":

* `FINAL_DEVIRT_NATIVE_SHADOW_REFUSED`: *"Read it beside
  [`FINAL_INVOKEVIRTUAL_PINNED`]: together they say how much of the door's
  candidate set the screen takes."* The sibling was printed; this one was not.
* `LONG_LONG_VALUE_INTRINSIC_SITES`: *"The acceptance criterion, and not the
  ns/op."* Unevaluable — no accessor had a caller.
* `bytes_alignment_waste`: *"reported so that 'zero' is a measurement rather than
  a belief."* Not reported; `log_jit_code_arena_census` logged 24 fields and not
  this one. (Fixed in this lane's commit, along with `bytes_usable` and
  `largest_free_extent_bytes`.)

These are the highest-value finds per unit of effort in the whole round, because
the doc *tells you what to check*. The author wrote down the invariant and then
did not wire it.

**Proposal.** A gate over doc comments in `jit/src/**` for the phrases this
codebase actually uses to make such a promise — `Read it beside`,
`read against`, `reported so that`, `answerable without`, `acceptance criterion`,
`printed even when zero`, `Read the two` — and, for each, a check that every
`[\`SYMBOL\`]` intra-doc link named in the same paragraph has a reader outside its
defining file.

This is a heuristic and will have false positives. It is still worth building,
because the promise phrases are a small closed set in practice (this lane found
them by reading, and the same half-dozen recur), and because a hit is nearly
always either a real gap or a doc that should be reworded — both worth a human
look. Unlike C1–C5 it does not need an allowlist to start: run it as a report,
not a gate, and triage the list once.

---

## 6. Give the `jit_code_cache_cap_bytes` cap an actual enforcement point in the arena

**Not an instrumentation proposal — the one behavioural item here.**

**Problem.** `JitCodeArena::bump`'s doc claimed the region-slot scan was "bounded
by the code-cache cap divided by the region size, 16 at the default settings",
and `JIT_CODE_DEFAULT_REGION_BYTES`' reason (c) made the same claim. Nothing
enforces it. No code in `JitCodeArena`, `map_region`, `alloc` or
`ExecutableBuffer::new_in` reads `jit_code_cache_cap_bytes()`; `map_region` calls
`platform_alloc(self.region_bytes)` unconditionally and fails only when the OS
refuses. The cap is enforced one layer up against `COMMITTED_JIT_CODE_BYTES`, and
`new_in` adds only a *block's* rounded size to that counter, never the region's
16 MiB — so reserved region bytes are outside the capped quantity entirely. Both
docs now say so (corrected in this lane's commit); the code is unchanged.

**Why it is not simply a doc fix.** Two design claims rest on the bound: the cost
argument for the linear scans in `bump` and `free_if_arena`, and the decision
that the span table "never needs an index". Both are fine for a well-behaved
workload and neither is guaranteed.

**Proposal, and the trap in it.** A bounded reservation is the natural fix, and
`r10-gauges-no-extent-large-enough-is-unreachable-20260921-RESOLVED-20260922.md` proposes exactly
that — but **do not label its refusal `NO_EXTENT_LARGE_ENOUGH`.** An arena that
stops mapping at the cap and refuses is refusing *for want of the cap*, which is
`CAP_EXCEEDED`; `note_jit_code_cache_cap_refusal` already files that condition
from the admission gate, before codegen runs. Moving the same refusal down into
`map_region` only changes when it is detected (after codegen, so the body is
thrown away) and filing it as fragmentation would send an operator to
`external_fragmentation` for a workload whose answer is
`CRATONVM_JIT_CODE_CACHE_MAX_MB`. The argument is set out in full in that page's
closing block and in `code_alloc_failure::NO_EXTENT_LARGE_ENOUGH`'s doc.

So: bound the reservation if the address-space measurement REVIEW-NOTE 3 asks for
justifies it, label the refusal `CAP_EXCEEDED`, and leave
`NO_EXTENT_LARGE_ENOUGH` reserved for the narrower condition it actually names —
a **fixed** reservation with free bytes but no contiguous run long enough, which
additionally requires the refusal path to distinguish "no extent" from "no
space".

**Prerequisite, and it is now available.** None of this should be decided without
the arena census, which nothing emitted until this lane wired
`log_jit_code_arena_census` into `vm-cli`'s shutdown path. The protocol is
`CRATONVM_JIT_CODE_ARENA=1 RUST_LOG=info`, and it is REVIEW-NOTE 3's own list.
Measure first.

---

## 7. An `instanceof` site census, to match `checkcast`'s

`jit/src/x64/op_object.rs` handles `checkcast` and `instanceof` a hundred lines
apart with the same inline fast-path structure. `checkcast` feeds five counters
split by cause and printed by `tiered.rs`; `instanceof` feeds none, and
`CRATONVM_JIT_INSTANCEOF_FINAL_MISS` is **default ON** with its engagement
decided by a hard-coded name-matching predicate that nothing counts. Filed with a
concrete three-counter design and a warning about the statics ratchet:
`docs/internal/fixed-bugs/r10-readers-instanceof-arm-has-no-site-census-FIXED-20260922.md`.

Listed here because it is the clearest remaining instance of the round's class
that is a *gap* rather than a *lie*, and because the asymmetry only became
visible once the `checkcast` line was confirmed to be printed.
