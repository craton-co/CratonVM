# Round 14 wave 4, lane compat4: proposals

Ranked. Each is an idea, not a work item, until the owner queues it.

## C4-1. A live `LinkedValues(true)` for `LinkedHashMap$LinkedValues.reversed()`

**What.** The last half of `r13w8-hashcompat3` item 3. `lhm.sequencedValues().reversed()` -- and so
`lhm.reversed().values()` since C3-2 -- is `native_view_reversed`'s reversed `ArrayList` snapshot:
right when read, but `values().remove(v)`, its iterator's `remove()` and `removeIf` never reach the
source, and its class is `ArrayList` (HotSpot: `LinkedHashMap$LinkedValues`). Mint the values
carrier as `native_lhm_values` does (`make_view_list_of` with `values_carrier_for`), set its
declared `reversed` (slot 0 on JDK 25; the list state already sits past it, `view_carrier_slots`),
and make the readers honour it: `resync_values_view` (reverse the collected pairs when the carrier
is reversed, a pure read before `collect_entries_any`), `values_view_node_at` (index `i` is node
`n - 1 - i`), `values_view_remove_jdk` (walk the nodes tail first, so the LAST equal value goes, as
`AbstractCollection.remove` over the reversed iterator does), and the values `removeIf` over nodes
(`native_al_remove_if`'s `remove_if_over_nodes` call: reverse `nodes`).
**Benefit.** Item 3 closes. **Cost.** Small-medium (four readers, one mint). **Risk.** Low-medium
(the values views are on the Spring path; every change is behind one switch). **First step.** A
probe row: `m = {a=1, b=1}; m.sequencedValues().reversed().remove(1)` removes `b` on HotSpot.

## Round 14 wave 5 (lane compat5): C4-1 landed

`native-collections/src/lib.rs`, kill switch `CRATONVM_COMPAT_LHM_REVERSED_LIVE_VALUES` (default on;
also needs `CRATONVM_COMPAT_LHM_REVERSED_LIVE_VIEWS`). `native_view_reversed` on a native
`LinkedValues` carrier over a `LinkedHashMap` answers `mint_reversed_linked_values` (a values
carrier with `reversed` set, snapshot tail first, not cached in the source's `values` field), and a
reversed one answers the source's `sequencedValues()`. Readers honouring it
(`values_view_reversed`): `resync_values_view`, `values_view_node_at` (index `n - 1 - i`),
`values_view_remove_jdk` (tail first), `native_al_remove_if`'s node walk, and `vc_route` (an
image-minted reversed `LinkedValues`). Left as it was: the by-VALUE fallbacks
(`remove_source_entry_by_value`, reached only with `CRATONVM_COMPAT_VALUES_REMOVE_JDK=0` /
`CRATONVM_COMPAT_VIEW_REMOVE_BY_NODE=0`) still remove the FIRST equal value in list order. Probe:
`C:\craton\jitr14-probes\src\R14Compat5ReversedValues.java`.

## C4-2. Retire the reversed-map snapshot machinery once C3-2's switch retires

**What.** With C3-2 on, `build_reversed_map_snapshot`, `rebuild_reversed_from`,
`resync_reversed_map[_carrying]`, `reversed_fingerprint_matches`, `lhm_source_generation`, the
three `__reversed_*` overlay rows and the `resync_reversed_map` call at the top of every
`LinkedHashMap` view native serve only the synthetic JDK, the kill switches and the unmodifiable
wrapper's `reversed()` (`UNMOD_MAP_CLASS`). Give the wrapper a real answer
(`Collections.unmodifiableSequencedMap(backing.reversed())`, JDK 21+, when the backing is a
`SequencedMap`), then delete the rest for the real JDK. **Benefit.** One overlay lookup off every
`LinkedHashMap.keySet()` / `values()` / `entrySet()` call and ~300 lines gone. **Cost.** Small.
**Risk.** Low after one release with the switch on. **First step.** A debug counter of
`resync_reversed_map` hits on a real-JDK Spring census (expected 0).

## C4-3. `Set.copyOf` / `Map.copyOf` / `List.copyOf` over a non-immutable argument: the JDK's `isEmpty` shortcut

**What.** JDK 25 `listCopy` answers `List.of()` (the shared `EMPTY_LIST`) for an empty argument and
`Set.copyOf` / `Map.copyOf` the shared `EMPTY_SET` / `EMPTY_MAP`, so `List.copyOf(new
ArrayList<>()) == List.of()` is `true` on HotSpot; the natives allocate a fresh wrapper (not read
further this wave). Check whether `native_list_of_empty` & co. already return one canonical
instance and, if so, route an empty argument there. **Benefit.** Identity parity on a common call.
**Cost.** Small. **Risk.** Low. **First step.** A probe row per family.

## C4-4. C3-4 (delete `SEED_TABLE`) -- measured plan, not contained this wave

**Why not landed.** `SEED_TABLE` is still the state of four live populations: the synthetic JDK's
`Random`, a subclass under `CRATONVM_RANDOM_SUBCLASS_YIELD=0`, an exact receiver whose constructor
built no `AtomicLong`, and **every** receiver under `CRATONVM_RANDOM_REAL_SEED_FIELD=0`, which is
SH3-2's kill switch -- deleting the table deletes the switch. It also carries the
`writeObject` hand-over and the lock-key eviction (`forget_random_state_keys`), and its unit tests
read it directly. **Plan.** (1) C3-3 first (memo the field slots per `(vm, ClassId)`, A/B
`RandomShadowCost` against `=0`); (2) a census counter behind `CRATONVM_DBG_*` of `Lcg::Table`
draws on a real-JDK run, expected 0 outside the switches; (3) retire `CRATONVM_RANDOM_REAL_SEED_FIELD`
and `CRATONVM_RANDOM_SUBCLASS_YIELD=0` together (owner decision); (4) give the synthetic `Random` its
seed in `_f0` as a `Long` and delete `SEED_TABLE`, `GAUSSIAN_TABLE`, `adopt_real_random_state` and
the write-object materialization. **Risk.** Low once (1)-(3) are in; before them it removes a kill
switch, which the round's rules forbid.
