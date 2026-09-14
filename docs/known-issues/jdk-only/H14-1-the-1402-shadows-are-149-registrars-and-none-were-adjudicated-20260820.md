# H14-1 — the 1402 shadows are 149 registrars, 100% attributable, and not one of them was ever adjudicated

**Status: OPEN — MEASURED.** One unarmed 104-vector strict arm plus one
`--dump-native-registry --explain-jdk-only` dump, both on
`C:/craton/target-jdkonly-h2/release/cratonvm.exe` (built at `fe59bf9d9`,
39,809,024 bytes, 2026-08-20 17:32). No source change; no build. New instrument:
`regression-suite/probes/shadow-triage.py`.

Lane H14, 2026-08-20. This is the first time the `native-shadows-bytecode`
population has been bucketed by anything. `H14-2` reads the distribution;
`H14-3` prices the top of it.

---

## 0. The gap this worktree was cut across

The worktree started at `26e4b5db4` (`Merge branch
'claude/jdk-only-mode-completion-1351c0' into dev`). `git merge --ff-only
claude/jdk-only-mode-handoff-09b48c` fast-forwarded **56 commits** to
`fe59bf9d9`. What was in the gap and matters to this lane:

* `f4431a2ee` / `3adf1c624` (`H1-1`) — **the observation sink was capped at 256
  rows**. Every shadow count published before it is a floor. `943` became
  `1403`, then `1402` after `H5-1` deleted one duplicate registration.
* `82def504d` / `695a38765` / `5e1bac952` (`H0-4`, `H0-5`) — the blast-radius
  table for six collection prefixes, and the `RMapGcStress` common factor.
* `db71dfb40` (`H0-3`), `774296dcb` (`H4-1`), `d2c3c2258` (`H5-1`) — the three
  disproved P0 remedies.
* `b81aae8fc` (`H3-1`) — `stub_ratchet.rs` made to parse again.

Without the merge this lane would have measured the 256-row floor and published
a distribution of a censored sample.

## 1. The measurement, reproduced

```text
CV=C:/craton/target-jdkonly-h2/release/cratonvm.exe
JDK="C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot"
CRATONVM_ARGS=--jdk-only bash regression-suite/run.sh
  # CRATONVM_NATIVE_SHADOW_SINK_CAP=200000 was also set and was IGNORED — §1b

  REGRESSION SUITE: 104 passed, 0 failed
  JDK-ONLY CENSUS (104 of 104 per-vector reports written):
    native-shadows-bytecode, UNION: 1402 native-won, 477 bytecode-won
    synthetic-native-registered, UNION: 1627
    interpreter_shadow_unenforced, SUM: 8702
    compatibility_classes, SUM: 0
    saturation: none — the counts above are totals, not floors.
```

Identical to `HANDOFF-20260820` §1 on every figure except
`interpreter_shadow_unenforced` (**8702**, against the handoff's `8698`).
`saturation: none` on both runs, so the delta is not truncation; it is not
investigated here and is stated rather than smoothed over.

**Nothing is capped, sampled or truncated in what follows.** All 104 per-vector
reports were captured (`run.sh` writes them to a PID-scoped directory and
`rm -rf`s it at the end; they were poll-copied out while the run was live, and
the count was checked against the summary's own `104 of 104`). Headroom,
MEASURED across all 104: **the fullest observation sink held 424 rows against a
cap of 4096** (`RJdkBridge1`), and zero reports are `saturated`.

### 1b. The cap knob silently ignored the value it was given, and one sub-sink still cannot report truncation

Two things about the instrument that `H1-1`'s repair did not reach. Neither
changes any number above; both would change one later.

* **`CRATONVM_NATIVE_SHADOW_SINK_CAP=200000` was discarded without a word.**
  `vm/src/vm/vm_exec.rs::jdk_only_native_shadow_cap` filters the parsed value
  to `0 < n <= JDK_ONLY_NATIVE_SHADOW_CAP_MAX` (**65,536**) and otherwise
  `unwrap_or`s the 4096 default. This run therefore used 4096, not 200000. The
  tree *does* state the rule — the function's own doc comment says "anything
  above … leaves the default in place", deliberately, so that "a diagnostic must
  never be the thing that fails". But an operator who raises the cap *because
  they are worried about a floor* gets the floor and no signal. Use a value
  ≤ 65,536, or read `observation_sink.cap` back out of a report and check it.
* **The `jit_compile` sub-sink reports `"truncated": null, "dropped": null`.**
  `run.sh`'s saturation test is `grep -l '"truncated": true'`, and `null` can
  never match it, so an overflow there is invisible to the census line — the
  exact shape of the defect `H1-1` fixed for the other two sinks. MEASURED
  today it does not bite: the largest `jit_compile.recorded` over all 104
  reports is **2**, against a cap of 256. `jit_fastpath` is 0 everywhere.

### 1a. One instrument correction, small and real

`run.sh`'s `bytecode-won` figure of **477 is 453 distinct method triples.**
The census unions whole JSON *lines*, and 24 triples are reported twice with
two different `native_kind` spellings — `bridge` and `check-override-name` —
from two different producers:

```text
java/lang/StringBuilder.append(Ljava/lang/String;)Ljava/lang/StringBuilder;  ['bridge', 'check-override-name']
java/lang/Thread.run()V                                                      ['bridge', 'check-override-name']
java/lang/Class.getEnumConstantsShared()[Ljava/lang/Object;                  ['bridge', 'check-override-name']
```

**`native-won` is unaffected**: 1402 lines, 1402 distinct triples, no double
counting. Only the "contract working" column is inflated, by 24 out of 477
(5.0%). Not a defect worth a code change; worth knowing before anyone computes
a ratio out of the two columns.

## 2. The instrument, and the join that is the whole difficulty

A `native-shadows-bytecode` row (`types/src/error.rs`, `to_json`) carries
`class`, `method`, `descriptor`, `native_kind`, `outcome` — and **no
`registered_by`**. "Which registrar owns this defect" is therefore not a field
in the report; it is a join:

```
report row (class, method, descriptor)
  -> --dump-native-registry entry with the same triple
  -> its registered_by == "<file>:<line>"
  -> the last top-level `fn` at or above <line> in that file
```

`regression-suite/probes/shadow-triage.py` does this. Three properties worth
stating because each one had a way to go quietly wrong:

* **The dump must be verbose.** `registered_by` is passed through
  `redact_registration_site` unless the run carried `--explain-jdk-only`
  (`vm-cli/src/main.rs`: `let verbose = args.explain_jdk_only`). A redacted dump
  attributes nothing and raises no error, so the script **refuses** a registry
  whose sites carry no `/src/` path rather than publishing a large
  `no-registrar` bucket.
* **Nothing is dropped.** Rows with no matching registration are reported in an
  `UNATTRIBUTED` section. **MEASURED: that section is empty.** All 1402 join a
  registration, and all 1402 join one with `owns_slot: true` — i.e. every row in
  the population is a registration a dispatch actually reaches.
* **The join is by registrar function, not by file** (`H0-1`). A file join
  merges 399 `native-collections` rows into one bucket, because they are all in
  one 70k-line `lib.rs`.

### 2a. `cluster-map.py`'s `fn` rule admits NESTED functions, and that moved the top bucket

`cluster-map.py` accepts any `fn` indented **four columns or fewer**
(`(len(line) - len(line.lstrip())) <= 4`). Rust allows a `fn` inside a function
body and this tree uses them. Run with that rule, the largest bucket in the
entire shadow population is:

| rank under `--fn-indent 4` | rows | bucket |
|---:|---:|---|
| 1 | **74** | `native-builtins/lib.rs::url_path_or_file_field` |
| 4 | **35** | `native-builtins/lib.rs::module_pkg_arg` |
| 27 | 16 | `native-builtins/lib.rs::native_set_accessible_write_override` |

`url_path_or_file_field` is at `native-builtins/src/lib.rs:13348`, indented four
columns, **inside** `pub fn register_essential_natives_with_shims`
(`:7195`–`:21495`); `module_pkg_arg` is at `:12195`, likewise. They are local
helpers, not registrars. Every registration between such a helper and the next
one is attributed to the helper.

`shadow-triage.py` therefore defaults to `--fn-indent 0` (true top level) and
keeps `--fn-indent 4` for diffing. The effect on the population:

| join | distinct buckets | top-1 | top 10 | top 25 |
|---|---:|---|---:|---:|
| `--fn-indent 4` (cluster-map's rule) | 169 | `url_path_or_file_field` 74 | 26.5% | 48.6% |
| `--fn-indent 0` (this record) | **149** | `register_essential_natives_with_shims` 184 | **34.3%** | **55.8%** |

Both numbers are honest about something different and neither is the truth on
its own: at indent 4 the monolith is split by whatever helper precedes each
registration, which is an artefact; at indent 0 the monolith is one bucket of
184 rows, which is not a coherent registrar either — see §3.1. **Every table
below uses `--fn-indent 0`.**

## 3. The classification

### 3.1 By registrar — 149 buckets, top 25

| # | registrar | rows | cum | classes | classes (first three) |
|---:|---|---:|---:|---:|---|
| 1 | `native-builtins/lib.rs::register_essential_natives_with_shims` | 184 | 13.1% | 41 | `java/io/FileDescriptor`, `java/io/FilterInputStream`, `java/lang/Class` +38 |
| 2 | `native-builtins/lang_string.rs::register_string_builder_natives` | 57 | 17.2% | 2 | `java/lang/StringBuffer`, `java/lang/StringBuilder` |
| 3 | `native-builtins/lang_misc.rs::register_throwable_subclass_natives` | 42 | 20.2% | 26 | `java/io/IOException`, `java/lang/ArithmeticException`, … |
| 4 | `native-collections/lib.rs::register_tree_map_natives` | 35 | 22.7% | 1 | `java/util/TreeMap` |
| 5 | `native-builtins/nio_file.rs::register_phase57_nio_file` | 34 | 25.1% | 6 | `java/nio/file/Files`, `java/nio/file/Path`, `java/nio/file/Paths` +3 |
| 6 | `native-collections/lib.rs::register_array_deque_natives` | 31 | 27.3% | 1 | `java/util/ArrayDeque` |
| 7 | `native-collections/lib.rs::register_concurrent_hashmap_natives` | 26 | 29.2% | 1 | `java/util/concurrent/ConcurrentHashMap` |
| 8 | `native-collections/lib.rs::register_hashset_natives` | 24 | 30.9% | 2 | `java/util/HashSet`, `java/util/LinkedHashSet` |
| 9 | `native-builtins/phases_late.rs::register_p64_hex_format` | 24 | 32.6% | 1 | `java/util/HexFormat` |
| 10 | `native-builtins/phases_late.rs::register_p71_biginteger_extras` | 24 | 34.3% | 1 | `java/math/BigInteger` |
| 11 | `native-builtins/net_phase_e.rs::register_uri_natives` | 24 | 36.0% | 1 | `java/net/URI` |
| 12 | `native-io/lib.rs::register_io_natives` | 23 | 37.7% | 3 | `java/io/ByteArrayInputStream`, `…OutputStream`, `java/io/FileOutputStream` |
| 13 | `native-builtins/lib.rs::register_exception_extras_natives` | 22 | 39.2% | 21 | `java/io/EOFException`, `java/lang/ClassCastException`, … |
| 14 | `native-collections/lib.rs::register_chm_key_set_view_natives` | 22 | 40.8% | 1 | `java/util/concurrent/ConcurrentHashMap$KeySetView` |
| 15 | `native-builtins/nio_file.rs::register_phase57_file` | 22 | 42.4% | 1 | `java/io/File` |
| 16 | `native-collections/lib.rs::register_hashmap_natives` | 22 | 43.9% | 1 | `java/util/HashMap` |
| 17 | `native-collections/lib.rs::register_tree_set_natives` | 21 | 45.4% | 2 | `java/util/TreeMap$KeySet`, `java/util/TreeSet` |
| 18 | `native-builtins/properties_sidetable.rs::register_properties_sidetable` | 20 | 46.9% | 1 | `java/util/Properties` |
| 19 | `native-collections/lib.rs::register_optional_natives` | 20 | 48.3% | 1 | `java/util/Optional` |
| 20 | `native-io/lib.rs::register_data_stream_natives` | 20 | 49.7% | 2 | `java/io/DataInputStream`, `java/io/DataOutputStream` |
| 21 | `native-builtins/unsafe_natives_ext.rs::register_unsafe_natives` | 17 | 50.9% | 1 | `jdk/internal/misc/Unsafe` |
| 22 | `native-builtins/lang_reflect.rs::register_wp2_1_natives` | 17 | 52.1% | 7 | `java/lang/Class`, `java/lang/reflect/Field`, … |
| 23 | `native-collections/lib.rs::register_set_view_carrier_natives` | 17 | 53.4% | 7 | `java/util/HashMap$KeySet`, `java/util/HashMap$EntrySet`, … |
| 24 | `native-collections/lib.rs::register_map_view_carrier_natives` | 17 | 54.6% | 6 | `java/util/HashMap$Values`, `java/util/Hashtable$ValueCollection`, … |
| 25 | `native-collections/lib.rs::register_linked_list_natives` | 17 | 55.8% | 1 | `java/util/LinkedList` |

Ranks 26–50 (each 9–16 rows, cumulative 76.9%): `register_concurrent_completeness_natives`,
`jmx.rs::register_object_name`, `register_phase54_method_handle`,
`register_p63_method_handles_lookup`, `register_annotation_overrides`,
`register_arraylist_natives`, `register_linked_hashmap_natives`,
`register_printstream_fallback_natives`, `register_java_lang_access`,
`register_properties_natives`, `register_iterator_natives`,
`register_re7_datagram_socket`, `register_p68_ssl`,
`register_collections_extras_natives`, `register_re4_url_http`,
`register_nio_selector_real`, `register_reference_natives`,
`register_re3_inet_address`, `register_al_sublist_natives_on`,
`register_classloader_real_natives`, `register_arrays_natives`,
`register_p67_foreign_memory`, `register_t28_method_handle_completeness`,
`register_socket_channel_real`, `t27_tls.rs::register_engine_impl_natives`.

**About rank 1.** `register_essential_natives_with_shims` is a **14,301-line
function** (`:7195`–`:21495`) whose existence the tree already documents —
`docs/architecture/natives-over-real-jdk-classes.md` §115 names it as the
real-JDK arm's entry point, and the reachability audit records it calling 183
further registrars. The 184 rows attributed to it are registrations made
**directly in its own body**; anything a callee registers attributes to the
callee. It is a bucket, not a work item, and `H14-2` §5 says what to do with it.

### 3.2 By crate — four, and one of them is 64% of the defect

| crate | rows | share | distinct registrars |
|---|---:|---:|---:|
| `native-builtins` | 898 | 64.1% | 99 |
| `native-collections` | 399 | 28.5% | 32 (all in one `src/lib.rs`) |
| `native-io` | 93 | 6.6% | 15 |
| `native-awt` | 12 | 0.9% | 3 |

`native-awt` at 12 is **not** a statement about AWT. `G79-1` established the
corpus has no AWT vector; 12 rows over four classes
(`java/awt/Toolkit`, `java/awt/GraphicsEnvironment`, `java/awt/image/BufferedImage`,
`sun/java2d/HeadlessGraphicsEnvironment`) is what leaks out of a corpus that
never asks — and `RJdkAwtHeadless` only exercises the *refusal* path.

### 3.3 By Java package — 53, and the top ten are 81.2%

| package | rows | cum |
|---|---:|---:|
| `java/util` | 418 | 29.8% |
| `java/lang` | 241 | 47.0% |
| `java/io` | 99 | 54.1% |
| `java/util/concurrent` | 98 | 61.1% |
| `java/net` | 63 | 65.5% |
| `java/lang/reflect` | 60 | 69.8% |
| `java/lang/invoke` | 56 | 73.8% |
| `jdk/internal/misc` | 37 | 76.5% |
| `java/security` | 35 | 79.0% |
| `java/nio` | 32 | 81.2% |

Then `java/nio/file` 31, `java/math` 24, `javax/net/ssl` 22, `sun/nio/ch` 17,
`javax/management` 16, `sun/security/ssl` 15, `java/nio/channels` 13,
`java/lang/ref` 11, `java/lang/foreign` 10, `java/lang/management` 10,
`sun/util/calendar` 9, `javax/crypto` 8, `java/awt/image` 8,
`jdk/internal/access` 7 — and 29 more packages holding 3 rows or fewer each.

### 3.4 By Java class — 252 classes, 1265 distinct `class.method` pairs

| class | rows | | class | rows |
|---|---:|---|---|---:|
| `java/lang/StringBuilder` | 45 | | `java/nio/file/Files` | 24 |
| `java/lang/Class` | 37 | | `java/math/BigInteger` | 24 |
| `java/util/TreeMap` | 35 | | `java/lang/Thread` | 23 |
| `java/util/ArrayDeque` | 31 | | `java/util/HashMap` | 23 |
| `jdk/internal/misc/Unsafe` | 27 | | `java/util/Properties` | 22 |
| `java/lang/reflect/Field` | 27 | | `java/util/concurrent/ConcurrentHashMap$KeySetView` | 22 |
| `java/util/concurrent/ConcurrentHashMap` | 26 | | `java/util/HashSet` | 20 |
| `java/net/URI` | 25 | | `java/util/Optional` | 20 |
| `java/util/HexFormat` | 24 | | `java/util/Arrays` | 17 |
| `java/io/File` | 24 | | `java/util/Collections` | 17 |

Top 10 classes = 301 rows (21.5%); top 25 = 595 (42.4%); top 50 = 873 (62.3%).
**The class axis is the flattest of the four** — flatter than the registrar
axis, which is the argument for planning by registrar.

### 3.5 By `NativeKind` — the axis is degenerate, and that is the finding

Two columns were expected to classify the population and neither does:

| axis | values observed | rows |
|---|---|---:|
| report `native_kind` | `bridge-ran-over-bytecode` **only** | 1402 / 1402 |
| registry `kind_stated` | `false` **only** | 1402 / 1402 |

The first is by construction: `NATIVE_SHADOW_RAN_TAG` is how "the native won"
is spelled, so on a `native-won` row `native_kind` is a discriminator and not a
kind (`types/src/error.rs` says so at the field). **Retagging cannot be
prioritised from the report, because the report has no tag axis on the rows
that matter.**

The second is a measurement and it is the headline of this record.
`kind_stated` is the census column that answers *"did anyone adjudicate this
kind, or did it inherit an ambient `set_category`?"* — and it is **false for
every one of the 1402**. Not one native in the entire defect population was
given its `Bridge` classification deliberately at its own registration site.
For the 184 rows of rank 1 the responsible line is a single
`registry.set_category(NativeKind::Bridge)` at `native-builtins/src/lib.rs:7251`,
restored 14,230 lines later at `:21481`.

The tree already knows ambient inheritance is pervasive
(`jdk-only-ambient-category-audit.md` measures 6,350
ambient registrations in `native-builtins` alone). What is new is that the
**intersection with the observed defect is total**: there is no sub-population
of deliberately-classified shadows to treat differently from the rest.

## 4. The image verdict — 88.7% of the population is a plain §1.4 defect

`--dump-native-registry` schema 3+ parses the real class-path bytes for every
registered class (`image_adjudication: true` on this dump), which answers for
rows this workload never loaded — the question `real_declaring_method` cannot
answer (`G33-1`).

| image verdict for the shadowed triple | rows | share |
|---|---:|---:|
| declared on the named class, **has a `Code` attribute**, not `ACC_NATIVE` | **1244** | 88.7% |
| **not declared** on the named class — inherited from a supertype | 156 | 11.1% |
| declared, no `Code`, not `ACC_NATIVE` (abstract/interface) | 2 | 0.1% |

* **The 1244 are the defect as §1.4 describes it**: real JDK bytecode exists,
  the native ran instead, and retiring the native leaves something to run. This
  is the number the effort should be quoting, not 1402.
* **The 156 are a different defect with a different fix.** The native is
  registered on a class that does not declare the method; the JDK inherits it.
  Concentrated in `register_exception_extras_natives` (21),
  `register_hashset_natives` (11), `register_chm_key_set_view_natives` (10),
  `register_throwable_subclass_natives` (8), `keystore.rs::register_engine_surface`
  (7), `ssl_security.rs::register_p68_ssl` (7). Top classes:
  `ConcurrentHashMap$KeySetView` 10, `HashSet` 9, `LinkedHashMap` 6,
  `LinkedHashSet` 5, `SSLSocket` 5. Retirement is the wrong verb here —
  the registration is on the wrong class.
* **The 2 are `java/nio/file/Path.toString()` and `Path.equals(Object)`**, both
  from `nio_file.rs::register_phase57_nio_file`. `Path` is an interface; there
  is no bytecode behind them. **A retirement of those two removes the only
  implementation there is.** This is `H5-1` §3's abstract-receiver mechanism
  showing up in the shadow census, and it is exactly the row that would have
  been retired by a plan reading only the row count.

## 5. 162 of the 1402 are registered more than once

| registrations for the shadowed triple | triples |
|---:|---:|
| 1 | 1240 |
| 2 | 138 |
| 3 | 23 |
| 4 | 1 |

The commonest file pairs, MEASURED:

| triples | the two files that both register it |
|---:|---|
| 33 | `native-builtins/src/phases_late/nio_file.rs` + `native-io/src/lib.rs` |
| 22 | `native-builtins/src/lang_misc.rs` + `native-builtins/src/lib.rs` |
| 18 | `native-builtins/src/properties_sidetable.rs` + `native-collections/src/lib.rs` |
| 16 | `native-builtins/src/lib.rs` + `native-builtins/src/phases_late.rs` |
| 7 | `native-builtins/src/net_phase_e.rs` + `phases_late/ssl_security.rs` |
| 24 | **the same file twice** — `native-collections/src/lib.rs` (6), `lang_invoke.rs` (4), `native-io/src/lib.rs` (3), `properties_sidetable.rs` (2), `uncaught_handlers.rs` (2), `jmx.rs` (2), and five files with one each |

`H5-1` deleted one such duplicate (`FileInputStream.read([BII)I`) and the census
fell by exactly 1. **There are 162 more in the shadow population alone**, and
one of them registers the same triple at the same source LINE twice
(`java/lang/ClassLoader.registerAsParallelCapable()Z`,
`deprecated_internal.rs:1167`, two entries, both `owns_slot: false`, with
`classloader_real.rs:617` winning).

**The operational consequence, and it is a trap:** only the `owns_slot: true`
registration is reachable. A retirement aimed at a losing registration changes
nothing and measures as "no effect", which reads as "this native was not the
problem". Read `owns_slot` before touching anything. Conversely,
`CRATONVM_ENFORCE_NATIVE_SHADOW` suppresses *every* native on the armed class,
so for these 162 the dial prices strictly more than deleting one registration
does — see `H14-3` §5.

## 6. How wide is the evidence for each row

| vectors that observed the row | rows | cum |
|---|---:|---:|
| exactly 1 | **663** | 47.3% |
| 2 | 230 | 63.7% |
| 3 | 119 | 72.2% |
| 4–7 | 181 | 85.1% |
| 8 or more (excl. 104) | 188 | 98.5% |
| **all 104** | **21** | 100% |

Nearly half the population is witnessed by a single vector. That is not a
reason to discount those rows — a shadow is a shadow on one dispatch — but it
does mean **the population is corpus-shaped**, and a corpus with no AWT vector
produces a distribution with no AWT in it.

The 21 rows every vector hits are the VM's own floor:

```text
java/lang/Object.<init>                 java/lang/Class.desiredAssertionStatus
java/lang/Enum.<init>                   java/lang/Module.getDescriptor
java/lang/ClassLoader.registerAsParallelCapable
java/util/Arrays.asList  java/util/Arrays.copyOf (x2, two registrars)
java/util/ArrayList.hashCode            java/util/HashSet.hashCode / .iterator
java/util/HashMap$KeyIterator.hasNext / .next
java/util/concurrent/ConcurrentHashMap.<init> / .get / .putIfAbsent
java/util/concurrent/CopyOnWriteArrayList.add / .addAll
jdk/internal/access/SharedSecrets.getJavaLangAccess
jdk/internal/misc/Unsafe.getUnsafe / .arrayIndexScale
```

`java/lang/Object.<init>` being on that list is the sharpest single fact in the
census: **under `--jdk-only`, a native stands in front of the real
`java.lang.Object` constructor on every dispatch of every vector.**

## 7. What this does NOT establish

* **A registrar is not a state-ownership cluster.** This script measures
  REGISTRATION, exactly as `cluster-map.py` says of itself. `G88-1` §5's map/set
  cluster was found by breaking it, not by a map, and `H4-1` §1 measured 168
  direct Rust calls that bypass the registry entirely — **none of which appear
  in any table here**, because they are not registrations.
* **A count is not a cost.** §3.1's ranking is rows, not vectors, not risk.
  `H14-3` prices the top of it; the two orders are not the same order.
* **104 vectors is not the world.** §6 says how corpus-shaped this is.
* **The `bytecode-won` half is not analysed here.** 453 distinct triples where
  §1.4 already works; they are still over-tagged registrations worth deleting,
  and nobody has looked at them either.
* **`interpreter_shadow_unenforced` (8702) is a different and much larger
  number** and is not attributed by this instrument at all.

## 8. NOMINATIONS

* **N1 — quote 1244, not 1402.** The retirable population is the rows with real
  bytecode behind them. 156 are misplaced registrations and 2 have no
  implementation to fall back on. Any plan that says "retire the 1402" contains
  two instructions that would delete `java/nio/file/Path.toString()`.
* **N2 — `kind_stated: false` on 100% of the defect closes the retagging
  question.** There is no adjudicated sub-population to separate out. Combined
  with `HANDOFF-20260820` §0, the conclusion is not "retag more carefully", it
  is that **the tag was never the mechanism**.
* **N3 — fix `cluster-map.py`'s `fn` rule to `indent == 0`,** or make the
  indent a flag as `shadow-triage.py` does. Its current rule puts a local helper
  at the top of the ranking. Lane H10 owns that file; this lane did not touch
  it.
* **N4 — sweep the 162 multi-registered triples for the `H5-1` defect.** One
  deletion there was worth exactly −1 on the census; 24 of the 162 are two
  registrations in ONE file, which is the shape `H5-1` found and whose defending
  comment was false.
* **N5 — put `shadow-triage.py` behind the census.** `run.sh` deletes the
  per-vector reports it just wrote. Keeping them under a flag would make this
  classification a by-product of every strict arm instead of a lane.
* **N6 — the `interpreter_shadow_unenforced` 8702 has never been attributed to
  anything.** It is six times the size of the population this record classifies
  and no instrument in the tree buckets it.
* **N7 — make an out-of-range `CRATONVM_NATIVE_SHADOW_SINK_CAP` say so** (§1b).
  Silently substituting the default is the right *behaviour* and the wrong
  *silence*: the operator setting it is by definition the one who does not trust
  the default. One `eprintln!` on the discard path, or echo the effective cap in
  the census line, which already has the number to hand.
* **N8 — give the `jit_compile` sub-sink real `truncated`/`dropped` fields**
  (§1b). `null` is unmatchable by the census's own saturation test, so that sink
  is in the state the whole population was in before `H1-1`. It records 2 rows
  today, which is why it is a nomination and not a finding.

---

## INDEPENDENT CHECK (lane H0, 2026-08-21) — and `kind_stated` is a usable triage SIGNAL

Two of this record's structural claims verified from a different direction, on a
registry dump of the round-5 binary rather than from the per-vector reports.

**Different population, stated up front so the numbers are not misread.** This
record's 1402 is the UNION of shadows actually DISPATCHED over 104 vectors.
Mine is every registration in one tiny run whose real method is loaded, declared
and has `Code` — a static, image-side shape test, 2775 rows. The two are not the
same set and the counts should not be compared. **The structure is what
transfers.**

### 1. "`kind_stated` is `false` on all 1402" — CONFIRMED, and the contrast is the finding

| population | n | `kind_stated: false` | `kind_stated: true` |
|---|---:|---:|---:|
| shadow-shaped (real method has `Code`) | 2775 | **2504 (90.2%)** | 271 |
| legitimate bridges (real method `ACC_NATIVE`) | 206 | 1 | **205 (99.5%)** |

**A near-perfect inverse correlation, and neither this record nor any other
states it.** This record says nobody adjudicated the defect population; the
other half is that somebody adjudicated almost the entire *legitimate*
population. `kind_stated` is set by `register_with_kind` — an explicit human
decision — against an inherited ambient `set_category`.

So **`kind_stated` is not just an absence in the defect set; it is a usable
discriminator across the whole registry.** Practical consequences:

* **A cheap first-pass filter for any retirement wave**: `kind_stated: true`
  rows are the ones somebody looked at, and 205 of 206 of them are correct.
  Sorting by this puts the reviewed work at the bottom of the queue for free.
* **A ratchet worth having**: a NEW registration with `kind_stated: false`
  landing on a method the image declares with `Code` is, on this evidence, ~90%
  likely to be a defect at birth. That is a gate that could refuse the defect
  *at the moment it is written* rather than counting it a year later.
* It also explains why a retag wave cannot work by sub-setting the defect
  population on kind: there is no adjudicated sub-population inside it to
  separate out. This record already says that; the table above is the reason.

### 2. "2 rows must not be touched" — CONFIRMED against the image

```
$ javap -c java.nio.file.Path
public interface java.nio.file.Path extends java.lang.Comparable<Path>, ...
  public abstract boolean equals(java.lang.Object);
  public abstract java.lang.String toString();
```

`Path` is an **interface** and both methods are `public abstract` with **no
`Code` attribute**. So the registration is the only implementation, and a
retirement driven by row counts would delete it and leave nothing. Exactly as
this record warns.

### 3. The duplicate-registration count, reconciled rather than disputed

This record reports **162** triples registered more than once. Across the WHOLE
registry I measure **878** (of 12,138 registrations, with 1,022 rows not owning
their slot). **These agree**: 162 is the count within the 1402, 878 is the
population-wide figure. Recorded because the two numbers will otherwise look
like a contradiction to the next reader, and this directory has a standing habit
of treating a denominator mismatch as a disagreement.
