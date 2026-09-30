# Round 10, lane `interner`: proposals

**Lane:** round 10 wave 7, `interner`.
**Owned files:** `jit/src/deopt.rs`, `jit/src/ir.rs`, `jit/src/lib.rs`, new
`jit/tests/r10_interner_*.rs`, new pages under `docs/known-issues/jit/` and
`docs/feature-designs/`.
**Not permitted to build, test or probe.** Everything below was read, not
executed. The only tools this lane ran are `rustfmt --check` (a formatter) and the
two pure-text gates; §6 says exactly which and what they returned.

Two things landed in this lane's own files (§1 and the `find_deopt_point`
contract). Everything else here is a decision someone else has to make, because
making it means editing a file this lane did not own. Each proposal states the
edit, the file, and why it is not here.

**Update, 2026-09-22 (round 10 wave 9, lane `deoptretire`, which owned every
file):** §2 was decided (option B — retire), §3 was done, and §4's minimum was
done by lane `offsetkey` in wave 8. §5 is the only item still open, and it is a
request to a CI gate rather than to the compiler. Each section below carries its
outcome inline. Wave 9 also **built and ran the tests**, which none of waves 6-8
were permitted to do.

---

## 1. Done: the interned frame-state representation is retired

`FrameStateInterner` and its subsystem are deleted from `jit/src/deopt.rs`. The
argument is `docs/jit/deopt-frame-state-interning.md` §7 and the resolution
section of
`docs/internal/fixed-bugs/r10-deoptverify-frame-state-interner-has-no-production-user-RETIRED-20260922.md`.
Not repeated here; the one-line version is that the saving it was designed for has
a multiplier of 1 on both backends, and the saving that *is* available was never
measured on a compile because there was no production interner to measure.

Nothing below depends on that having been the right call — §2 and §3 are the
same decisions whichever way it had gone.

---

## 2. Decide `ir::InlineScopeTable`: land a populator, or retire it

**DECIDED 2026-09-22: option B, retire.** `InlineScope`, `InlineScopeId`,
`InlineScopeTable` and `ir_lower::Lowerer::caller_chain_for` are deleted;
`resolve_frame_state` sets `caller: None` outright and says why;
`lower_inner_with_scopes` collapsed into `lower_inner_with_array_list` (its only
extra parameter was the table, and every caller passed it empty).
`MAX_INLINE_SCOPE_DEPTH` stays — it bounds `IrInlineFrameSites`' stack-trace
walks, which are live code. The five `ir_lower` tests that built chains went with
it; what they were covering (an unresumable CALLER sinks the whole point, and the
walk is bounded) is `jit/src/deopt.rs`'s own coverage of
`frame_state_is_resumable`.

One correction to the recommendation below, made while doing it: option A was
NOT blocked only by ownership. `IrBuilder::build` does not inline, and when it
splices (`IrInlineSite`) it deliberately carries the caller's `invoke` bci with
re-execute semantics rather than a scope chain — because re-execution needs no
frame identity while a chain needs the innermost frame's, which
`resolve_frame_state` leaves for the VM to fill from the running
`CompiledMethod`, i.e. it would name the caller. The side table was the easy
half. That is now written where the table was, in `jit/src/ir.rs`.

The original text follows, unchanged.

**Owner needed for:** `jit/src/ir_lower.rs` (and `jit/src/lib.rs` phase 7, which
this lane *does* own, so only the `ir_lower.rs` half is blocked).

### The state

The READ side is wired to production:

* `ir_lower::Lowerer::caller_chain_for(index)` calls
  `InlineScopeTable::snapshot_scope`, `::chain` and `::scope`;
* `Lowerer::resolve_frame_state` fills `FrameState::caller` from it;
* `lower_inner_with_scopes` takes the table and is the real lowering entry point.

The WRITE side is not. `push_scope`, `bind_snapshot` and `bind_snapshot_range`
have no caller outside `#[cfg(test)]` anywhere in the workspace, and production
reaches the lowerer through `ir_lower::lower_inner`, which builds an **empty**
table. So `is_empty()` is true on every compile, `FrameState::caller` is `None` in
every artifact this tier installs, and the depth/parent refusals
(`MAX_INLINE_SCOPE_DEPTH`, the foreign-parent rejection) are exercised only by
their own unit tests.

A wiring-status note now sits on the `impl InlineScopeTable` block in
`jit/src/ir.rs` saying exactly that, so a reader of that file learns it there
instead of by grepping. The note is not the fix.

### Option A — land the populator

The information needed is already at hand at the splice: `jit/src/lib.rs` holds
`inline_sites: HashMap<usize, InlineSite>` keyed by the **caller pc**, and
`InlineSite` carries `class_name`/`method_name`/`descriptor`, so the caller bci
and the callee key are both available. The edit is:

1. at the splice, `push_scope(caller_key, caller_bci, caller_snapshot, parent)`;
2. `bind_snapshot_range(..)` over the contiguous run of snapshots the splice
   appends to `Graph::safepoints`;
3. call `lower_inner_with_scopes` instead of `lower_inner`.

Step 3 is in `ir_lower.rs`'s public surface and steps 1–2 are in `lib.rs`. **Do
not do this without first reading**
`docs/internal/fixed-bugs/r10-deoptverify-splice-published-point-would-carry-the-callers-locals-FIXED-20260922.md`:
a point published from inside a splice is currently miscompiled on an axis that
has nothing to do with scope tables, and landing a populator on top of that
produces well-formed-looking metadata describing the wrong frame. That page is the
prerequisite, not a footnote.

There is also a **consumer-side** prerequisite on the VM half: the resume sinks
refuse a `ReconstructedFrame` with a non-empty `caller_frames`
(`vm/src/runtime/interpreter.rs`), and the single-pass sink
`build_deopt_frame_inner` refuses an owned chain outright
(`DeoptFrameBail::InlinedChain`). So a populated table with no VM work turns
"caller chain present" into "this method's deopts all refuse", which is a
performance regression dressed as a feature.

### Option B — retire it

Delete `InlineScope`, `InlineScopeId`, `InlineScopeTable`, `MAX_INLINE_SCOPE_DEPTH`
and their tests from `ir.rs`; in `ir_lower.rs`, delete `caller_chain_for`, collapse
`lower_inner_with_scopes` into `lower_inner`, and make `resolve_frame_state` set
`caller: None` (which is what it produces today). The honest state is then "this
tier does not inline, and its metadata says so".

**Recommendation:** Option B unless the splice defect above is being fixed in the
same cycle. The read side being wired makes this table look live, and that is
worse than the interner's position was: the interner had no production reference
at all, so a `rg` told the truth about it, whereas a `rg` for
`InlineScopeTable` finds production hits and misleads. The largest cost of leaving
it is not the lines; it is that every future reader of `resolve_frame_state` has to
re-derive that the branch is dead.

---

## 3. Delete `CompiledMethod::find_deopt_point`, with its test oracle

**DONE 2026-09-22.** The function, its `debug_assert` and the three
`#[cfg(test)]` lines are gone. The replacement is exactly what this section
asked for, including the "exactly once" that an offset-keyed search could not
have asserted. A retirement note where the function was states the order of work
any future offset-keyed lookup owes first, and
`jit/tests/r10_offsetkey_deopt_point_identity.rs::find_deopt_point_stays_deleted`
is the ratchet (tightened from "no production call site", which was the
strongest claim available while the definition survived).

The original text follows, unchanged.

**Owner needed for:** `jit/src/ir_lower.rs` (three `#[cfg(test)]` lines).

Round 10 settled that the function has no production caller and should go; see
`docs/internal/fixed-bugs/r10-deoptverify-find-deopt-point-has-no-production-caller-RETIRED-20260922.md`.
What stopped the deletion in this lane is `jit/src/ir_lower.rs:31424`–`31426`:

```rust
let off = p2.native_offset;
assert!(cm.find_deopt_point(off).is_some());
assert_eq!(cm.find_deopt_point(off).unwrap().bci, 2);
assert!(cm.find_deopt_point(off + 9999).is_none());
```

Those three lines are the function's only remaining users, and they are not really
about the lookup: the test around them
(`cm.deopt_points.len() == 2`, offsets ascending, per-point stack contents) is
about the lowerer's emitted offsets. Replace them with direct assertions on
`cm.deopt_points` — that `p2.native_offset` appears exactly once, and that no
point has offset `p2.native_offset + 9999` — and the function can go, along with
its `debug_assert`. The ordering invariant it needed is kept by
`DeoptVerifier`'s structural lane, which runs on the install path in release
builds, unlike the `debug_assert`.

Note the "exactly once" in that replacement: it is a stronger assertion than
`find_deopt_point` could make, and it is the one worth having — see §4.

---

## 4. Make `native_offset` a key, or stop implying it is one

**The minimum was DONE in wave 8 by lane `offsetkey`**, which owned both files;
the stronger option is still refused, in that order, for the reason below. Wave 9
re-checked both corrected paragraphs while rewriting the same comments for §3 and
left them saying the same thing in the past tense. Record:
`docs/internal/fixed-bugs/r10-interner-native-offset-is-not-a-unique-deopt-point-key-RESOLVED-20260922.md`.

The original text follows, unchanged.

**Owner needed for:** `jit/src/osr_exit.rs`, `jit/src/x64/loop_rewrite.rs` (prose);
`jit/src/x64/deopt_stubs.rs` (if the stronger option is taken).

Filed as
`docs/internal/fixed-bugs/r10-interner-native-offset-is-not-a-unique-deopt-point-key-RESOLVED-20260922.md`.
Short version: `build_and_record_deopt_point` records `buf.pos()` and emits no
code, the recorder families de-duplicate in three maps that do not see each other,
so two adjacent metadata-only records at one pc share one offset. Two comments in
the tree state the opposite and each uses that belief to justify a different
imprecision being safe.

**Minimum:** correct those two comments. That is a prose edit in two files, and
the page says exactly what each should say instead.

**Stronger, if anyone wants an offset-keyed lookup later:** give the single-pass
recorder one identity. A `recorded_offsets: FxHashSet<u32>` next to
`deopt_points`, or a single `recorded_at: FxHashMap<(usize, DeoptReason), usize>`
replacing the three `*_box_ptr_by_bci` maps' overlapping role, would make
uniqueness a producer invariant; the verifier could then tighten
`DeoptPointsUnsorted` to `>=` (or gain a `DuplicateNativeOffset` lane) and the
lookup would become writable. **In that order.** Tightening the verifier first
refuses artifacts to prove a hypothesis, which is what the wave-6 page correctly
declined to do and what this lane also declined to do.

---

## 5. Teach `check-orphan-instruments.sh` about `&self` readers

**STILL OPEN 2026-09-22.** This is the one item of this document left, and it is
deliberately not in wave 9's scope: it is a change to a CI gate and its baseline,
not to the compiler, and three lanes now want the same teaching (see the last
paragraph). Doing it one lane's way would be the fourth partial answer.

**Owner needed for:** `scripts/check-orphan-instruments.sh`,
`scripts/baselines/orphan-instruments-allowlist.txt`, `docs/ci/orphan-instrument-gate.md`.

A small but concrete observation from this lane's subject matter, offered because
`docs/ci/orphan-instrument-gate.md` already collects this gate's blind spots (the
same-file false positive in `jit/src/lib.rs`, the absent `pub static ...Atomic...`
census).

The interner was the largest single instance of round 10's defect class — 1 119
lines of production code whose five measurement getters read zero for the
feature's entire life — and
**the gate could not see any of it.** Check C2 matches `pub fn NAME()` with zero
arguments and no `&self`, by design; every `InterningStats` getter
(`slot_sharing_ratio`, `slot_byte_saving`, `state_dedup_ratio`,
`owned_slot_bytes`, `interned_slot_bytes`) and `FrameStateInterner::stats` is an
`&self` method. So the gate's allowlist has no entry for any of them, the deletion
retires no entry, and a future re-landing of the same shape would again be
invisible.

That is not an argument for dropping the `&self` exclusion — it exists because
`&self` methods are overwhelmingly ordinary accessors, and censusing them all
would drown the gate. It is an argument for a narrower rule: **a `&self` method
with zero other arguments, returning a float or an integer, whose body reads only
fields whose names the struct also exposes through a `*Stats`/`*Census`-shaped
type, and which nothing outside its defining file calls.** That is a small enough
population to census and is exactly the shape a "measurement nobody reads" takes
on a non-atomic, per-compilation instrument — the atomic-backed variant the gate
already covers is only half the population.

Two other lanes have asked for adjacent teaching of the same gate (`#[cfg(test)]`
is not `pub`; a same-file caller in a 30 000-line file is a real caller), and all
three want the same thing: enough structural awareness to tell a definition from a
use. Worth doing once.

---

## 6. What this lane ran, exactly

* `rustfmt --check --edition 2021` on each file it edited:
  `jit/src/deopt.rs`, `jit/src/ir.rs`, `jit/src/lib.rs`,
  `jit/tests/r10_interner_deopt_point_keys.rs` — **no diff in any of them.**
  Running it on `jit/src/lib.rs` makes rustfmt follow that crate's `mod`
  declarations, and it reports pre-existing formatting drift in files this lane did
  not touch (`jit/src/aarch64.rs`, `jit/src/direct_helpers.rs`,
  `jit/src/exec_memory.rs`, `jit/src/ir_lower.rs`, `jit/src/lambda_adapter.rs`,
  `jit/src/tests.rs`, `jit/src/tiered.rs`). That drift is not this lane's and was
  left alone.
* `scripts/check-no-diag-prints.sh` and `scripts/check-orphan-instruments.sh` —
  see the commit message and the lane report for what each returned.
* **No `cargo` of any kind, and no `target/` directory was created.** Nothing in
  this lane's output should be read as a measured result; the only numbers quoted
  anywhere are line counts and things the source text says about itself.
