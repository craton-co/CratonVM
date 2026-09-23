# JIT round 10, lane `nearglobals`: proposals for `jit/src/platform.rs`

**Written:** 2026-09-21, round 10 wave 7, lane `nearglobals`.
**Scope:** `jit/src/platform.rs` — the executable-memory and OS-interface layer.
**Landed in this lane's commit:** the fix for
`docs/internal/fixed-bugs/r10-readers-near-globals-place-retires-on-a-stale-cursor-FIXED-20260922.md`
(the retirement decision), the missing reader for `code_near_globals_stats`
(item 1 of `r10-readers-platform-dead-public-surface-RESOLVED-20260922.md`), and the
documentation corrections those two required.
**Read, not executed:** this lane may not build or run anything. `rustfmt --check`
was run on the two files it touched, which is a formatter and not a build.

Everything below is a proposal: something this lane either could not do from its
position or decided against doing, with the reasoning written out so that the next
person does not have to rederive it. The ordering is by how much a reader would
lose by skipping it.

---

## 1. Give `near_globals` a way to be re-armed, or say in writing that it cannot be

**Not done, and it is the largest remaining thing about this module.**

The retirement decision is now correct, but it is still permanent and still
process-global. `RETIRED` is an `AtomicBool` with no reset, `CURSOR` and `ANCHOR`
are process-global `AtomicUsize`, and `enabled()` latches its flag in a
`OnceLock`. Three consequences, all of which survive this lane's fix:

* **Two VMs in one process share the verdict.** A VM created after another has
  retired the strategy never walks the ladder, and AGENTS.md's rule against
  process globals for compatibility state is the same rule one category over. The
  file is on `jit/tests/process_global_statics_ratchet.rs`' baseline, so the
  statics are accounted for, but accounting is not the same as correctness.
* **Retirement cannot be undone by anything that changes the address space.**
  A later `munmap` of whatever was in the way does not re-arm it. In practice the
  address space only gets more crowded, so this is the right default — but it is a
  default and nothing says so.
* **A test cannot observe retirement through `place`.** This lane's fix works
  around that by extracting `walk`, which is pure and returns the decision as a
  value; the statics remain unreachable from a test.

Three shapes, in increasing cost:

1. **Write it down.** One paragraph on `RETIRED` saying that the verdict is
   per-process and deliberately unrecoverable, and why. Zero risk, and it is what
   turns "nobody thought about it" into "somebody decided". If nothing else on this
   list happens, this should.
2. **Retire per size class.** The defect this lane fixed was size-dependence
   leaking into a size-independent decision; the residue is that the decision is
   still taken on the evidence of one size. A `[AtomicBool; N]` bucketed by
   `size.next_power_of_two().trailing_zeros()` would let a 2 MB request retire
   without silencing 64-byte adapters. Costs N statics against the ratchet
   baseline, and the ratchet is a file this lane may not edit, so it has to be one
   commit with whoever owns it.
3. **Move the state into the arena handle.** `near_globals` state is really
   per-`JitRealm`, the way `DESPEC_SET` turned out to be. That is the correct
   answer and it is a much larger change than this module: `place` is called from
   `platform_alloc`, which has no realm to consult.

## 2. `synchronize_instruction_stream`: the module header names the wrong function

**Not done; a one-line doc edit outside this lane's scope.** Detailed as item 6 of
`r10-readers-platform-dead-public-surface-RESOLVED-20260922.md`. The header says the
reader's half of publishing code "is `synchronize_instruction_stream`"; every
production barrier goes through `synchronize_instruction_stream_for_epoch`, and
the ungated form has no caller outside this file. A porter who follows the header
writes a per-dispatch barrier where the gate exists specifically to avoid one.

The edit is to the module header and would also decide whether the ungated form
keeps its `pub`. It is a doc-only change and the reason it is not in this commit is
that this lane's edits are confined to the near-globals module and its census, and
a header paragraph about aarch64 publication is not that.

## 3. `CRATONVM_JIT_POISON_FREE` prints nothing when it poisons nothing

**Filed as its own page**, now fixed and retired to the internal tree as
`r10-nearglobals-poison-mode-cannot-report-that-it-poisoned-nothing-20260921-RESOLVED-20260922.md`.
The exact three-line edit is in that page. It belongs in
`vm/src/jit/code_cache_lifecycle.rs`, which another lane owns this wave.

Summary: `poisoned == 0` means "flag off", "this target has no poisoning arm", or
"every `mprotect(PROT_NONE)` failed", and the report distinguishes none of them
because it has no `else`. `jit_poison_free_enabled()` is `pub`, is the predicate
that disambiguates the first from the rest, and has no caller.

## 4. The rest of `r10-readers-platform-dead-public-surface-RESOLVED-20260922.md`

Items 2 through 5 of that page are left open, and each is left open for its own
reason rather than for lack of time:

* **Item 2, `JitCodeArena::is_inert`.** Deleting it is right and is a `pub`
  deletion, which touches the public-API ratchet. Note the correction this lane
  added to that page: the parenthetical claim that `vm-cli/src/main.rs` reads
  `JIT_CODE_ARENA_IS_INERT` is wrong — vm-cli mentions the name only in a comment.
  The item's conclusion still holds (the constant has three real readers inside
  `platform.rs` and the method has none), but a deletion commit should not lean on
  the vm-cli sentence.
* **Item 3, `JitError::AllocationFailed`.** Two defensible answers, and choosing
  between them is a decision about the allocation-failure ledger, not about dead
  code: routing `alloc_executable`'s `None` through the variant would have to agree
  with `code_alloc_failure::OS_REFUSED`'s existing accounting rather than duplicate
  it, and `alloc_executable`'s doc argues at length that it is the right place for
  that count. The cheap answer — delete the variant, its two arms and the test
  together — is probably correct and is still a public-API change.
* **Item 4, `instruction_stream_barriers_issued`.** The page's reasoning for NOT
  wiring it is sound and this lane endorses it: a reader in `vm-cli` would retire
  an allowlist entry and print a line that is structurally zero on every x86-64
  run, which converts "nothing reads this" into "this reads zero and you cannot
  tell why". The honest wiring is `cfg`-gated or prints an explicit
  "not applicable on this target", and that is an aarch64 bring-up judgement.
* **Item 5, the eviction-hook cluster.** Already has a written rationale in the
  file's own `REVIEW-NOTE`. An installed-by-nothing seam with a reason is a
  different thing from an orphaned instrument and should not be swept up with one.

## 5. A behavioural arm for the near-globals flag

**Not done; needs a run, which this lane cannot do.** The census now has a reader,
but that reader asserts the DEFAULT configuration. Nothing in the tree asserts
anything about a run with `CRATONVM_JIT_CODE_NEAR_GLOBALS=1`, and the honest reason
is that the outcome is host-dependent by construction: `mmap`'s first argument is a
hint and the kernel may ignore it, so on a host with no room near the anchor
`in_reach=0 fell_back=N retired=true` is the CORRECT reading and is
indistinguishable from a bug.

What can be asserted on any host, and what a follow-up with a build should write:

* With the flag on, `in_reach + fell_back` equals the number of `platform_alloc`
  calls made while the strategy was enabled. That is a conservation law, it is
  host-independent, and it is exactly the property whose violation was the
  `FELL_BACK` undercount wave 6 fixed — the counter could only read 0 or 1 while
  thousands of buffers fell back. Nothing pins it today, so the same regression
  could return.
* With the flag on and `CRATONVM_DBG_CODE_NEAR_GLOBALS=1`, the number of
  `[near-globals]` lines equals `in_reach + fell_back` minus the
  post-retirement early returns. Weaker, and needs the log.

The first is worth having. It needs a way to count `place` entries, which is a new
counter, which is a new static against the ratchet baseline — so it is one commit
with the ratchet owner, and it should only be taken if someone is also going to
run the flag.

## 6. Considered and rejected

Written down because each is the obvious next idea and each is worse than it looks.

* **Clear `CURSOR` whenever the cursor step refuses.** This is what the wave-6
  page suggested as an afterthought ("consider clearing `CURSOR` when (3) refuses
  it"). It would throw away a cursor whose base is still inside the window and
  which merely cannot hold one large buffer, costing every following small buffer a
  full ladder walk for a staleness it does not have. The landed fix clears only the
  permanently-out-of-window case, which `in_reach(cursor, 0, anchor)` identifies
  exactly because `size == 0` collapses the predicate to its base half.
* **Replace the `break` with a `continue` and leave the retirement condition
  alone** (the page's "Minimal" option). It fixes the reachable instance and leaves
  the shape: two unrelated conditions still produce one `None`, and the next person
  to touch the loop has the same decision to get wrong. The enum costs four
  variants and a `match`.
* **Require `rungs_probed == LADDER.len()` to retire.** Stricter and wrong in the
  expensive direction. A rung whose offset under/overflows at this anchor can never
  be probed, so a process with a low anchor would never retire and would pay eight
  `mmap`/`munmap` pairs per compile forever — the exact cost retirement exists to
  avoid. Hence `Hint::NoSuchAddress` as a distinct state from `Hint::TooBigHere`:
  the first neither earns nor blocks retirement, the second blocks it.
* **Make `walk`'s statics resettable for the test instead of extracting `walk`.**
  A `#[cfg(test)] fn reset_for_test()` would let a test drive `place` directly, and
  it would be a test-only mutator of process-global state in an allocation path,
  in a file whose review history is mostly about exactly that class of thing. The
  pure function is more code and no new surface that a production caller can reach.
* **Count the cursor step as a ladder rung.** Would make `probed` match the loop
  index and simplify the accounting, and would also mean a single successful
  packing address that the kernel relocates could, together with seven
  `NoSuchAddress` rungs, retire the strategy on one syscall's evidence. The cursor
  is a preference, not a rung.

## 7. Predicted ratchet impact of this lane's commit

Stated as a prediction so a mismatch is a finding, per this wave's instructions.

* **`scripts/baselines/orphan-instruments-allowlist.txt`: no change, 105 entries.**
  Nothing is retired and nothing is added. Derivations:
  * `code_near_globals_stats` gained a caller, but it was never on the list and is
    not a `C2` candidate: the `cfg` stub matches `DEF_C2`'s signature and its body
    plus the twelve following lines contain no `Ordering::`, `.load(`, `.store(`,
    `fetch_add(`, `Atomic` or `swap(`, so the atomic-body filter drops it. Checked
    by reading the window.
  * `near_globals::stats` is a `C2` candidate (`pub fn stats() -> (usize, ...)`,
    body is three relaxed loads) and is already classified as CALLED by pass 2's
    literal `stats(` search matching `rec.stats()`, `pool.stats()`, `heap.stats()`
    and `coordinator.stats()` in `jfr/src/recording.rs`, `gc/src/gen_heap.rs`,
    `gc/src/zgc.rs` and `jit/src/deopt.rs`. That classification does not change.
  * The new integration test would not have changed it anyway: pass 2 excludes
    `**/tests/**` from the caller search, so a call from `jit/tests/…` is invisible
    to the gate by design.
  * New items added to `platform.rs` are `pub(super)` (`Hint`, `Hint::addr`,
    `CursorUpdate`, `Walk`, `walk`) or private (`apply_cursor`). `C1` and `C2` both
    anchor on `pub fn`, so none of them enters the census.
* **`jit/tests/process_global_statics_ratchet.rs`: no change.** This commit adds no
  `static` declaration to `jit/src/**` — including in `#[cfg(test)]` code, which
  that ratchet counts. The five new tests use local `std::cell::Cell` counters
  precisely for that reason.
* **`scripts/check-no-diag-prints.sh` and the flag ratchets: no change.** No new
  `eprintln!` (the two new outcome strings go through the existing `report`, which
  is already behind `CRATONVM_DBG_CODE_NEAR_GLOBALS`), and no new flag name.
