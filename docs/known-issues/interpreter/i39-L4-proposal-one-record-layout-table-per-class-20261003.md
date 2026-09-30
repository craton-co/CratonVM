# Proposal: one record-layout table per class, built at link time, for every record door

**Status: proposal, filed 2026-10-03 by interpreter round i1 wave 39, lane
L4.** Not implemented.

## Why

A record's `hashCode`/`equals`/`toString` are answered by three doors that
each rebuild the same facts about the record's layout from different sources:

* the whole-record intrinsic (`native-builtins/src/intrinsics/record.rs`,
  `record_hash_code_at` / `record_equals_at`), reached through
  `record_object_intrinsic` and the nested-component fast path: the
  component count and first slot from `NativeContext::object_method_fast_path`
  (a class-manager read per record per hash), and each component's
  `boolean`-ness from `BOOLEAN_COMPONENTS`, a process-wide map (keyed per VM
  since wave 39) filled from `NativeContext::record_components`, which clones
  a `String` pair per component;
* the `invokedynamic` executor (`vm/src/runtime/invokedynamic.rs`
  `execute_record_object_method`), which carries its own slots and
  descriptors in `ResolvedCallSite::RecordObjectMethod`, per call site;
* the class-level recogniser (`Class::generated_record_object_methods`,
  `object_methods_args_are_canonical` since wave 39), which decides whether
  the first door may answer at all.

Wave 39 found two defects in the seams between them (the intrinsic answered
sites whose getters were not javac's; the boolean memo was keyed across
VMs), both from the same cause: the layout is a fact about the CLASS, but
it lives in three places with three lifetimes.

## Direction

1. **One table on the `Class`**, built once when the record class is linked
   (or on first use, memoised like `record_object_methods`): for each
   component, the instance slot and the descriptor's kind (int-like,
   `boolean`, `long`, `float`, `double`, reference), plus the three
   "generated body with javac's getters" bits. An `Arc<[RecordSlot]>`,
   read without the class-manager lock by the doors that already hold the
   class id.
2. **The intrinsic reads it** instead of `object_method_fast_path`'s count
   and `BOOLEAN_COMPONENTS`: the `boolean` hash comes from the table, so the
   process-wide map, its lock and its `forget_vm_*` hook go away.
3. **The javac-shape call site shares it**: a canonical
   `ResolvedCallSite::RecordObjectMethod` holds the class's table instead of
   its own `Vec`s; only a getter-driven site keeps per-site slots.
4. **The JIT** can then inline a record's `hashCode` over a table known at
   compile time (today the generated body is an `invokedynamic` the JIT
   bridges, and the intrinsic is interpreter-only).

## Expected win and cost

Removes one process global, one class-manager read per record hash, and the
`String` clones of the first `boolean` probe per class; makes the three
doors agree by construction. Cost: one table per record class (a few words
per component). Measure with the Hibernate flush planner shape the
intrinsic was written for (records keyed in `HashMap`s, millions of hashes).

## Risks

A redefinition that changes a record's components must rebuild the table
(a record's layout cannot change under redefinition today, since fields
cannot be added or removed, but the rule should be written down where the
table lives).
