// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! TEST-ONLY-API RATCHET — a one-way ratchet on `pub` items that only their own
//! tests reference.
//!
//! ## Why this exists (ARCH-2026-08-04 A7)
//!
//! `vm/src/runtime/lockfree_resolve.rs` carried 389 lines describing a
//! three-level resolution cache of which one tier was wired. The other two —
//! `ThreadLocalResolveCache`, `SharedResolutionState::resolve_method` /
//! `cache_method` / `resolve_field` / `cache_field`, and an `invalidate_all`
//! that took three write locks to clear two permanently-empty maps — had no
//! production callers at all. They survived for months because **their own unit
//! tests referenced them**, which is exactly the condition under which rustc's
//! `dead_code` lint stays silent.
//!
//! A 2026-07-26 audit found this and documented it accurately in the module
//! header. It was still there on 2026-08-04. Prose in a header does not delete
//! code; a failing build does.
//!
//! ## What it measures
//!
//! For every `pub` / `pub(crate)` / `pub(super)` item declared in a scanned
//! crate's `src` production code (outside `#[cfg(test)]`), count references to
//! its bare identifier across every workspace member. An item is an
//! **offender** when it has no production reference beyond its own declaration
//! *and* at least one reference from test code.
//!
//! **Two declaration crates, two independent scans, two frozen baselines:**
//! `vm/src` ([`BASELINE_OFFENDERS`]) and, since 2026-09-22, `jit/src`
//! ([`BASELINE_OFFENDERS_JIT`], which says why). The REFERENCE census has
//! always read every member of [`MEMBERS`]; only the declaration sweep was
//! `vm`-only, and an item that the predicate above described exactly sat in
//! `jit/src` for a round because of it. They are separate scans rather than one
//! merged map — see [`scan`] for the collision reason.
//!
//! References are bucketed as:
//!
//!   * *production* — `<member>/src/**.rs`, outside `#[cfg(test)]` regions,
//!     excluding comment lines and `impl` headers (see the two traps below).
//!   * *test* — `#[cfg(test)]` regions in `src`, plus all of `<member>/tests`,
//!     `<member>/benches` and `<member>/examples`.
//!
//! ## Two traps this scanner had to be taught (both cost a real miss)
//!
//! 1. **A doc comment is not a use.** The first version counted every line,
//!    so `lockfree_resolve.rs`'s own header — which *named* all the dead types
//!    while explaining that they were dead — pushed each of them to
//!    `prod >= 2`. The audit's honesty was hiding the corpse. Comment lines are
//!    now skipped.
//! 2. **`impl Foo` is not a use of `Foo`.** It is part of `Foo`'s definition.
//!    Counting it meant `ThreadLocalResolveCache` (three `impl` blocks, zero
//!    call sites outside tests) read as live. `impl` headers are now skipped.
//!
//! Both were found by checking the gate against a known answer — the A7 items —
//! rather than by reading it. A scanner nobody has watched fail is a scanner
//! that passes vacuously.
//!
//! ## Known limitation: cross-crate name collisions make this LENIENT
//!
//! Matching is by bare identifier, so an unrelated same-named item in another
//! crate masks an offender here. `lockfree_resolve.rs`'s `ResolutionKey`,
//! `ResolvedTarget`, `ResolvedField`, `cache_field`, `method_count` and
//! `field_count` were all masked this way by live, unrelated declarations in
//! `cratonvm-classloading` — the gate caught five of the eleven A7 items,
//! not eleven.
//!
//! That error direction is deliberate: this gate must never fail on code that
//! is actually used. It under-reports; it does not over-report. Do not "fix"
//! this by making matching path-aware unless you are prepared to re-verify the
//! whole baseline.
//!
//! ## Working the backlog down
//!
//! [`BASELINE_OFFENDERS`] is the frozen count. Adding a test-only `pub` item
//! pushes past it and fails; removing one requires lowering the baseline in the
//! same change. Run with `--nocapture` to see the full list — each entry is
//! either dead code to delete or an item whose only real caller is a test, in
//! which case it should usually be `#[cfg(test)]` itself.
//!
//! ```text
//! cargo test -p cratonvm-vm --test no_test_only_public_api -- --nocapture
//! ```

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Frozen upper bound on test-only `pub` items in `vm/src`.
///
/// Measured 2026-08-04 immediately after the A7 deletion. Restoring the deleted
/// code takes it to 327, and the five-item difference is exactly
/// `ThreadLocalResolveCache`, `cache_method`, `resolve_method`, `resolve_field`
/// and `invalidate_all` — verified by diffing the two offender lists, not by
/// reading the scanner. The remaining A7 items (`ResolutionKey`,
/// `ResolvedTarget`, `ResolvedField`, `cache_field`, `method_count`,
/// `field_count`) are masked by the cross-crate collision documented above.
///
/// **Re-measured 2026-08-24 after fixing the scanner, and this number is not
/// comparable to the 319 before it.** `split_regions` cleared its `pending`
/// flag only on a `{`, so a `#[cfg(test)]` on a BRACE-LESS item latched it for
/// the rest of the file. Seven such sites in `vm/src` were swallowing **7 732
/// production lines** -- `vm/src/memory/gc.rs` entire, from a
/// `#[cfg(test)] use crate::types::Value;` on line 16, and
/// `vm/src/runtime/resolve/mod.rs` from line 88. Over that tail the gate was
/// vacuous in both directions: a `pub` declared there could never be reported,
/// and a genuine production call there counted as a test reference.
///
/// With the scanner fixed the same two trees read 317 at `f0709247f` (where the
/// old scanner said 319) and 320 on `dev` at `e645a7349` (where it said 324) --
/// so of the five items that appeared to push the gate over, only THREE were
/// real. `frame_trace_wanted` and `publish_peer_jit_coverage_for_stw` are both
/// called from `safepoint_check`, ~130 lines past a
/// `#[cfg(test)] mod root_snapshot_cache_tests;`, and were never offenders.
///
/// Of the three real ones, two are fixed in this change (`helper_symbol` and
/// `other_thread_in_jit`, both now `#[cfg(test)]`), which is 320 - 2 = 318.
///
/// The third, `admit_direct_native_entry`, was deliberately LEFT. It was not
/// dead code: it was the destination of an in-flight migration, and
/// `the_two_direct_doors_take_exactly_the_addresses_h12_measured` in
/// `vm/src/jit/helpers.rs` existed precisely because the two compile doors
/// still took 7 + 2 raw helper addresses without asking it. Deleting it to buy
/// one point on this ratchet would have deleted the thing those doors were
/// supposed to migrate ONTO and destroyed the H12 record. The entry ended:
/// *"When O1/O2 land, that call appears, this item stops being an offender,
/// and the baseline drops with it."*
///
/// **They landed on 2026-09-22 and it did: 290 -> 289.** Both ladders obtain
/// every shadowing helper's address from `admit_direct_native_entry`, so the
/// item has thirteen production call sites and is off the list. Its witness
/// test is kept and re-baselined to zero (and renamed
/// `neither_direct_door_takes_a_raw_helper_address_any_more`), because the
/// thirty-one helper items being `pub(in crate::jit)` is what makes the
/// closure structural and a test is what notices if someone widens one back.
///
/// This is the shape the instruction at the top of this file asks for: an
/// item carried with its reason and its exit condition written down, exited by
/// the change that satisfied the condition rather than by raising a number.
///
/// # 289 -> 281 on the 2026-09-22 `origin/dev` merge
///
/// Eight more came off on `dev`'s side while O1/O2 was in flight; neither
/// branch crosses on its own. Lowered here because the gate's own ratchet arm
/// requires it in the same change -- a win left unlocked leaks back silently,
/// which is the whole reason that arm exists (ARCH-2026-08-04 A7).
///
/// Zero slack, matching `stub_ratchet`'s contract: this is the observed count,
/// not a rounded-up allowance.
///
/// # 318 -> 319 on 2026-09-02: `offload::release_submission`
///
/// **This is the `admit_direct_native_entry` case again, and it is recorded
/// the same way: named, reasoned, and with the condition that takes it back
/// down.** The instruction above is not to raise this number to make a
/// failure go away; it is not a bar on carrying an item whose reason is
/// written out.
///
/// Identified by diffing the offender LISTS, not the counts — one prebuilt
/// binary scored `vm/src` at HEAD and at `1410653b1` (the commit that last
/// settled this number), and `comm` gave exactly one newcomer and nothing
/// gone:
///
/// ```text
/// fn release_submission prod=1 test=19 vm/src/runtime/offload.rs
/// ```
///
/// `prod=1` is its own declaration. `b6133b92d` (the same day) rewrote the
/// synchronous JIT-caller path onto `dispatch_method_sync`, which — by
/// design, and correctly — never registers a submission, so it no longer has
/// anything to release. That deleted the last production call.
///
/// **It must not be gated or deleted to buy this point back.**
/// `submissions().write().remove(&handle)` inside it is the ONLY remove from
/// the GPU submission registry, against exactly one insert in
/// `register_submission` — so with no caller, nothing drains that map at all
/// and every async submission leaks its entry, its CUDA stream and its event
/// for the life of the process. Deleting the drain would cement the leak;
/// `#[cfg(test)]`-ing it would assert "test-only by design", which is false.
/// See
/// `docs/known-issues/gpu/submission-registry-has-no-drain-20260902.md`.
///
/// **Back to 318 when** either `GpuExecutor.releaseSubmission` gets a native
/// (the API the registry's own overflow warning tells callers to use, and
/// which `bench-gpu/GpuAsyncChainBench.java` already calls) or an
/// executor-close path drains the map. Either gives this item a production
/// caller, and the number comes down in that change.
/// **299 -> 297 on the 2026-09-09 `origin/dev` merge.** Neither side crossed
/// on its own — both branches declare 299 — so the two items came off the list
/// only once the two sets of `vm/src` changes were in one tree. Lowered here
/// because the gate's own ratchet arm requires it in the same change: a win
/// left unlocked leaks back silently, which is the whole reason that arm
/// exists (ARCH-2026-08-04 A7).
///
/// # A third disposition: `pub` for `vm/tests/`
///
/// The assertion below offers two remedies — delete it, or make it
/// `#[cfg(test)]`. There is a class that dichotomy cannot serve, and the
/// 2026-09-09 sweep walked into it: an item that exists for an INTEGRATION
/// test. `vm/tests/*` is a separate crate, so it reaches only `pub` API and is
/// compiled WITHOUT `cfg(test)` — gating such an item hides it from the very
/// tests it exists for, and deleting it deletes their instrument. The scan
/// counts those references as test references, correctly, so the item reads as
/// an offender and cannot stop being one.
///
/// Two of the 296 are exactly this, both deopt-census accessors read by
/// end-to-end tests that spawn a VM:
///
/// ```text
/// fn lambda_site_deopt_outcomes     vm/src/runtime/interpreter/lambda.rs
/// fn reset_deopt_frame_bail_counts  vm/src/runtime/interpreter/deopt_resume.rs
/// ```
///
/// Neither is dead and neither can be gated. They are carried, and named here
/// for the reason `admit_direct_native_entry` is: so that the next person to
/// lower this number does not spend an afternoon rediscovering that
/// `#[cfg(test)]` breaks the tests that item exists to serve.
///
/// **296 -> 292 on 2026-09-12**, with the code-cache lifecycle join
/// (`vm/src/jit/code_cache_lifecycle.rs`). Four items came off, all in that
/// file: `record_install`, `record_retirement` and `process_lifecycle` were
/// deleted — shims onto a model queue that no production path ever fed — and
/// `code_cache_lifecycle_raw` gained a production caller in
/// `code_cache_lifecycle_report`, which now reports the JIT's real accounting.
///
/// # 291 -> 291 on 2026-09-21: the three code-cache `record_*` gauges
///
/// **This entry exists because the number did NOT move, and a reader diffing
/// the offender list after that change would otherwise wonder why.** It is also
/// the worked example for a disposition none of the entries above record: an
/// item can leave the scan ENTIRELY. That is the assertion message's second
/// remedy (`#[cfg(test)]` it) applied together with dropping `pub`, and the
/// second half is what does the work here — `decl_name` needs the `pub`.
///
/// Round 10 wave 5 inverted the code-cache allocation-failure gauges from push
/// to pull (`vm/src/jit/code_cache_lifecycle.rs`; `cratonvm-vm` depends on
/// `cratonvm-jit` with no edge back, so the JIT could not call into the VM and
/// the VM had to fetch). That left three process-level `pub fn` wrappers —
/// `record_allocation_failure`, `record_capacity_bytes`, `record_free_space` —
/// with no caller and no possible caller. Wave 5 did not delete them, and
/// `docs/known-issues/jit/r10-gauges-code-cache-push-wrappers-outlived-their-direction-20260921.md`
/// says why in terms of this file: each wrapper shares its bare name with a
/// `CodeCacheLifecycle` INSTANCE method that only this module's own
/// `#[cfg(test)]` block calls, so deleting the wrappers would leave each name
/// at `prod=1` (its own declaration) with `test>=1` — the offender predicate
/// exactly — and take the count to 294 against the `<=` arm below.
///
/// **That arithmetic was checked against the predicate in wave 6 and it was
/// right.** Before the change each of the three names had exactly three
/// production lines in `vm/src` and nowhere else: the instance-method
/// declaration, the free-function declaration, and the free function's
/// one-line body. (Matching is by bare identifier and `decls` is keyed by
/// name, so the two declarations are ONE entry, not two — which is why the
/// answer is +3 and not +6.) Test references: three calls to
/// `record_allocation_failure` on a locally built instance, and one each for
/// the other two. Delete the wrappers alone and all three names land on
/// `prod=1, test>=1`.
///
/// Wave 6 owns both files and did not pay the three points. The wrappers are
/// deleted AND the three instance methods are now private and `#[cfg(test)]`,
/// so `decl_name` — which requires a `pub`/`pub(crate)`/`pub(super)` prefix,
/// and which never sees the lines at all because `split_regions` files a
/// `#[cfg(test)]`-gated method's whole body as test — no longer declares the
/// names. Three names leave the scan; none of the three was ever an offender;
/// the count stays 291.
///
/// **How that was arrived at, and how to recompute it.** By reading the
/// predicate, not by running the gate — wave 6's lanes may not build or test,
/// which is stated here because a frozen number defended by an unexecuted
/// argument should say so. The argument has two halves, and the second is the
/// one worth re-checking if this ever fails:
///
/// 1. the three names leave `decls`, and they were not offenders, so
///    `offenders.len()` cannot change *through them*;
/// 2. no OTHER name's `prod` count is pushed to `<= 1` by the lines that went
///    away. The deleted and re-gated lines mention, besides the three names
///    themselves: `PROCESS_LIFECYCLE` (a private `static`, so `decl_name`
///    skips it and it is not in `decls`), `FreeSpace` (`pub struct`, which
///    keeps five production mentions: its declaration,
///    `external_fragmentation`'s signature, `from_raw`'s construction,
///    `arena_free_space`'s return type and its body), and `get` / `store`
///    (both declared in `vm/src` and both referenced hundreds of times across
///    the workspace as `HashMap::get` and `Atomic*::store`). Every other
///    identifier on those lines — `requested_bytes`, `reason`, `bytes`,
///    `free`, `extents`, the six counter field names — is declared nowhere in
///    `vm/src` as a `pub` item, so it is not in `decls` and `tally` ignores it.
///
/// To recompute from scratch: run this test with `--nocapture` before and after
/// the change and `comm` the two offender lists, which is the method every
/// other entry in this comment used. A list diff, never a count diff — the
/// 2026-08-24 and 2026-09-02 entries are both records of a count that moved for
/// reasons other than the one assumed.
///
/// # 291 -> 290 on 2026-09-21: `code_cache_lifecycle_report` gained a reader
///
/// One item leaves the offender list, and it is the one the entry above
/// predicted would have to. Round 10 wave 7 (lane `report`) closed
/// `docs/internal/retired/r10-wrappers-process-report-has-no-reader-and-seven-unfed-fields-20260921-RETIRED-20260922.md`,
/// whose Finding 1 is that nothing in production called
/// `vm/src/jit/code_cache_lifecycle.rs`'s `code_cache_lifecycle_report()` —
/// so the whole JIT code-cache pull that waves 5 and 6 built ran only under
/// `cargo test`. That page names this file as the corroboration: "the same
/// function is one of the frozen offenders in
/// `vm/tests/no_test_only_public_api.rs` (production reference count 1 — its own
/// declaration — and two test references). It has been sitting inside that
/// ratchet's 291 as accepted debt, with nothing naming it."
///
/// It now has a production caller: `maybe_dump_shutdown_reports` in
/// `vm-cli/src/main.rs`, under `flags().jit.method_stats`, beside the
/// `[cratonvm] implicit null-check table health:` line that this same round
/// added for the same reason.
///
/// **How that was arrived at, and how to recompute it.** By reading the
/// predicate, not by running the gate — this lane may not build or test, which
/// is stated here for the same reason the entry above states it: a frozen number
/// defended by an unexecuted argument should say so. Three things had to hold,
/// and the third is the one worth re-checking if this ever fails.
///
/// 1. **`code_cache_lifecycle_report` was an offender and stops being one.**
///    The predicate is `prod <= 1 && test >= 1`. Its production references are
///    now exactly two: its own declaration in
///    `vm/src/jit/code_cache_lifecycle.rs`, and the new call in
///    `vm-cli/src/main.rs` (`vm-cli` is in [`MEMBERS`], and `<member>/src/**`
///    outside `#[cfg(test)]` is production). Every other mention in the tree is
///    on a line beginning `//`, `///` or `//!`, which the reference census
///    skips. `prod = 2` fails `prod <= 1`. That is -1.
/// 2. **No other name's production count FELL.** The change removes no call.
///    `retire_reason_breakdown` is the only one that came close: its
///    `Display` call site now sits inside an `if r.modelled_sources
///    .retire_reasons`, which moves the line but does not delete it, so it
///    stays at `prod = 2` (declaration plus call). Had it dropped to 1 it still
///    could not have become an offender — it has no test reference at all, and
///    the predicate needs `test >= 1`.
/// 3. **No name was pushed INTO the offender set by a new TEST reference.**
///    This is the direction that would cancel the -1 and leave the count at
///    291, so it is the one to check. The four new tests in
///    `code_cache_lifecycle.rs` reference one newly-declared name,
///    `ModelledFieldSources`, and otherwise only names those tests' neighbours
///    already referenced. `ModelledFieldSources` is a `pub struct`, so
///    `decl_name` does declare it — and it carries four production references
///    besides: the field `modelled_sources: ModelledFieldSources` on
///    `CodeCacheLifecycleRaw`, `ModelledFieldSources::ALL` in
///    `CodeCacheLifecycle::raw`, and `ModelledFieldSources::NONE` in
///    `code_cache_lifecycle_raw`. (`impl ModelledFieldSources {` is NOT one:
///    the census skips any line starting `impl`, which is trap 2 in this
///    file's header.) `prod = 4`, so it is not an offender.
///
///    Its two associated constants, `pub const ALL` and `pub const NONE`, do
///    not enter `decls` at all, and the reason is a quirk of `decl_name` worth
///    writing down because it is easy to assume otherwise: the modifier loop
///    strips `"const "` from `"const ALL: Self = …"` BEFORE the keyword loop
///    runs, so the keyword loop is handed `"ALL: Self = …"`, matches none of
///    `fn`/`struct`/`enum`/`trait`/`type`/`const`/`static`, and returns `None`.
///    A `pub const` inside an `impl` is invisible to this scanner. That is a
///    leniency, consistent with the header's "it under-reports; it does not
///    over-report", and not something this change relies on beyond arithmetic.
///
/// One further hazard, checked because it would have silently reversed the
/// whole thing: the comment above the new call in `vm-cli/src/main.rs` contains
/// the literal text `#[cfg(test)] mod tests` while explaining what the census
/// used to find. [`split_regions`] guards its `#[cfg(test)]` arm with
/// `!is_comment`, and the line begins with `//`, so it neither opens a test
/// region nor latches `pending`. Had it done either, the new call 60 lines
/// later would have been filed as a TEST reference, `prod` would have stayed at
/// 1, and this baseline would be wrong in the direction that fails the build.
/// This is the same "the first literal `#[cfg(test)]` is inside a doc comment"
/// lesson [`split_regions`]' own doc comment says was learned the expensive way.
///
/// To recompute: `--nocapture` before and after, `comm` the two lists, expect
/// exactly one line gone — `fn code_cache_lifecycle_report` — and nothing new.
///
/// # 290 -> 290 on 2026-09-22: the code-cache field producers
///
/// **Another entry for a number that did NOT move**, and it exists for the
/// reason the 2026-09-21 one does: a reader diffing `vm/src/jit/
/// code_cache_lifecycle.rs` after round 10 wave 8 will find that file
/// substantially changed and should not have to work out why this constant
/// stayed put.
///
/// Wave 8 (lane `producers`) gave three of the seven suppressed
/// `CodeCacheLifecycleRaw` field groups a producer in `cratonvm-jit` and pulled
/// them here. In `vm/src`, the change consists of: two new `pub` FIELDS
/// (`CodeCacheLifecycleRaw::oldest_deferral_generations` and
/// `SweepOutcome::oldest_deferral_generations`), one new `const _` assertion
/// block, statements added to three function bodies, two new `Display` arms,
/// rewritten doc comments, and edits to two tests. In `vm-cli/src`, three added
/// `eprintln!` blocks.
///
/// Nothing in that list can move the count, and each clause of that claim is
/// worth stating because two of them are the traps this file's header names:
///
/// 1. **A struct FIELD is invisible to [`decl_name`].** `pub
///    oldest_deferral_generations: Option<u64>,` has `"pub "` stripped, leaves
///    `"oldest_deferral_generations: Option<u64>,"`, and matches none of
///    `fn`/`struct`/`enum`/`trait`/`type`/`const`/`static`, so it never enters
///    `decls`. The same quirk the 2026-09-21 entry records for a `pub const`
///    inside an `impl`, from the same loop.
/// 2. **No production reference was REMOVED.** The change deletes no call. The
///    nearest thing is `ModelledFieldSources::NONE`, whose one production use
///    moved from being the whole assignment in `code_cache_lifecycle_raw` to
///    being the assignment two flags are then set back on — it is still
///    assigned, so its count is unchanged, and `NONE` is a `pub const` inside an
///    `impl` and therefore not in `decls` anyway.
/// 3. **No name was pushed INTO the offender set by a new TEST reference**,
///    which is the direction that would have raised it.
///    `the_process_report_suppresses_every_field_it_cannot_source` was rewritten
///    to assert the flags field by field and in doing so REMOVED its
///    `ModelledFieldSources::NONE` reference and added references to
///    `CodeCacheLifecycleReport`, `from_raw` and `code_cache_lifecycle_raw` —
///    all of which carry two or more production references, so none can be an
///    offender. `a_process_sweep_does_not_claim_a_deferral_age` was renamed to
///    `a_process_sweep_does_not_claim_a_deferral_age_in_sweeps`; a `#[test]`
///    function's own name is declared inside a `#[cfg(test)]` region, which
///    [`split_regions`] files as test, so it is not in `decls` and renaming it
///    changes nothing here.
/// 4. The new `jit/tests/r10_producers_*.rs` files are scanned as TEST code
///    (`jit` is in [`MEMBERS`]), so any `vm/src` name they happened to mention
///    would count as a test reference. They mention none: every identifier in
///    them resolves in `cratonvm_jit`, `cratonvm_jit_api`, `cratonvm_types` or
///    `std`.
///
/// **How that was arrived at, and how to recompute it.** Not by reading the
/// predicate alone this time. This lane may not run `cargo test`, so the
/// scanner's three passes — [`split_regions`], [`decl_name`] and the reference
/// census, including `brace_delta`'s string/char handling and the `impl`/comment
/// skips — were reimplemented as a script and run over the worktree TWICE: once
/// with this lane's four source files reverted to `HEAD`, and once with them
/// restored. Both runs reported **3346 declarations and 290 offenders**, and
/// `diff` of the two full offender listings was EMPTY — not merely equal in
/// count, which is the distinction the 2026-08-24 and 2026-09-02 entries above
/// were both written about.
///
/// Two independent reasons that is evidence rather than a coincidence: the port
/// reproduces the frozen 290 exactly on the pristine tree, which it could not do
/// if its predicate differed from this file's; and the comparison that matters is
/// a LIST diff, which is insensitive to a port that is uniformly off.
///
/// It is still a port and not this test. If the orchestrator's run disagrees,
/// believe the run — and the disagreement is then a finding about the port, since
/// the offender LIST was identical across the change either way.
const BASELINE_OFFENDERS: usize = 281;

/// The same ratchet, frozen for `jit/src`.
///
/// # Why a second declaration crate exists at all
///
/// Round 10 lane `offsetkey` found `jit/src/osr_exit.rs::exceptional_reason_at_bci`
/// — a `pub fn` whose only references outside its own declaration were two
/// asserts in its file's `#[cfg(test)]` module. That is this gate's offender
/// predicate stated word for word, and this gate did not fire, because until
/// 2026-09-22 its DECLARATION pass read `vm/src` and nothing else while its
/// REFERENCE pass already read every member of [`MEMBERS`]. The machinery was
/// there; only the sweep was missing. Two other mechanisms missed the same item
/// for two other reasons — `scripts/check-orphan-instruments.sh` by shape (it
/// matches `record_*`/`note_*`, zero-argument atomic readers and `*_EVENTS`
/// rows; this is a two-argument enum-returning verdict function) and rustc's
/// `dead_code` by the `pub`-plus-own-test-reference mechanism this whole file
/// was written for. See
/// `docs/internal/retired/r10-offsetkey-exceptional-reason-at-bci-is-unwired-and-cannot-refuse-20260921-RETIRED-20260922.md`
/// and §2 of `docs/feature-designs/jit-r10-offsetkey-proposals.md`.
///
/// `exceptional_reason_at_bci` itself is NOT in this number: the same change
/// that added this scan gave it its production caller (the OSR exception-exit
/// transfer in `vm/src/runtime/interpreter/deopt_resume.rs`), which is the
/// disposition this gate's assertion message asks for rather than a point
/// bought by deleting or gating something.
///
/// # The count is large, and that is expected
///
/// `jit` exports a wide surface that `vm` consumes, and — the direction that
/// matters here — it carries a great deal of structure that only `jit`'s own
/// tests and `jit/tests/*` exercise. This baseline is a CENSUS frozen as found,
/// not a claim that every entry is dead. Work it down the way the `vm` baseline
/// was worked down: `--nocapture`, `comm` the lists (never the counts), and
/// lower the number in the same change that earns it.
///
/// Two leniencies from the header apply MORE strongly here, and both under-report:
///
///  * cross-crate bare-name collisions — a `jit` item masked by a live
///    same-named `vm`, `types` or `classloading` item never reads as an
///    offender. `jit` shares far more vocabulary with `types` and `jit-api`
///    than `vm` does with anything;
///  * a name declared in BOTH `vm/src` and `jit/src` is scored once per scan
///    against the same workspace-wide reference counts, so such an item is an
///    offender in both or neither. That is why [`scan`] keeps one map per
///    declaration crate instead of merging them.
///
/// Measured 2026-09-22 on `origin/dev` at 5a6d5073b plus this change:
/// **163 offenders across 3,016 declarations**, against `vm`'s 281 across
/// 3,324. The `vm` scan is unaffected by the addition — it reported 281 in the
/// same run, which is its frozen number.
const BASELINE_OFFENDERS_JIT: usize = 163;

/// Declaration floor for the `jit/src` scan — the same anti-vacuity guard
/// [`MIN_DECLARATIONS_SCANNED`] is, sized for that crate.
const MIN_DECLARATIONS_SCANNED_JIT: usize = 1_000;

/// Minimum number of declarations the scan must find before its result means
/// anything.
///
/// Without this, any change that broke the declaration regex, mis-rooted the
/// workspace walk, or pointed the scan at an empty directory would report zero
/// offenders and **pass**. A gate that cannot distinguish "clean" from "did not
/// run" is not a gate. The real count is ~2,666; 2,000 leaves room for genuine
/// deletion without leaving room for a silent no-op.
const MIN_DECLARATIONS_SCANNED: usize = 2_000;

/// Workspace members to search for references. Mirrors the `[workspace]
/// members` list in the root `Cargo.toml`.
const MEMBERS: &[&str] = &[
    "reader",
    "types",
    "native-api",
    "native-collections",
    "native-io",
    "native-builtins",
    "native-builtins-crypto",
    "native-builtins-security",
    "native-awt",
    "jit-api",
    "jit",
    "jit-cuda",
    "cuda-bridge",
    "classloading",
    "craton-gpu",
    "gc",
    "vm",
    "vm-cli",
    "jfr",
    "libcratonvm",
    "cratonvm-embed",
    "difftest",
];

/// Repository root — `vm/`'s parent.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("vm/ must have a parent")
        .to_path_buf()
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Split one file into (production lines, test lines).
///
/// Skips the body of any genuine `#[cfg(test)]`-gated item — a real attribute
/// line, not a comment mentioning one — tracking brace depth so both a trailing
/// `mod tests { .. }` and a mid-file `#[cfg(test)] fn helper() { .. }` are
/// excluded. This is the same shape as `scan_production_section` in
/// `vm/src/runtime/interpreter/tests.rs`, which learned the "the first literal
/// `#[cfg(test)]` is inside a doc comment" lesson the expensive way.
fn split_regions(src: &str) -> (Vec<&str>, Vec<&str>) {
    let mut prod = Vec::new();
    let mut test = Vec::new();
    let mut pending = false;
    let mut depth: i32 = 0;

    for line in src.lines() {
        let t = line.trim_start();
        let is_comment = t.starts_with("//") || t.starts_with('*');

        if !is_comment && depth == 0 && !pending && t.starts_with("#[cfg(test)]") {
            pending = true;
            test.push(line);
            continue;
        }
        if pending {
            test.push(line);
            depth += brace_delta(line);
            if line.contains('{') && depth <= 0 {
                pending = false;
                depth = 0;
            } else if depth > 0 {
                pending = false;
            } else if !is_comment && code_part(line).contains(';') {
                // A BRACE-LESS gated item: `use x;`, `mod y;`, `const Z: T = v;`.
                // It ends on this line, and without this arm `pending` latches
                // FOREVER -- every remaining line in the file is then filed as
                // test, which makes the gate vacuous over the tail of that file
                // in both directions: a `pub` declared there is never seen (so it
                // can never be an offender) and a genuine production CALL there is
                // counted as a test reference (so the callee it keeps alive reads
                // as test-only).
                //
                // Measured on 2026-08-24: seven such sites in `vm/src` were
                // swallowing 7 732 production lines, `vm/src/memory/gc.rs` entire
                // (a `#[cfg(test)] use crate::types::Value;` on line 16) and
                // `vm/src/runtime/resolve/mod.rs` from line 88. Three of the five
                // offenders that pushed this gate over its baseline were artifacts
                // of exactly this -- `frame_trace_wanted` and
                // `publish_peer_jit_coverage_for_stw` are both called from
                // `safepoint_check`, 130 lines past a `#[cfg(test)] mod
                // root_snapshot_cache_tests;`.
                //
                // An attribute line (`#[allow(..)]`) carries no `;` and keeps
                // `pending` raised, which is what a multi-attribute item needs; a
                // comment line is excluded because a prose `;` inside the doc
                // block between the attribute and its item would end the region
                // early.
                pending = false;
                depth = 0;
            }
            continue;
        }
        if depth > 0 {
            test.push(line);
            depth += brace_delta(line);
            if depth < 0 {
                depth = 0;
            }
            continue;
        }
        prod.push(line);
    }
    (prod, test)
}

/// The line with any trailing `//` comment removed.
///
/// The semicolon arm of [`split_regions`] must look at CODE only. Written
/// without this, `#[allow(deprecated)] // prose; with a semicolon` ended the
/// pending region on the attribute line and let the whole
/// `mod tests { .. }` body that followed leak into production --- caught by
/// `a_braced_cfg_test_block_still_swallows_its_body`, which is the entire
/// reason that test exists.
fn code_part(line: &str) -> &str {
    match line.find("//") {
        Some(i) => &line[..i],
        None => line,
    }
}

/// The net BLOCK-brace count of a line: braces inside a string literal, a
/// char literal or a line comment are not block delimiters.
///
/// Counting the raw line was the SECOND incarnation of the region-latch bug
/// the `;` arm of [`split_regions`] fixed. `vm/src/vm/vm_exec.rs:31238` is
/// a `format!` whose escaped `{{` is a literal brace in the OUTPUT, not an
/// opened block; counted raw it left the `#[cfg(test)] mod tests` region at
/// depth 2 forever, so 3 633 production lines of that file were filed as
/// test. MEASURED 2026-09-02 across `vm/src`: five files, 5 131 production
/// lines, and the visible symptom was `with_cas_lock` reported `prod=1`
/// while `vm_exec.rs:33264` calls it in production. With this counting,
/// every one of the 142 files closes its regions.
fn brace_delta(line: &str) -> i32 {
    let b = line.as_bytes();
    let mut depth = 0i32;
    let mut i = 0usize;
    let mut in_str = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' => in_str = true,
            // A line comment ends the code part of the line.
            b'/' if i + 1 < b.len() && b[i + 1] == b'/' => break,
            // A char literal holding a brace. Matched narrowly (three bytes,
            // brace in the middle) so a lifetime such as `&'a T` is never
            // mistaken for a quote that needs closing.
            b'\''
                if i + 2 < b.len()
                    && b[i + 2] == b'\''
                    && (b[i + 1] == b'{' || b[i + 1] == b'}') =>
            {
                i += 3;
                continue;
            }
            b'{' => depth += 1,
            b'}' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    depth
}

/// Extract the declared name from a `pub` item line, if it is one.
fn decl_name(line: &str) -> Option<(&'static str, String)> {
    let t = line.trim_start();
    let rest = t
        .strip_prefix("pub(crate) ")
        .or_else(|| t.strip_prefix("pub(super) "))
        .or_else(|| t.strip_prefix("pub "))?;
    // Strip modifiers that may appear between `pub` and the item keyword.
    let mut rest = rest.trim_start();
    for m in ["async ", "unsafe ", "extern \"C\" ", "const "] {
        if let Some(r) = rest.strip_prefix(m) {
            rest = r.trim_start();
        }
    }
    for kw in [
        "fn ", "struct ", "enum ", "trait ", "type ", "const ", "static ",
    ] {
        if let Some(after) = rest.strip_prefix(kw) {
            let name: String = after
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if name.is_empty() || !name.starts_with(|c: char| c.is_alphabetic() || c == '_') {
                return None;
            }
            let kind: &'static str = match kw {
                "fn " => "fn",
                "struct " => "struct",
                "enum " => "enum",
                "trait " => "trait",
                "type " => "type",
                "const " => "const",
                _ => "static",
            };
            return Some((kind, name));
        }
    }
    None
}

/// Count identifier occurrences in `line` that are in `names`.
///
/// Rust identifiers in this tree are ASCII, but the *lines* are not — comments
/// carry `§`, `→`, `—` and friends. Walking by byte index and slicing on it
/// panics mid-codepoint, so this walks `char_indices` and only ever slices at a
/// boundary it was handed.
fn tally(line: &str, names: &HashSet<String>, counts: &mut HashMap<String, usize>) {
    let mut start: Option<usize> = None;
    for (idx, ch) in line.char_indices() {
        let is_start = ch.is_ascii_alphabetic() || ch == '_';
        let is_cont = ch.is_ascii_alphanumeric() || ch == '_';
        match start {
            None => {
                if is_start {
                    start = Some(idx);
                }
            }
            Some(s) => {
                if !is_cont {
                    let word = &line[s..idx];
                    if names.contains(word) {
                        *counts.entry(word.to_string()).or_insert(0) += 1;
                    }
                    start = if is_start { Some(idx) } else { None };
                }
            }
        }
    }
    if let Some(s) = start {
        let word = &line[s..];
        if names.contains(word) {
            *counts.entry(word.to_string()).or_insert(0) += 1;
        }
    }
}

/// One offender row: `(name, kind, declaring path, prod refs, test refs)`.
type Offender = (String, &'static str, String, usize, usize);

/// Run the three passes for one declaration crate and return
/// `(declarations scanned, offenders)`.
///
/// `decl_crate` names the workspace member whose `src` tree supplies the
/// DECLARATIONS. The reference census is unchanged and always reads every
/// member in [`MEMBERS`] — that asymmetry is the scanner's shape, and §2 of
/// `docs/feature-designs/jit-r10-offsetkey-proposals.md` is the observation that
/// only the declaration half was ever `vm`-only.
///
/// Each declaration crate gets its own `#[test]` and its own frozen baseline
/// rather than sharing one. Merging them into a single `decls` map would make
/// the cross-crate collision leniency WORSE in a way that is invisible from the
/// count: `decls.entry(name).or_insert(..)` keeps the first declaration of a
/// bare name, so a `jit` item sharing a name with a `vm` item would silently
/// inherit the other one's path and never be reportable. Two scans, two
/// baselines, and a name declared in both crates is assessed once per crate.
fn scan(decl_crate: &str) -> (usize, Vec<Offender>) {
    let root = repo_root();

    // ---- 1. Declarations in <decl_crate>/src production code --------------
    let mut decl_files = Vec::new();
    rust_files(&root.join(decl_crate).join("src"), &mut decl_files);
    assert!(
        !decl_files.is_empty(),
        "found no .rs files under {decl_crate}/src — the scan is mis-rooted at {}",
        root.display()
    );

    // name -> (kind, relative path)
    let mut decls: HashMap<String, (&'static str, String)> = HashMap::new();
    for path in &decl_files {
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        let (prod, _) = split_regions(&src);
        for line in prod {
            if let Some((kind, name)) = decl_name(line) {
                let rel = path
                    .strip_prefix(&root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .into_owned();
                decls.entry(name).or_insert((kind, rel));
            }
        }
    }

    let names: HashSet<String> = decls.keys().cloned().collect();

    // ---- 2. Reference census across the workspace -------------------------
    let mut prod_refs: HashMap<String, usize> = HashMap::new();
    let mut test_refs: HashMap<String, usize> = HashMap::new();

    for member in MEMBERS {
        let base = root.join(member);
        // src: split into production / test regions.
        let mut files = Vec::new();
        rust_files(&base.join("src"), &mut files);
        for path in &files {
            let Ok(src) = std::fs::read_to_string(path) else {
                continue;
            };
            let (prod, test) = split_regions(&src);
            for line in prod {
                let t = line.trim_start();
                // See the module header: a comment is not a use, and an `impl`
                // header is part of the type's own definition.
                if t.starts_with("//") || t.starts_with('*') || t.starts_with("impl") {
                    continue;
                }
                tally(line, &names, &mut prod_refs);
            }
            for line in test {
                tally(line, &names, &mut test_refs);
            }
        }
        // tests / benches / examples: entirely test code.
        for sub in ["tests", "benches", "examples"] {
            let mut files = Vec::new();
            rust_files(&base.join(sub), &mut files);
            for path in &files {
                let Ok(src) = std::fs::read_to_string(path) else {
                    continue;
                };
                for line in src.lines() {
                    tally(line, &names, &mut test_refs);
                }
            }
        }
    }

    // ---- 3. Offenders -----------------------------------------------------
    // The declaration line itself counts as one production reference, so a
    // genuinely-unused item sits at exactly 1.
    let mut offenders: Vec<Offender> = decls
        .iter()
        .filter_map(|(name, (kind, path))| {
            let p = prod_refs.get(name).copied().unwrap_or(0);
            let t = test_refs.get(name).copied().unwrap_or(0);
            (p <= 1 && t >= 1).then(|| (name.clone(), *kind, path.clone(), p, t))
        })
        .collect();
    offenders.sort();
    (decls.len(), offenders)
}

/// Print an offender list and apply the two-sided ratchet against `baseline`.
fn assert_ratchet(decl_crate: &str, floor: usize, baseline: usize) {
    let (scanned, offenders) = scan(decl_crate);

    assert!(
        scanned >= floor,
        "only {scanned} declarations scanned in {decl_crate}/src (floor {floor}). \
         The scan found almost nothing, which means it broke rather than that \
         the tree got clean — check decl_name() and the {decl_crate}/src walk \
         before touching the baseline."
    );

    println!(
        "test-only-api [{decl_crate}]: {} offenders across {scanned} declarations (baseline {baseline})",
        offenders.len()
    );
    for (name, kind, path, p, t) in &offenders {
        println!("  {kind:7} {name:55} prod={p} test={t}  {path}");
    }

    assert!(
        offenders.len() <= baseline,
        "test-only public API in {decl_crate}/src rose to {} (baseline {baseline}).\n\
         A `pub` item there is now referenced only by tests — it is either dead \
         code to delete, or an item whose sole real caller is a test and which \
         should therefore be `#[cfg(test)]` itself.\n\
         Run with --nocapture for the full list. Do NOT raise the baseline to \
         make this pass; that is the exact failure mode this gate exists to \
         stop (see ARCH-2026-08-04 A7).",
        offenders.len()
    );

    // Ratchet: if the count dropped, the baseline must drop with it, otherwise
    // the win silently leaks back.
    assert_eq!(
        offenders.len(),
        baseline,
        "test-only public API in {decl_crate}/src fell to {} (baseline {baseline}). \
         Good — now lower the baseline to {} in this same change to lock it in.",
        offenders.len(),
        offenders.len()
    );
}

#[test]
fn no_new_test_only_public_api() {
    assert_ratchet("vm", MIN_DECLARATIONS_SCANNED, BASELINE_OFFENDERS);
}

/// The same ratchet, with `jit/src` supplying the declarations.
///
/// See [`BASELINE_OFFENDERS_JIT`] for why this exists and what it cost to
/// freeze.
#[test]
fn no_new_test_only_public_api_in_jit() {
    assert_ratchet("jit", MIN_DECLARATIONS_SCANNED_JIT, BASELINE_OFFENDERS_JIT);
}

// ---------------------------------------------------------------------------
// The scanner's own tests.
//
// This file's header says it in as many words: "A scanner nobody has watched
// fail is a scanner that passes vacuously." It had no self-tests, and it was
// wrong -- for long enough that 7 732 lines of `vm/src` were invisible to it.
// These pin the region split against the shapes that broke it.
// ---------------------------------------------------------------------------

/// A `#[cfg(test)]` on a BRACE-LESS item must end at that item's `;`.
///
/// The bug this pins: `pending` was cleared only by a `{`, so `#[cfg(test)] use
/// x;` / `#[cfg(test)] mod y;` latched it forever and every remaining line in
/// the file was filed as test. `vm/src/memory/gc.rs` has such a `use` on line
/// 16 -- the whole file was invisible. Both directions matter: a `pub` in the
/// swallowed tail can never be reported (the gate goes quiet), and a genuine
/// production call there is counted as a test reference (a live item reads as
/// test-only).
#[test]
fn a_braceless_cfg_test_item_ends_at_its_semicolon() {
    for item in ["use crate::types::Value;", "mod guard;", "pub mod guard;"] {
        let src = format!("use std::foo;\n#[cfg(test)]\n{item}\npub fn live() {{}}\nlive();\n");
        let (prod, test) = split_regions(&src);
        assert!(
            prod.iter().any(|l| l.contains("pub fn live")),
            "the declaration after `{item}` was swallowed into the test region"
        );
        assert!(
            prod.iter().any(|l| l.trim() == "live();"),
            "a production CALL after `{item}` was counted as a test reference"
        );
        assert!(
            test.iter().any(|l| l.contains(item)),
            "the gated item itself belongs to the test region"
        );
        assert_eq!(
            test.len(),
            2,
            "only the attribute and its own item are test lines, got {test:?}"
        );
    }
}

/// A `#[cfg(test)] mod tests { .. }` still swallows its whole block, and
/// attributes and doc comments may sit between the attribute and the item.
///
/// `vm/src/threading/varhandle.rs` has exactly this shape, with a `;` inside
/// the prose of the comment between `#[allow(deprecated)]` and `mod tests {` --
/// which is why the semicolon arm above must skip comment lines.
#[test]
fn a_braced_cfg_test_block_still_swallows_its_body() {
    let src = "pub fn live() {}\n\
               #[cfg(test)]\n\
               #[allow(deprecated)] // prose; with a semicolon in it\n\
               mod tests {\n\
               fn helper() {}\n\
               }\n\
               pub fn also_live() {}\n";
    let (prod, test) = split_regions(src);
    assert!(prod.iter().any(|l| l.contains("pub fn live")));
    assert!(
        prod.iter().any(|l| l.contains("pub fn also_live")),
        "the block closed, so what follows it is production again"
    );
    assert!(
        test.iter().any(|l| l.contains("fn helper")),
        "the block body is test"
    );
    assert!(
        !prod.iter().any(|l| l.contains("fn helper")),
        "the block body must not leak into production"
    );
}

/// A `#[cfg(test)]` mentioned inside a comment is not an attribute.
///
/// The header records this as a lesson learned "the expensive way" in a sibling
/// scanner; it was never pinned here.
#[test]
fn a_cfg_test_inside_a_comment_opens_no_region() {
    let src = "// #[cfg(test)] is discussed here\npub fn live() {}\n";
    let (prod, test) = split_regions(src);
    assert!(prod.iter().any(|l| l.contains("pub fn live")));
    assert!(
        test.is_empty(),
        "no region should have opened, got {test:?}"
    );
}
