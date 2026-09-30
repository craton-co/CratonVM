# Proposal: one VarHandle row read per interpreted access

**Status: proposal, filed 2026-10-10 by interpreter round i1 wave 46, lane
L4.** Not implemented. Carries the performance note of the closed
`docs/internal/fixed-bugs/interpreter-L4-varhandle-views-exact-behavior-and-access-mode-queries-FIXED-20261010.md`
("What would fix it", last item).

## Why

An interpreted `VarHandle` access (and a compiled one the thin helpers do
not serve: a generic site, a declined fast arm) reaches the access-mode
natives in `native-builtins/src/lang_invoke.rs`. Read from the code on
`55834015b`, `varhandle_get_mode` does, before a plain instance-field read:

1. `vh_check_arity`: `vh_meta_get` (a memo probe, `vh_memo_lookup`, else the
   table lock), to learn the kind and count the coordinates;
2. `vh_check_leading_coordinate`: nothing on the common path (a non-null
   leading argument returns at once);
3. `is_segment_var_handle`: the receiver's class name
   (`class_name_arc_of_id`) compared with `SegmentVarHandle`'s;
4. `p67_segment_vh_shape`: a second memo probe (the FFM layout table);
5. `vh_meta_get` again, for the body;
6. the view kinds, then the array test (`vh_array_call`), then the kind
   match.

`vh_check_arity` already HAS the row the body fetches again (it returns only
its `kind`). An INSTANCE or STATIC row is written only by
`alloc_instance_var_handle` / `alloc_static_var_handle` (and copied by
`varhandle_with_exactness`), none of which registers a layout shape or
mints a `SegmentVarHandle`; but `vh_check_arity`'s refusal path re-asks
both FFM predicates "even if a side-table row answered for its identity",
so whether a row and a layout shape can ever share one lock key must be
settled from `vh_lock_key` before step 2 below is safe.
`varhandle_set_mode`, the CAS, exchange, get-and-set, get-and-add and
bitwise natives have the same prologue.

## Direction

1. Let `vh_check_arity` return the `Option<Arc<VarHandleMeta>>` it read
   (one refcount bump, no copy), and pass it to the body in place of the
   second `vh_meta_get`. Nothing may be cached across a GC point: the row is
   keyed by the handle's weak lock key and holds no object reference, so it
   stays valid across one.
2. When that row is `VH_KIND_INSTANCE` or `VH_KIND_STATIC`, skip steps 3 and
   4 (they can only answer "no" for such a row). Keep the order for every
   other receiver, so FFM handles and views are decided exactly as today.
3. Keep `vh_check_arity`'s refusal path unchanged (it re-asks both FFM
   predicates before refusing).

`--compatible` and `--jdk-only` both run these natives (registered at step 1
in both modes), so the change is mode-neutral and must be byte-identical in
output.

## Cost and measurement

No new state; one fewer memo probe and one fewer class-name fetch per
instance/static access. Measure with `--nojit`, fat LTO, interleaved against
the previous landing (the handoff's "bench each wave on fat LTO" rule): the
interpreted rows of `tools/probes/interp/L4/L4W43VarHandleThinHelperBench.java`
run under `--nojit` (`get-int`, `set-int`, `cas-int`, `get-ref`). Probes that
must stay identical: `L4W31VarHandleViews`, `L4W37VarHandleExact`,
`L4W38FfmExactLayoutHandle`, `L4W45VarHandleQueries`,
`L4W46VarHandleHidingField`, both modes. If the rows do not move beyond the
host's timing floor (8-40% on layout-sensitive rows), reject it.
