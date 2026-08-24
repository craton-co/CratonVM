# Map-view iteration had FIVE doors, and fail-fast is now wired at each

**Status: FIXED — 2026-08-24.** All sixteen measured cells match HotSpot
25.0.3+9. The last open door — `TreeMap.entrySet()`, `TreeMap.values()`,
`HashMap.values()`, `LinkedHashMap.values()` — is closed by a biased stamp on
the carrier's own declared `expectedModCount`.

## What is measured

`probes/MapModCountProbe` structurally modifies a map while iterating one of
its views and reports whether `ConcurrentModificationException` is thrown.
HotSpot throws in every cell. The `values.put` column is NEW: `values()` is a
different door from `keySet()`/`entrySet()` and had never been measured, so the
table read as twelve cells when it is sixteen.

| | entrySet.put | entrySet.remove | keySet.put | values.put |
|---|---|---|---|---|
| HashMap | CME | CME | CME | CME |
| LinkedHashMap | CME | CME | CME | CME |
| TreeMap | CME | CME | CME | CME |
| Hashtable | CME | CME | CME | CME |

`CRATONVM_NO_MAP_ITERATOR_FAILFAST=1` on the SAME binary returns twelve of the
sixteen to `NONE` and leaves `Hashtable` at `CME` — which is the attribution:
every cell this work closed is gated by that switch, and `Hashtable` never
needed it because its views hand out java.base's OWN cursor.

## The door map

The thing worth keeping: "the map iterator" is five separate implementations,
and a fix at one says nothing about the others. Each row was established by
measurement (`probes/ViewShapeProbe` for the classes, `probes/ItrFieldProbe`
for the delivered iterator's own fields).

| view | carrier shape | iterator native | iterator class | fixed by |
|---|---|---|---|---|
| `HashMap` / `LinkedHashMap` `.keySet()` `.entrySet()` | HashSet-shaped, backing map | `native_hs_iterator` then `native_map_key_itr_next` | `HashMap$KeyItr`, `HashMap$EntryIterator` | `4aba0dc88` |
| `TreeMap.keySet()` | TreeSet-shaped, `ts_array_table` side-table, source stashed in the LAST capacity slot of the element array | `native_ts_iterator` then `native_snapshot_itr_next` | `TreeSet$Itr` | `c7194682e` |
| `TreeMap.entrySet()` / `.values()`, `HashMap.values()`, `LinkedHashMap.values()` | `MAP_VIEW_CARRIERS`, ArrayList-shaped | `native_al_iterator` | `TreeMap$EntryIterator`, `TreeMap$ValueIterator`, `HashMap$ValueIterator` | **this change** |
| `Hashtable.*` | its own | `real_ht_view_enumerator` | `Hashtable$Enumerator` | never broken — java.base's OWN cursor, not a snapshot |
| `ConcurrentHashMap.values()` | `CHM$ValuesView` | `native_al_iterator` | `java.util.ArrayList$Itr` | out of scope; see `VALUES_ITR_CARRIERS`, which states why CHM cannot take a snapshot carrier |

The first three all walk a SNAPSHOT taken at `iterator()` time, which is why
none of them could see a concurrent modification until each was given a
generation to watch.

## The fix, and the two attempts it is built on

`al_itr_expected_mod_count_slot` resolves `ArrayList$Itr.expectedModCount`,
which names nothing on a `TreeMap$EntryIterator`; it declines these carriers by
design. So door 3 got its own resolver, `al_view_itr_expected_slot`, using the
carrier's OWN declared `expectedModCount` — below the undeclared snapshot block
at `al_itr_alt_base`, and `int`-typed on every carrier, so writing it cannot
type-pun a reference the way the `MAP_VIEW_CARRIERS` `modCount` collision
described on `al_mod_count_slot` would.

**The encoding is the load-bearing part.** The slot stores **generation + 1**,
so `0` unambiguously means "never seeded, do not check".

That is not defensive padding, it is the fix for a measured failure. The FIRST
attempt at this door relaxed `al_itr_expected_mod_count_slot` to cover these
carriers and compared the raw value. Every measured cell went `CME` — and then
every Spring Boot class died in ~1.5 s inside JUnit discovery with a **spurious**
`ConcurrentModificationException`, because an iterator minted on a path that
never seeded reads `0`, and `0` is a perfectly legal generation. The raw value
cannot carry both meanings. With the bias, a missed seed fails OPEN.

The SECOND attempt had the right encoding and was still not shipped, because it
measured **inert**: its seed sat at the tail of `alloc_arraylist_iterator_as`,
which the named-carrier branch returns before reaching. It fired once in a whole
probe run. The seed now sits INSIDE that branch, next to the three snapshot
writes.

`view_comod_stamp` / `view_comod_is_stale` are free functions precisely so the
arithmetic that decides whether to throw is testable without a heap — the bug
above is entirely an arithmetic property, and three unit tests now pin it,
including `an_unseeded_values_view_iterator_never_reports_a_comodification`.

## The engagement control

A pass/fail cell cannot tell a working door from a dead stamp with a live
check — both read "no CME" on the arms where nothing was modified.
`CRATONVM_DBG_VIEW_COMOD=1` prints `seed`, `check`, and decisively `UNSEEDED`.
On `MapModCountProbe`: ZERO `UNSEEDED` lines, and every carrier class present.

```text
2  seed  itr=java/util/TreeMap$EntryIterator             slot=2 gen=8
1  seed  itr=java/util/TreeMap$ValueIterator             slot=2 gen=8
1  seed  itr=java/util/HashMap$ValueIterator             slot=2 gen=8
1  seed  itr=java/util/LinkedHashMap$LinkedValueIterator slot=2 gen=10
   check itr=java/util/TreeMap$EntryIterator  expected=8  actual=8   -> ok
   check itr=java/util/TreeMap$EntryIterator  expected=8  actual=9   -> CME
```

`probes/ItrFieldProbe` is the second control, and it reads the DELIVERED
iterator rather than the mint path: `expectedModCount` is now non-zero on
`TreeMap$EntryIterator`, `TreeMap$ValueIterator` and `HashMap$ValueIterator`,
where it read `0` on both earlier attempts.

## The gate, and why unit tests are not it

Enabling fail-fast is the change that surfaces latent mutate-while-iterating
bugs, so a green unit suite proves very little here: **attempt 1 passed
`cratonvm-native-collections` 136/136 and still killed every Spring Boot class.**

Run on one binary, ON vs `CRATONVM_NO_MAP_ITERATOR_FAILFAST=1`:

* `probes/MapModCountProbe` — the sixteen cells above, plus the `LIVEVIEW` rows.
* `probes/ItrFieldProbe` — the delivered iterator is actually seeded.
* `probes/MapViewBehaviourProbe` — **194 of 194 lines identical to HotSpot**,
  so view liveness, write-through removal and the entry/value discriminator all
  stand where they were.
* `cargo test -p cratonvm-native-collections` — 243 passed, 0 failed.
* Six map-heavy Spring Boot classes end to end, checking BOTH the counts and
  the absence of any `ConcurrentModificationException` in the logs:

```text
                                            ON                   OFF                  CME
BatchJdbcAutoConfigurationTests             tests=34  failed=0   --                   0
FlywayAutoConfigurationTests                tests=73  failed=0   --                   0
CacheAutoConfigurationTests                 tests=59  failed=0   tests=59  failed=0   0
JacksonAutoConfigurationTests               tests=162 failed=2   tests=162 failed=2   0
TomcatServletWebServerFactoryTests          tests=129 failed=1   tests=129 failed=1   0
ZipContentTests                             tests=29  failed=0   --                   0
```

486 tests. The three failures are `shouldRegisterProblemDetailsMixinWithJsonMapper`,
`shouldRegisterProblemDetailsMixinWithXmlMapper` and `sslWithHttp11Nio2Protocol`
— the SAME names in both arms, so they are pre-existing and untouched by this
door. Zero `ConcurrentModificationException` anywhere.

## One environment note, so it is not rediscovered

`CacheAutoConfigurationTests` cannot be launched on Windows through the ordinary
runner: its classpath is 35 266 characters, over the 32 767 command-line limit.
That is a host limit, not a VM one — substituting a drive letter for the
gradle-cache prefix brings it to 27 349 and it runs. The infra jars must then be
looked up by GROUP and ARTIFACT directory, not by a bare `junit-*.jar` filter
over the whole cache, or JUnit 4's own jar loses to `junit-jupiter-api` and the
run dies on `class not found: junit/runner/Version`.
