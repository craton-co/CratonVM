# Round 14 wave 3, lane compat3: proposals

Ranked. Each is an idea, not a work item, until the owner queues it.

## C3-1. A live, write-through iterator for `LinkedKeySet.reversed()` (closes `LinkedHashSet.reversed()`)

**What.** After CP2-2 the JDK's `ReverseLinkedHashSetView` runs, and its `iterator()` is
`map().sequencedKeySet().reversed().iterator()`. `native_view_reversed` answers that
`reversed()` with an `ArrayList` snapshot, so `it.remove()` -- and with it `remove(o)`,
`removeIf`, `retainAll`, `clear()` on the reversed set -- never reaches the source (HotSpot's
`LinkedKeyIterator(reversed).remove()` removes the source node). Register nothing new: make
`native_view_reversed`, for a `LinkedHashMap$LinkedKeySet` carrier, return a carrier that the
existing map-key-iterator natives serve in reverse (the node-carrying snapshot of
`itr_snapshot_nodes`, reversed, with `key_itr_remove_node` doing the removal) -- the machinery
`CRATONVM_COMPAT_ITR_REMOVE_BY_NODE` already uses for forward iterators.
**Benefit.** Closes the last observable half of `r13w8-hashcompat3` item 3 for sets.
**Cost.** Medium (one carrier kind, reversed snapshot order). **Risk.** Low-medium
(`modCount` checks must match the JDK's). **First step.** A native-collections test: remove the
first element through `lhs.reversed().iterator()` and assert the source lost its LAST element.

## Round 14 wave 4 (lane compat4): C3-1 landed

Not by a reversed snapshot iterator but the JDK's own shape: `native_view_reversed` on a native
`LinkedKeySet` / `LinkedEntrySet` carrier mints a live view carrier over the same source with its
declared `reversed` field set (`mint_reversed_linked_view`), and every native reader honours the
field (`linked_set_view_reversed`); the entry-set `ArrayList` type hole closes with it. Switch
`CRATONVM_COMPAT_LHM_REVERSED_LIVE_VIEWS` (default on). Details and the residual (reversed
`values()`) on `r13w8-hashcompat3-compatible-collection-residuals-20260928.md`, "Round 14 wave 4".

## C3-2. `LinkedHashMap.reversed()` to the JDK's `ReversedLinkedHashMapView`

**What.** The CP2-2 move for maps: stop registering the `reversed()` closure (the rebuilt
snapshot map, `build_reversed_map_snapshot` + `resync_reversed_map`) on a real JDK in
`--compatible`. The view's members call `LinkedHashMap.this` natives (`get`, `put`, `remove`,
`putFirst`/`Last`, `pollFirst`/`LastEntry`) and its `keySet()` / `values()` / `entrySet()` go
through `sequencedKeySet().reversed()` etc. (the same `native_view_reversed` route as C3-1).
**Benefit.** Writes through the view reach the source (`r13w8-hashcompat3` item 3, map half);
deletes the fingerprint / generation resync code (`reversed_fingerprint_matches`,
`__reversed_source` overlay rows) once proven. **Cost.** Medium: audit every
`ReversedLinkedHashMapView` member against the natives first (JDK 25 `LinkedHashMap.java`).
**Risk.** Medium (the map views are the busiest `--compatible` path). **First step.** The member
table, as for CP2-2, then a probe mirroring `R14Compat3LhsReversed` for maps.

## Round 14 wave 4 (lane compat4): C3-2 landed

`register_linked_hashmap_natives` leaves `LinkedHashMap.reversed()` to the JDK's
`ReversedLinkedHashMapView` on a real JDK in `--compatible` (`lhm_reversed_left_to_bytecode`;
switch `CRATONVM_COMPAT_LHM_REVERSED_REAL_VIEW`, default on, which also requires C3-1's switch).
The member table is on the switch's doc comment. The snapshot / fingerprint / resync code stays for
the synthetic JDK, the kill switches and the unmodifiable wrapper; deleting it is for when the
switches retire. Probe `C:\craton\jitr14-probes\src\R14Compat4ReversedViews.java`.

## C3-3. A per-(VM, class) slot memo for the `Random` field road

**What.** SH3-2 resolves `Random.seed` and `AtomicLong.value` by NAME on every draw
(`exact_random_seed_cell`, two `resolve_field_index_by_class_id` calls) and
`haveNextNextGaussian` / `nextNextGaussian` on every Gaussian. Memo the four slots per
(`vm_identity`, `ClassId`) the way `hs_map_slot` does (`ClassMemo`, positive answers only), then
measure `RandomShadowCost` / `RandomBench` A/B against `CRATONVM_RANDOM_REAL_SEED_FIELD=0`.
**Benefit.** Likely faster than the side table (no lock-key registry mutex, no `SEED_TABLE` write
lock), which would also justify retiring the natives for the real JDK altogether once `AtomicLong`
is JIT-intrinsic (the `CRATONVM_JDK_RANDOM` note). **Cost.** Small. **Risk.** Low.
**First step.** The A/B measurement before and after the memo.

## C3-4. Delete `SEED_TABLE` for the real JDK path; keep a synthetic field-0 road

**What.** After C3-3 is measured, the side table serves only the synthetic JDK (whose `Random` is
the two-`_f` slot synthetic) and the rare fallbacks. Store the synthetic seed as a boxed-free
`Long` in `_f0` (the module doc already notes it could) and delete `SEED_TABLE`,
`GAUSSIAN_TABLE`, `adopt_real_random_state` and the writeObject materialization, leaving
`forget_random_state_keys` to SHA1PRNG only. **Benefit.** Two process-wide tables and a
lock-key per `Random` gone (AGENTS.md: per-VM state). **Cost.** Medium (the unit tests keyed on
the table). **Risk.** Low once C3-3 is in. **First step.** Census which receivers still reach
`Lcg::Table` on a real-JDK run (a debug counter behind `CRATONVM_DBG_*`, removed after).

## C3-5. Run the whole battery in the compat arm every wave that touches `--compatible`

**What.** `w2a-compat` has no output for `R13HashcompatEqualsOrder` or
`R13Shadow5VmErrorMessagePair`, so a wave-1 fix of the second could not be confirmed and the
wave-3 triage (`r14w3-compat3-compatible-probe-triage-CLOSED-20260929.md`) had to argue from code.
**Benefit.** Every `--compatible` claim gets a measurement. **Cost.** One pmatrix arm.
**Risk.** None. **First step.** Make `pmatrix14.sh`'s compat arm take the full probe list.

## C3-6. An identity-audit gate for `create_string` producers of JDK-interned names

**What.** Wave 1's hit-or-fresh `create_string` needs every native whose HotSpot twin interns
(`StringTable::intern` in the VM, or `.intern()` in the Java body) to call `intern_string`. The
audit was by hand and missed `Class.getPackageName` / `Package.getName`
(`r14w3-compat3-lang-class-package-name-interning-patch-FIXED-20260929.md`). A small table test in
`native-builtins/tests` listing (class, method) pairs known to be interned on HotSpot
(`Class.getName`, `getPackageName`, `Field/Method.getName`, `StackTraceElement` names, ...) and
asserting their registered native's source function contains `intern_string` would make the
next miss a red test. **Benefit.** Stops silent identity regressions. **Cost.** Small.
**Risk.** Low (text-pin style, like the create_string ratchet). **First step.** The list, from
the JDK 25 sources (`rg '\.intern\(\)' java.base`) and HotSpot's `StringTable::intern` callers.
