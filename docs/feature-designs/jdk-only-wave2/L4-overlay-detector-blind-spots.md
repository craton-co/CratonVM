# L4 — Close the overlay detector's three blind spots

**Owns:** `vm/src/vm/vm_exec.rs`, the hunter only (~lines 3070–3200 and the
`NativeContextImpl::set_field` call site ~9150)
**Gated on:** nothing.
**Conflicts:** L11 also edits `vm_exec.rs`, in the dispatch regions (~14700,
~22700). Disjoint — but coordinate, and never `git add -A` blind.
**Effort:** M
**Evidence:** [`fabricated-object-layouts-leak-into-native-code.md`](../../known-issues/jdk-only/fabricated-object-layouts-leak-into-native-code.md)

## Goal

`CRATONVM_DBG=overlay,overlay-all` produced item 2's entire work list — 24
slots across 13 classes, from three small probes. It is the most productive
instrument this feature has. It also **misses three whole categories**, so its
output is a floor and anyone treating it as a census will declare victory early.

## The three gaps

1. **Reads are uninstrumented.** The hunter sits on
   `NativeContextImpl::set_field`. A native that *reads* slot 3 of a real
   `Properties` gets a reference where it expects an int and silently
   misbehaves; nothing reports it. Reads are arguably the more common half.
2. **Same-kind wrong-slot writes are invisible.** The trigger is
   `overlay_write_is_destructive`, a *type* mismatch. Writing an `Int` into the
   wrong `Int` slot passes silently — and that is precisely the defect this
   item's title describes ("the index still resolves and points at a different
   field").
3. **`Object(None)` over a primitive is ignored.** `overlay_write_is_destructive`
   only flags `Object(Some(_))`. `native_props_init` writes a null to
   `PROPS_FIELD_DEFAULTS`, which is `loadFactor` on a real layout, and the
   hunter says nothing.

## Design

Gap 3 is a one-line predicate change and should land first — cheap, and it will
immediately widen the census.

Gap 1: mirror the hunter onto the read path. Same cold-log shape, same
`overlay-bt` support. Expect volume; the `overlay-all` suppression pattern is
the precedent for keeping it usable.

Gap 2 is the hard one and needs a different signal, because there is no type
mismatch to detect. Options, in increasing cost:

* **Name-check at the write site.** When the receiver's class is `BootImage`
  origin and the native writes by raw index, compare the declared field *name*
  at that index against what the caller believes it is writing. Requires the
  caller to state a name — i.e. an instrumented `set_field_named_slot(idx, name)`
  used at converted sites. Only covers migrated code, but it turns each
  conversion into a permanent assertion.
* **Shadow-layout diff.** For each class we fabricate, record our intended slot
  meanings; when the real class is loaded, diff the two layouts once and report
  every disagreeing index. Catches everything, including reads, without
  per-access cost. This is the strongest option and is probably the right one.

## Steps

1. Flag `Object(None)` over a primitive descriptor. Re-run the census; expect new
   rows (at minimum `Properties` slot 3).
2. Add the read-path hunter behind the same tokens.
3. Build the shadow-layout diff: at `ensure_synthetic_class` / fabrication time
   we already know the slot count we wanted; when the real class exists, emit one
   report per class rather than per access.
4. Re-run and update item 2's table. **The number will go up.** That is the
   point — an instrument that finds nothing new after being widened was not
   widened.

## Verification

**Inject a violation and watch it fire**, for each gap, then revert:

* write `Object(None)` to a known primitive slot → must report;
* read a known mismatched slot → must report;
* write an `Int` to a wrong `Int` slot → must report (gap 2 only).

A detector that reports nothing on injected input is decoration. Two guards
shipped on 2026-08-03 that were vacuous for exactly this reason.

## Done when

All three gaps report on injected input, item 2's table is re-measured with the
widened detector, and the record says explicitly what the detector still cannot
see.
