# Round 10 wave 8, lane `bybci` — proposals

Everything here is **read, not executed**. This lane was not permitted to build,
test, probe or run the regression suite; the only tool it ran is
`rustfmt --edition 2021 --check` on the five source files it edited, which reported
no diff. Every size estimate below is a count of call sites and declarations from
the source, never a measurement.

The lane's own change (`Compiler::may_file_by_bci`, the by-bci key-space owner
guard) is not proposed here — it landed. See
`docs/internal/fixed-bugs/r10-splice-by-bci-deopt-maps-have-no-owner-FIXED-20260922.md`'s
resolution.

---

## §1 — If anyone ever does widen the four by-bci keys, the owner is the splice INSTANCE, not the depth

**Why this section exists rather than a "deferred" note.** The widening was
proposed by the wave-7 page with the `InlineCalleeScope` DEPTH as the owner
component. That component is wrong, and it is wrong against the requirement the
page itself states one clause earlier: *"two splices of the same callee at
different pcs must not share entries"*. Both are depth 1. Since
`truncate_deopt_points_to` purges only a ROLLED-BACK splice's entries, splice A at
caller pc 10 leaves a live entry that splice B at caller pc 40 finds by
`contains_key`, skips its own filing for, and then bakes — the page's own
SUBSTITUTION, narrowed from cross-method to cross-splice and not removed.

**What would work.** The splice's own identity, which the emitter already has: the
caller pc `try_emit_inline_site(pc, site)` was entered at. That is unique per
splice within a compile, it is `Copy`, and a nested splice pushes no scope so depth
never has to be reconstructed from it.

```rust
#[derive(Copy, Clone, PartialEq, Eq, Hash)]
struct ByBciKey {
    /// `u32::MAX` for the compiling method; otherwise the CALLER pc the
    /// innermost open splice was entered at.
    splice_site: u32,
    bci: u32,
}
```

**Size, as a count of sites rather than a guess.** 10 filing sites, 8
`contains_key` tests, 6 `get` lookups in `emit_deopt_stubs`, 4 declarations and 4
`Compiler::new` initialisers in `x64.rs`, plus the stub-list tuples
(`deopt_stubs: Vec<(usize, usize, i64)>`, also `x64.rs`) which must carry
`splice_site` because `emit_deopt_stubs` resolves a stub after every scope has
popped. `truncate_deopt_points_to`'s three `retain` calls need no change: they
filter on the pointer value.

**Recommendation: do not do it, and this is the argument.** The guard that landed
makes the ambiguity harmless (substitution becomes suppression) at a cost of one
already-loaded length test per filing site, and it is exercised by a unit test.
The widening makes it impossible — a strictly stronger property — but it cannot be
exercised by anything, because no workload opens a splice with a publisher armed.
A 30-site mechanical change to a key type in the deopt-metadata path, shipped
unexercised, is the trade this lane declined. The right trigger to revisit is the
one the guard announces: a `BY-BCI FILING REFUSED` line under
`CRATONVM_DBG_EXCFRAME=1`. Until that line exists on some run, the widening is
work with no observable subject.

**Also note the ownership finding, which is not about the code.** The same change
has now been blocked twice, from opposite sides: wave 7's lane owned `x64.rs` and
not the emission files; wave 8's lane owns the emission files and not `x64.rs`.
Any wave that actually wants this must hand one lane both, or accept a two-wave
change with a non-compiling intermediate — which the ratchet in
`jit/tests/r10_bybci_publication_key_space.rs` would then need retiring in the same
commit (it asserts the keys are still bare, deliberately, so that a widening cannot
land while a test silently claims the guard is still the mechanism).

---

## §2 — Give `deopt_box_ptr_by_bci` a reason discriminator (small, and the better use of the same effort)

Filed as `r10-bybci-deopt-box-map-conflates-two-deopt-reasons-20260921.md`, and **LANDED 2026-09-22** — option *(a)*
below, with the reachability prerequisite run first and answered yes. Retired to
the internal tree as `r10-bybci-deopt-box-map-conflates-two-deopt-reasons-20260921-FIXED-20260922.md`.

Three producers file into one bare-bci map with two distinct `DeoptReason`s
(`BoundsCheck` from `emit_deopt_snapshot_at_guard` and two `op_invoke` arms,
`ReceiverTypeChanged` from sixteen), and `snapshot_pre_intrinsic_call`'s
idempotence test ignores the reason. Where they coincide, one reason is lost, and
because `osr_exit::deopt_reason_at_bci` recovers the reason **by scanning points at
a bci**, the loss converts its honest `Ambiguous` verdict into a confident
`Unique(wrong)` one — after which `recommend_action` applies the count-based policy
to what is really a `ReceiverTypeChanged` trap and never de-speculates the guard.

**Two shapes, and the cheaper one is right.**

* *(a)* key on `(bci, DeoptReason)`. Touches 2 filing sites, 3 `contains_key`
  tests, 2 `get` lookups, 1 declaration, 1 initialiser. `emit_deopt_stubs` already
  has the reason at the lookup — it is the stub tag it is matching on — so no tuple
  widening.
* *(b)* split the map, giving the sixteen receiver guards their own
  `receiver_guard_box_ptr_by_bci`. This is the shape `x64.rs`'s own comment says
  was already applied once, for the same reason, when reason 9 was separated:
  *"Sharing one map let a reason-2/6 box be handed to a reason-9 stub (and vice
  versa)."*

**Prefer (a).** It is smaller, it states the property (the key means "this bci's
snapshot for THIS reason") rather than encoding it in a map name, and it does not
add a fifth map for a future reason to be forgotten from. (b)'s only advantage is
matching an existing precedent, and that precedent exists because reason 9's key
means something genuinely different (a throw site, not a resume point) — which is
not true of 2 versus 6.

**Prerequisite, and it is the actual first task.** Establish reachability, which
this lane could not. One `CRATONVM_DBG_DEOPT` line at the end of
`emit_deopt_stubs` naming any `site_pc` that carries both a reason-2 and a reason-6
entry answers it in a single run of the 101-vector suite, and is a smaller change
than either fix. If that line never prints, (a) is a ratchet rather than a bug fix
and should be sized as one.

---

## §3 — The callee-local WIDTH source (restated, not re-argued)

Unchanged from `docs/feature-designs/jit-r10-splice-proposals.md` §1, and carried
here only so the residual is not lost when that page is next revised: a spliced
callee has an OOP source for its locals (`InlineOopScope::mask_at_cur`, the
callee's own `compute_local_oop_masks` dataflow, already trusted by the safepoint
oop maps) but no WIDTH source, because there is no `classify_local_kinds` and no
liveness for a callee. One `FrameValue::Unsupported` slot makes a whole frame
unresumable, so publishing precise `StackSlotRef`s for the reference slots alone
buys nothing. The useful unit of work is a callee-side kind/liveness analysis.

**What wave 8 adds to that:** it is not on the critical path for anything, and
should not be scheduled before §2's probe. Wave 7 made a splice-published frame
honest (callee geometry, all slots `Unsupported`, unresumable); wave 8 made the
key space it would file into unambiguous. Both are about the frame being *safe*
when it exists. Making it *resumable* is a feature, and it is gated on a consumer
that does not exist either: `build_deopt_frame_inner` still refuses a non-empty
caller chain outright (`DeoptFrameBail::InlinedChain`). A precise callee frame
delivered to a sink that refuses chains is unobservable, so the VM-side
`materialise_inlined_chain` delegation is the true predecessor, and it is a `vm/`
change no JIT lane owns.

---

## §4 — Two gate blind spots this lane hit, with the smallest thing that would fix each

`docs/ci/orphan-instrument-gate.md` records three known blind spots. This lane's
sweep of its six files hit two of them and has a concrete remedy for one.

**(i) No `pub static ... Atomic ...` census.** The gate's C1/C2 look at functions.
`jit/src/x64/bytecode_walk.rs` has three `pub static Atomic`s
(`AASTORE_SITES_WALKED`, `AASTORE_ZGC_GATE_FALLBACKS`,
`AASTORE_ZGC_GATE_SUPPRESSED`) whose only reader is
`aastore_barrier_gate_census()`, itself allowlisted as an orphan. So the gate
reports one orphan where the honest count is one getter plus three statics that are
unreadable outside a debug eprintln — and if somebody retired the getter by
deleting it, the three statics would become invisible to the gate entirely while
getting *less* readable. **Remedy:** a C4 arm — `pub static [A-Z_0-9]+: .*Atomic`
whose name has no reference in another file, excluding `**/tests/**`, identical
plumbing to C1/C2 and reusing the same `-F -f` pass. Cheap, and it fails in the
same safe direction (a false positive lands on the allowlist).

**(ii) C2 skips `&self` methods.** The gate's header defends this at length and
the defence is good — admitting `&self` triples the candidate population with
ordinary accessors. But the doc also records that this restriction "hid the round's
largest instance". **Remedy that keeps both properties:** restrict the `&self`
admission to methods whose body touches a `static` item *by name*, not merely "an
atomic". An accessor reads `self.field`; an instrument reads a module-level
`SCREAMING_CASE` static. That is one extra condition on the existing `-A 12`
body filter and does not inflate the candidate set with accessors at all.

Neither is this lane's to write: `scripts/` is not in its file set.
