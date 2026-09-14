# H14-2 — the six families the effort has been planning around are 14.3% of the defect, and a third of it belongs to no P-row at all

**Status: OPEN — MEASURED counts, ARGUED mapping.** Derived from `H14-1`'s
classification of all 1402 `native-shadows-bytecode` `native-won` rows. No
source change; no build. Every count below is a total, not a floor
(`saturation: none` on the arm that produced it).

Lane H14, 2026-08-20.

---

## 1. Concentration — the defect has a head, and it is smaller than the tail

MEASURED, `--fn-indent 0` (see `H14-1` §2a for why that qualifier is
load-bearing):

| axis | distinct | top 10 | top 25 | top 50 | top 100 |
|---|---:|---:|---:|---:|---:|
| **registrar function** | **149** | 481 (34.3%) | 782 (55.8%) | 1078 (76.9%) | 1329 (94.8%) |
| Java class | 252 | 301 (21.5%) | 595 (42.4%) | 873 (62.3%) | 1145 (81.7%) |
| Java package | 53 | 1139 (81.2%) | 1345 (95.9%) | 1399 (99.8%) | — |
| source file | 64 | — | — | — | — |
| crate | 4 | — | — | — | — |

The tail is real: the **median registrar owns 5 rows**, 80 of the 149 own five
or fewer (193 rows, 13.8%), and 25 own exactly one.

**Read this the way the standing trap says to.** "Top 25 = 55.8%" is a
fraction and fractions read as good news. The absolute statement is the useful
one: **clearing the 25 largest registrars leaves 620 shadows standing across
124 more registrars**, and clearing the top 50 leaves 324 across 99.

## 2. Ownership clusters — 95, and one of them is a third of the defect

Registrars transitively joined where they share a Java class (the same rule
`cluster-map.py` uses, with the same limitation — it measures REGISTRATION, not
state):

| # | rows | cum | registrars | classes | crates | lead |
|---:|---:|---:|---:|---:|---|---|
| 1 | **453** | 32.3% | **34** | 61 | builtins + collections + io | `phases_early.rs::register_real_jdk_forkjoin_essentials` |
| 2 | 64 | 36.9% | 2 | 31 | builtins | `lib.rs::register_exception_extras_natives` |
| 3 | 57 | 40.9% | 1 | 2 | builtins | `lang_string.rs::register_string_builder_natives` |
| 4 | 54 | 44.8% | 9 | 6 | builtins | `lang_invoke.rs::register_t4_method_handle_invoke` |
| 5 | 35 | 47.3% | 3 | 2 | builtins + collections | `properties_sidetable.rs::register_properties_sidetable` |
| 6 | 35 | 49.8% | 1 | 1 | collections | `lib.rs::register_tree_map_natives` |
| 7 | 31 | 52.0% | 1 | 1 | collections | `lib.rs::register_array_deque_natives` |
| 8 | 26 | 53.9% | 1 | 1 | collections | `lib.rs::register_concurrent_hashmap_natives` |
| 9 | 24 | 55.6% | 1 | 1 | builtins | `phases_late.rs::register_p64_hex_format` |
| 10 | 24 | 57.3% | 2 | 1 | builtins | `nio_file.rs::register_phase57_file` |

**94 of the 95 clusters are small and most are single-registrar,
single-class** — clusters 6 through 10 are one registrar over one class each.
That is the encouraging half: the shadow population is far more separable than
the *registration* population, where `cluster-map.py` on the same dump returns
a single blob of 6660 registrations over 605 classes and 188 sites spanning
four crates.

Cluster 1 is the discouraging half: 453 rows, 34 registrars, 61 classes, three
crates, transitively bound because several registrars each touch
`java/lang/Class`, `java/util/Arrays` or `java/lang/System`. It contains
`register_essential_natives_with_shims`. **It is not one PR and it is not one
retirement**, and any plan that treats "the top cluster" as a unit of work has
mis-sized its first step by a factor of thirty.

## 3. The six families everyone has been planning around are 14.3%

`H0-4` priced six collection prefixes and its migration order has been the
working plan since. MEASURED against the population:

| priced prefix (`H0-4`) | rows | share of 1402 |
|---|---:|---:|
| `java/util/concurrent/ConcurrentHashMap` | 50 | 3.6% |
| `java/util/HashMap` | 42 | 3.0% |
| `java/util/TreeMap` | 41 | 2.9% |
| `java/util/LinkedHashMap` | 31 | 2.2% |
| `java/util/HashSet` | 20 | 1.4% |
| `java/util/Hashtable` | 16 | 1.1% |
| **all six** | **200** | **14.3%** |
| **everything else** | **1202** | **85.7%** |

And by registrar: **135 of the 149 registrars have ZERO rows under any of the
six**, carrying 1171 rows (83.5%) between them.

`H0-4` is not wrong about anything it measured. Its six numbers are correct and
its migration order within those six stands. What it could not know, because
nobody had counted, is that **the six are a seventh of the problem** and its
"cheapest first" order starts at rank 4, 6, 7, 8, 14, 16 and 17 of the ranking.

## 4. A third of the defect is claimed by no P0/P1/P2 row

`docs/jdk-only-runtime-services.md` has 23 P-rows. Mapping each shadow onto the
row that would claim it (**ARGUED mapping by package/class, MEASURED counts**):

| the P-row that claims it | rows | share |
|---|---:|---:|
| P0 *Wholesale `Bridge` over-tagging* — all of `java/util*` except `java/util/concurrent` | 428 | 30.5% |
| P1 *NIO, files, networking* | 164 | 11.7% |
| P1 *Thread and executor semantics* / *ForkJoin* | 124 | 8.8% |
| P2 *Crypto provider completeness* | 94 | 6.7% |
| P1 *Reflection and generated accessors* | 60 | 4.3% |
| P1 *JNI and native binding* (`Unsafe`, `SharedSecrets`) | 44 | 3.1% |
| P0 *JMX real path* | 29 | 2.1% |
| P2 *Headful AWT* (**CLOSED(5)**) | 12 | 0.9% |
| P1 *JPMS / module semantics* | 2 | 0.1% |
| **— no P-row claims them —** | **445** | **31.7%** |

The unclaimed 445, broken out. (The `java/lang/String*` group is 58 in the
mapping above and 57 here: the difference is the single
`java/lang/StringUTF16.getChars` row, which the CLOSED *Core `String` dispatch*
row does name explicitly, so it is subtracted here and left in the table above
where the mechanical package rule put it.)

| unclaimed area | rows | note |
|---|---:|---|
| `java/lang` core + `java/lang/ref` | 168 | `Object`, `Class`, `Thread`, `Enum`, `System`, `Module`, `Runtime`, `StackTraceElement`, the boxed primitives, the reference types |
| `java/io` streams | 99 | `File`, `FileOutputStream`, `ByteArray*Stream`, `Data*Stream`, `PrintStream`/`PrintWriter`, `BufferedWriter` — P1 *NIO, files, networking* is written about `java.nio` and `sun.nio.ch` |
| `java/lang/StringBuilder` (45) + `StringBuffer` (12) | 57 | P1 *Core `String` dispatch* is **CLOSED** and is about `java/lang/String`; nothing claims the builders. **`java/lang/String` itself contributes ZERO rows** — that row's forced-native list is a different mechanism and does not show up as a §1.4 shadow. The one `java/lang/StringUTF16.getChars` row IS named by that row and is counted there, not here |
| `java/lang/invoke` | 56 | `MethodHandle`, `MethodHandles`, `Lookup`, `MethodType`, `VarHandle`, `MemberName` |
| `java/math` | 24 | `BigInteger` |
| `java/text` + `sun/util/calendar` | 14 | |
| other | 26 | |

**Two of these are the third and fourth largest registrars in the tree.** The
argument this settles: the P0 table was written from examples, the examples came
from where people happened to be working, and a third of the measured defect is
in areas the table does not mention.

## 5. What a retirement would clear that nobody has proposed retiring

Ranked by rows, restricted to registrars with **no** row under `H0-4`'s six
priced prefixes. `real bytecode` / `inherited` / `no impl` are `H14-1` §4's
image verdict, and they change the verb:

| # | registrar | rows | classes | real bytecode | inherited | no impl | proposed anywhere? |
|---:|---|---:|---:|---:|---:|---:|---|
| 1 | `native-builtins/lib.rs::register_essential_natives_with_shims` | 184 | 41 | 179 | 5 | 0 | no — and it is a 14k-line function, see §6 |
| 2 | `native-builtins/lang_string.rs::register_string_builder_natives` | 57 | 2 | 55 | 2 | 0 | **no** |
| 3 | `native-builtins/lang_misc.rs::register_throwable_subclass_natives` | 42 | 26 | 34 | 8 | 0 | **no** |
| 4 | `native-builtins/nio_file.rs::register_phase57_nio_file` | 34 | 6 | 32 | 0 | **2** | no; the 2 are `Path.toString`/`equals` and must NOT be retired |
| 5 | `native-collections/lib.rs::register_array_deque_natives` | 31 | 1 | 30 | 1 | 0 | dial-tested 2026-08-12 (`retired_shadow.rs` arm C2) and **found verdict-neutral by a probe that did not reach it** |
| 6 | `native-builtins/phases_late.rs::register_p71_biginteger_extras` | 24 | 1 | 24 | 0 | 0 | **no** |
| 7 | `native-builtins/phases_late.rs::register_p64_hex_format` | 24 | 1 | 24 | 0 | 0 | **no** |
| 8 | `native-builtins/net_phase_e.rs::register_uri_natives` | 24 | 1 | 24 | 0 | 0 | **no** |
| 9 | `native-io/lib.rs::register_io_natives` | 23 | 3 | 20 | 3 | 0 | **no** |
| 10 | `native-builtins/nio_file.rs::register_phase57_file` | 22 | 1 | 22 | 0 | 0 | **no** |
| 11 | `native-builtins/lib.rs::register_exception_extras_natives` | 22 | 21 | **1** | **21** | 0 | no — and retirement is the **wrong verb**: 21 of 22 are registered on a class that does not declare the method |
| 12 | `native-io/lib.rs::register_data_stream_natives` | 20 | 2 | 19 | 1 | 0 | **no** |
| 13 | `native-collections/lib.rs::register_optional_natives` | 20 | 1 | 20 | 0 | 0 | **no** |
| 14 | `native-builtins/properties_sidetable.rs::register_properties_sidetable` | 20 | 1 | 20 | 0 | 0 | named by `G88-1` §6 as a cluster, never priced |
| 15 | `native-collections/lib.rs::register_linked_list_natives` | 17 | 1 | 14 | 3 | 0 | **no** |
| 16 | `native-builtins/unsafe_natives_ext.rs::register_unsafe_natives` | 17 | 1 | 17 | 0 | 0 | **no** |
| 17 | `native-builtins/lang_reflect.rs::register_wp2_1_natives` | 17 | 7 | 13 | 4 | 0 | **no** |
| 18 | `native-collections/lib.rs::register_concurrent_completeness_natives` | 16 | 3 | 16 | 0 | 0 | **no** |
| 19 | `native-builtins/jmx.rs::register_object_name` | 16 | 1 | 16 | 0 | 0 | P0 *JMX real path*, pinned by `H0-1`, never priced |
| 20 | `native-builtins/lang_invoke.rs::register_phase54_method_handle` | 15 | 4 | 15 | 0 | 0 | **no** |
| 21 | `native-builtins/lang_invoke.rs::register_p63_method_handles_lookup` | 15 | 2 | 15 | 0 | 0 | **no** |
| 22 | `native-collections/lib.rs::register_arraylist_natives` | 14 | 1 | 12 | 2 | 0 | **partly**: 12 `ArrayList` triples are already retired, 38 rows on that class are not |
| 23 | `native-builtins/reflect_annotations.rs::register_annotation_overrides` | 14 | 7 | 12 | 2 | 0 | **no** |
| 24 | `native-builtins/shared_secrets_bridge.rs::register_java_lang_access` | 13 | 1 | 13 | 0 | 0 | **no** |
| 25 | `native-builtins/logging_shims.rs::register_printstream_fallback_natives` | 13 | 2 | 13 | 0 | 0 | **no** |

**Nineteen of the top twenty-five have never been named as a retirement
candidate anywhere in this directory.**

**`H14-3` priced thirteen of them, and the answer is better than this table
looks.** Rows 2, 3, 5, 7 and 13 above cost **zero vectors** — `StringBuilder`,
the 26 exception classes, `ArrayDeque`, `HexFormat`, `Optional`, together 174
rows and 12.4% of the whole defect. Rows 8, 9, 10 and 12 cost one vector each;
row 6 (`BigInteger`) costs 2 and row 4 (the `java.nio.file` group) costs 5.
Row 14, `java/util/Properties`, is **65 / 104** — worse than `HashMap` and the
deepest dependency measured anywhere. Row 1, the monolith, is 28 / 104.
**The ranking by rows and the ranking by cost are almost unrelated**, which is
the argument for pricing before planning.

### 5a. The positive control that says retirement works, and the one that says it is partial

MEASURED, from the same 1402:

| already-retired family | rows remaining |
|---|---:|
| `java/util/logging/*` (the 2026-08-11 wave) | **0** |
| `sun/nio/fs/*` (`H2-1`, this wave) | **0** |
| `java/util/function/*` (`H3-1` deletions) | **0** |
| `java/lang/module/*` (stateless table) | **0** |
| `java/util/ArrayList` (12 triples retired 2026-08-12) | **38** |
| `java/util/Arrays` + `Arrays$ArrayList` (1 triple retired) | 20 |
| `java/util/Collections` (1 triple retired) | 18 |

Four whole-family waves show up as exact zeros — the instrument agrees with the
work, which is the best available evidence that a shadow retirement is a real
closure and not a relabel. Two of the four zeros are witnessed by a vector that
asks — `RJdkLogging` for `java/util/logging`, and `RFileTimes` for
`sun/nio/fs/*`, which `H2-1` reports passing 68 checks with all eight retired
natives printing `[JDK-ONLY-REFUSED]`. The other two zeros are weaker in kind: a
zero can also mean the corpus never reached the family.

The three triple-by-triple entries show the other half: **retirement is
per-triple, so retiring 12 of a class's triples clears 12 rows and leaves the
class in the census.**

## 6. `register_essential_natives_with_shims` is not a work item

184 rows, 41 classes, 13.1% of the defect, one bucket — and
`native-builtins/src/lib.rs:7195`–`:21495` is **14,301 lines**. The tree already
documents the shape (`docs/architecture/natives-over-real-jdk-classes.md`
names it as the real-JDK arm's entry point; the reachability audit records it
calling *"183 in-crate sub-registrars spanning 91 modules"*). What is new is
that 184 registrations are made **directly in its own body** and are the single
largest attributable block of the strict-mode defect.

It also carries the ambient-category machinery that `H14-1` §3.5 measured:
one `registry.set_category(NativeKind::Bridge)` at `:7251`, restored at
`:21481`, is why every one of those 184 rows reports `kind_stated: false`.

**The only honest subdivision available today is by CLASS**, because the `fn`
join cannot see inside it and the nested helpers are not registrars
(`H14-1` §2a). That is what `H14-3`'s arm for it does, and its result should be
read as an upper bound on the block, not as a costed PR.

## 7. What this changes about the plan

1. **The "collection cluster" framing has been sizing a seventh of the work.**
   `H0-4`'s six prefixes are 200 rows. The whole of `native-collections` is 399.
   `native-builtins` is 898.
2. **The unit of work is the registrar, and the registrar axis is the sharpest
   of the four.** 149 buckets, 94 of 95 clusters small, most of them one
   registrar over one class. That is a work list, and it did not exist yesterday.
3. **Three verbs, not one.** 1244 rows are *retire*; 156 are *move the
   registration to the class that declares the method*; 2 are *do not touch*.
   A plan with one verb will do the wrong thing 11% of the time.
4. **Retagging is finished as a strategy, on this evidence too.**
   `HANDOFF-20260820` §0 disproved three retagging remedies by mechanism.
   `H14-1` §3.5 adds the census's own verdict: `kind_stated` is `false` on
   1402 of 1402, so there is no adjudicated sub-population and nothing for a
   retag to distinguish.
5. **Start where the dial says zero.** `H14-3`.

## 8. What this does NOT establish

* **The P-row mapping in §4 is ARGUED.** It is a package/class rule written by
  reading the row titles, not a claim any row's author would necessarily accept.
  The *counts* are measured; the assignment is a judgement, and the "no P-row
  claims them" bucket is the part worth arguing about.
* **Rows are not cost and not risk.** §5's ranking is a count. `H0-4`'s
  `HashSet` cell (20 rows, 1 vector) and `HashMap` cell (42 rows, 23 vectors)
  already show the two orders differ.
* **`native-awt` at 12 rows is a corpus artefact**, not a small AWT problem
  (`G79-1`: no AWT vector exists).
* **Nothing here measures the 168 direct Rust calls** `H4-1` §1 found bypassing
  the registry. They cannot appear in a registration-derived table, and they are
  the reason the collection cluster resisted a retag.

## 9. NOMINATIONS

* **N1 — re-cut the P0 table against §4.** Three areas holding 445 rows have no
  row at all: `java.lang` core (168), `java.io` streams (99), `java.lang.invoke`
  (56) — plus `StringBuilder`/`StringBuffer` (58) which the CLOSED *Core String
  dispatch* row does not cover.
* **N2 — retire `register_string_builder_natives` first.** 57 rows, 2 classes,
  1 registrar, 1 cluster, 55 of 57 with real bytecode behind them. It is the
  largest single-registrar single-cluster item in the tree. **MEASURED price in
  `H14-3`.**
* **N3 — `register_throwable_subclass_natives` + `register_exception_extras_natives`
  are one 64-row cluster over 31 exception classes with two different fixes** —
  34 retire, 29 are registered on classes that do not declare the method. Split
  them before either.
* **N4 — price the tail, not just the head.** 80 registrars own 5 rows or fewer.
  A batch arm over all 80 is one run and would say whether the tail is free.
* **N5 — `java/util/ArrayList` has 38 shadows left after a retirement wave that
  cleared 12.** Somebody stopped, and the record of why is not in this
  directory.
* **N6 — `java/lang/Object.<init>` is shadowed on all 104 vectors** (`H14-1`
  §6). Nothing in the P0 table mentions it.
