# Fix note — vm-lockorder-doc

## Finding
The docs review (`docs/reviews/fable-2026-06-10/docs-review.md:43`) flagged a
broken link: `docs/README.md` referenced a design note `docs/lock-order.md`
that does not exist anywhere in the tree. Round 2 (docs-fixes) repointed the
external link — `docs/README.md:35` now names
`../vm/src/runtime/lock_order.rs` as "the canonical, in-source definition of the
global lock acquisition order" — but left a follow-up (docs-fixes.md:86-89): the
source file's *own* module doc still cited the nonexistent `docs/lock-order.md`
~9 times, told readers "the two MUST stay in sync," and deferred to that file for
the "authoritative list" of enforcement status. So the canonical authority kept
pointing at a 404.

## Root cause
A docs reorg removed/never-created `docs/lock-order.md`, but
`vm/src/runtime/lock_order.rs` was written assuming a companion external doc held
the authoritative hierarchy table and the runtime-enforcement-status list. With
that file gone and the external links repointed *to this source file*, the module
became self-canonical but its prose still delegated to the missing doc.

## Exact change
Documentation / comments only — no lock logic, types, `use`s, or discriminants
changed. In `vm/src/runtime/lock_order.rs`:

1. **Module doc header rewritten to be self-referential.** It now states this
   module IS the canonical, in-source definition of the global lock acquisition
   order; that there is no separate `docs/lock-order.md`; and that `LockLevel` +
   `tracking::level_from_u8` are the single source of truth (nothing external to
   keep in sync).
2. **Added an explicit "## The global lock acquisition order" table** (L10
   `class_manager` … down to L0 `scratch`) so the documented order is present and
   accurate *in this file* rather than only implied by the scattered enum
   variant doc comments. Verified the table rows match the `LockLevel`
   discriminants exactly (Scratch=0 … ClassManager=10).
3. **Folded the old external "Runtime enforcement status" reference** into a new
   in-file `### Runtime enforcement status` subsection (wired: L6 `monitors`;
   not-wired: L10 `class_manager`, L8 `heap`).
4. **Repointed every remaining `docs/lock-order.md` / "from the doc" mention** in
   the `LockLevel` doc, the `OrderedMutex` doc, and the `#[cfg(test)]` comments to
   say "this module" / "this module's doc comment" instead. The only surviving
   literal mention of `docs/lock-order.md` is the new sentence that explicitly
   says it does not exist.

The runtime "## The rule" / "## Enforcement strategy" / examples and all the
descending-order examples were preserved (descriptions unchanged, just the stale
"per docs/lock-order.md" attributions dropped).

## Files touched
- `vm/src/runtime/lock_order.rs` (doc comments + test comments only)
- `docs/reviews/fable-2026-06-10/fixes/vm-lockorder-doc.md` (this note)

## Tests added
None. The existing `lock_level_discriminants_match_docs` /
`lock_level_ordering` tests already pin the hierarchy the doc table describes;
their comments were updated to reference the in-module table. No new test was
warranted for a doc-only change.

## Follow-up & risk
- Very low risk: comments/doc only; `cargo doc` and `cargo build` unaffected
  (rustdoc table is valid GFM; no intra-doc-link to a missing path).
- `docs/README.md:35` already points here (Round 2), so the link graph is now
  consistent end-to-end.
- Out of scope for this task (other owners): `docs/internal/{gc-tuning.md,
  embedding.md,perf-campaign-remaining.md}` and several `docs/internal/**` notes
  still link to `docs/lock-order.md`. Those internal docs should be repointed to
  `vm/src/runtime/lock_order.rs` in a separate docs pass, but they live outside
  my owned file.
