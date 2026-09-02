# F34-1 — the 284 registrars that only exist in synthetic mode, what serves their classes in `--jdk-only`, and the gate that keeps the answer honest

Status: **census complete; gate written
(`native-builtins/tests/registrar_reachability.rs`); no `--jdk-only` capability
gap found at class granularity; the real exposure is DRIFT, and it is
un-gated.** Wave F, lane F34, 2026-08-13.

**Nothing was built or run.** The lane's constraints forbade `cargo`
build/check/test and forbade executing the CratonVM binary. Everything below was
measured from source, from `javap`/`java` against HotSpot JDK 25.0.3+9-LTS, and
from a Python mirror of the gate's own algorithm. §7 says exactly which claims
are verified and which are not.

Predecessors, and read them before this one:
`W7-5-registrars-that-never-shipped.md` (the census this repeats and confirms),
`W7-30-stub-ratchet-boot-path-scope.md` §7 (the shared `vm_init` model),
`W8-C15-2-option-objects-with-no-reader.md` (the `register_p64_hex_format`
promotion that is this file's negative control).

---

## 0. Two premises in the lane brief were wrong, and both mattered

The brief is quoted, then what the source says.

**Premise 1 — *"Nobody has enumerated them."*** False.
`docs/known-issues/jdk-only/W7-5-registrars-that-never-shipped.md`, dated
2026-08-12, is exactly this enumeration: 825 registrar functions across five
crates, 301 absent from the default `cratonvm-cli` build, **278 of them
reachable only via `register_synthetic_overrides`**. My independent count,
scoped to `native-builtins` alone, is **284**. The two agree; the difference is
scope and a day of drift, not disagreement.

This matters because it changes what the lane is for. The census was not
missing. What was missing is the thing a document cannot do: **fail**. W7-5 §6.3
asked for a ratchet, and what landed —
`native-builtins/tests/essential_wiring_ratchet.rs` — ratchets six named
*triples* on the `java.util.stream` abstract-terminal path. It says nothing
about the population and cannot notice a 74th family arriving. So the
deliverable stands; only its justification changes.

**Premise 2 — `register_p64_hex_format` presented as a live instance.** Stale;
it was fixed before this lane opened. Its call site is now
`lib.rs:21381`, inside `register_hex_format_real_jdk_natives`, which
`register_essential_natives_with_shims` calls at `lib.rs:19378`. The brief cited
`~lib.rs:21381` as the *defect* location; that line is the *fix*. The comment
there records why it is registered last:

> `register_p64_hex_format` is the corrected twin. Registering it LAST is
> deliberate: `register()` is last-write-wins, and until this call existed the
> phases_late family only ever won in synthetic-jdk mode, so the shipping
> default ran the broken copy.

Rather than discard it, this record uses it: `register_pe_panama` (still
synthetic-only) and `register_p64_hex_format` (promoted) are the gate's
**two-sided control**. A scanner that has stopped discriminating answers
"synthetic-only" for everything and would sail past a positive-only control.

---

## 1. The mechanism, unchanged

`register_builtins` → `register_synthetic_overrides` is
`#[cfg(feature = "synthetic-jdk")]` (`lib.rs:21600` / `:21612`), and
`synthetic-jdk` is in no crate's default feature set. A registration pass whose
only call-site chain runs through `register_synthetic_overrides` is therefore
**not in the shipping binary at all** — not "present and declined by policy" in
`--jdk-only`. Its `register(...)` calls read as coverage and are not coverage,
and any test that exercises it measures an implementation the shipping modes
never run.

---

## 2. Method

Keyed on the **argument**, not the name. A registration pass need not be called
`register_*` — F30's scanner keyed on the prefix and missed
`vm_init.rs::init_service_loader_bootstrap`. The population here is every `fn`
in `native-builtins/src` whose signature mentions `NativeMethodRegistry`:
**754 definitions, 727 distinct names**.

Call sites were resolved into a graph over the blanked source (comments and
string/char literal contents replaced by spaces, so a class name in prose is
never mistaken for code). Edges from `#[cfg(test)]` modules and `#[test]`
functions were **excluded** — a call site in a test is not a shipping call site,
and treating one as such is exactly how a test comes to cover an implementation
the shipping mode never runs.

Shipping roots are pass names referenced anywhere outside
`native-builtins/src` (excluding `tests`/`benches`/`examples`/`fuzz` trees) plus
any pass called from a non-pass function. `synthetic_only` = reachable from
`register_synthetic_overrides` minus reachable from the shipping roots.

### 2.1 The one place this census was wrong twice, and both are the same shape

Recorded because both produced a **confident, vacuous zero**, which is the
campaign's signature failure.

1. **The external-reference scan read comments.** `register_pe_panama` — the
   known live instance — was classified shipping-reachable because
   `vm/src/vm/vm_init.rs:16424` and `:16692` *discuss it in prose*. Blanking
   comments before the identifier scan fixed it.
2. **`register_builtins` is itself `#[cfg(feature = "synthetic-jdk")]`,
   `pub`, and referenced from `vm/src/native/builtins.rs`.** Taken as a
   shipping root it reaches `register_synthetic_overrides` and drags all 463
   synthetic-reachable names into the shipping set. `synthetic_only` came back
   **0**, with every floor satisfied.

Both are now defended in the gate, and the second is why
`MIN_SHIPPING` / `MIN_EXTERNAL_REFS` exist as floors rather than as prose.

---

## 3. The population

| | count |
|---|---:|
| `fn` definitions parsed in `native-builtins/src` | 18,540 |
| …taking a `NativeMethodRegistry` (distinct names) | 727 |
| reachable from `register_synthetic_overrides` | 463 |
| reachable from the shipping side | 407 |
| **synthetic-only (the closure)** | **284** |
| …of which are DIRECT children of `register_synthetic_overrides` | **73** |
| direct children that are *not* synthetic-only | 44 |

The 73 direct families cover all 284 by subtree — there is no synthetic-only
pass that is not inside one of them. That is why the gate allow-lists at two
levels (§6).

---

## 4. Triage — what serves each family's classes in `--jdk-only`

For each family: the Java classes it registers transitively; how many no
shipping-side registrar in `native-builtins`, `native-io`, `native-collections`,
`native-builtins-crypto`, `native-builtins-security` or `vm` touches
("exclusive"); whether each exclusive class exists in JDK 25.0.3+9-LTS
(`Class.forName` under boot → platform → system loader) **and whether it
declares any `native` method** (`getDeclaredMethods` + `Modifier.isNative`); and
how many `(class, name, descriptor)` triples the family shares with a shipping
pass ("drift").

### 4.1 The headline result

**Of the 127 classes exclusive to a synthetic-only family, ZERO declare a
`native` method in JDK 25.** 88 are boot-loader classes, 15 platform-loader, 24
do not exist in JDK 25 at all.

That is the answer to "is `--jdk-only` missing a capability because of this
gating", and it is **no, not at class granularity**. Every JDK class these
registrars exclusively serve has real bytecode for every method it declares, so
real-JDK mode runs the JDK's own implementation. Wiring these registrars in
would not add coverage — it would *replace* working JDK code with a partial Rust
reimplementation, which is W7-5 §3.1's point restated with a measurement behind
it.

The 24 absent classes are third-party (`org/slf4j/MDC`, `Marker`,
`MarkerFactory`, `org/apache/logging/log4j/Logger`,
`com/fasterxml/jackson/databind/node/ObjectNode`, `com/google/gson/GsonBuilder`,
`org/graalvm/nativeimage/*` ×7, `org/springframework/beans/PropertyBatchUpdateException`)
or JDK names that moved or never existed
(`jdk/incubator/concurrent/StructuredTaskScope*` — JDK 25 has
`java/util/concurrent/StructuredTaskScope`; `java/net/PlainSocketImpl`, removed
after JDK 16; `java/lang/foreign/UpcallStub`;
`com/sun/net/httpserver/HttpServerImpl`; `java/util/concurrent/SynchronousQueue$Itr`;
`java/util/logging/LogManager$LoggerEnumeration`; and `application/json`, which
is a MIME type). Nothing in `--jdk-only` can reference any of them, so their
absence is inert rather than a gap.

### 4.2 The result that is NOT benign

**2,412 distinct `(class, name, descriptor)` triples are registered by BOTH a
synthetic-only family and a shipping pass.** `register()` is last-write-wins
with no unregister API, and `register_builtins` runs
`register_essential_natives` *then* `register_synthetic_overrides` — so in
synthetic-JDK mode the synthetic-only copy wins, and in `--jdk-only` the
shipping copy wins. **For those 2,412 triples the two modes run different
code**, and every test compiled with `--features synthetic-jdk` measures the
copy that does not ship.

That is not a hypothetical: it is `register_pe_panama` / `structLayout`
generalised. Its own row is 38 of 52 triples shared with
`phases_late/foreign_ffm.rs::register_p67_foreign_memory`, which
`register_essential_natives_with_shims` calls at `lib.rs:7108`.

**This lane did not gate the drift, and it is the larger problem.** See §8.

### 4.3 The 73 families

`classes` = registered transitively; `exclusive` = not touched by any
shipping-side registrar in the five crates above; `absent` = of those, not
present in JDK 25; `drift` = triples shared with a shipping pass.

| family | defined | classes | exclusive | absent in JDK 25 | drifting triples | verdict |
|---|---|---:|---:|---:|---:|---|
| `register_aot_natives` | `aot.rs:1451` | 2 | 1 | 0 | 1/14 | REAL-JDK BYTECODE |
| `register_atomic_boolean_natives` | `util_concurrent_ext.rs:7853` | 1 | 0 | 0 | 8/8 | SHIPPING TWIN |
| `register_bigdecimal_natives` | `math_bignum.rs:3102` | 1 | 0 | 0 | 18/32 | SHIPPING TWIN |
| `register_biginteger_natives` | `math_bignum.rs:1338` | 1 | 0 | 0 | 19/32 | SHIPPING TWIN |
| `register_byte_array_output_stream` | `serialization.rs:5100` | 1 | 0 | 0 | 0/12 | SHIPPING TWIN |
| `register_cds_natives` | `cds.rs:837` | 2 | 1 | 0 | 0/11 | REAL-JDK BYTECODE |
| `register_classfile_api_natives` | `classfile_api.rs:1083` | 20 | 16 | 0 | 0/78 | REAL-JDK BYTECODE |
| `register_classloader_natives` | `classloader.rs:9448` | 13 | 0 | 0 | 26/52 | SHIPPING TWIN |
| `register_completable_future_natives` | `util_concurrent_ext.rs:5020` | 3 | 1 | 0 | 0/23 | REAL-JDK BYTECODE |
| `register_concurrent_extras` | `concurrent_extras.rs:805` | 2 | 0 | 0 | 2/15 | SHIPPING TWIN |
| `register_crypto_impl_natives` | `crypto_impl.rs:1335` | 1 | 0 | 0 | 2/2 | SHIPPING TWIN |
| `register_enterprise_final_natives` | `lib.rs:42177` | 84 | 3 | 1 | 122/232 | APP/ABSENT |
| `register_enterprise_natives` | `lib.rs:39013` | 2 | 0 | 0 | 0/8 | SHIPPING TWIN |
| `register_enum_natives` | `lang_misc.rs:1895` | 1 | 0 | 0 | 4/8 | SHIPPING TWIN |
| `register_functional_completion_natives` | `lib.rs:39789` | 1 | 0 | 0 | 0/8 | SHIPPING TWIN |
| `register_functional_extras_natives` | `lib.rs:39005` | 0 | 0 | 0 | 0/0 | TOMBSTONE |
| `register_graalvm_compat_natives` | `graalvm_compat.rs:1381` | 8 | 8 | 8 | 0/15 | APP/ABSENT |
| `register_http2_natives` | `http2.rs:2988` | 21 | 2 | 1 | 38/90 | APP/ABSENT |
| `register_jackson_gson_natives` | `phases_late/xml_json.rs:3268` | 5 | 2 | 2 | 0/38 | APP/ABSENT |
| `register_java_lang_extras_natives` | `lib.rs:39486` | 12 | 0 | 0 | 3/14 | SHIPPING TWIN |
| `register_jdk25_concurrency_natives` | `jdk25_concurrency.rs:2046` | 0 | 0 | 0 | 0/64 | SHIPPING TWIN |
| `register_jdk25_language_natives` | `jdk25_language.rs:368` | 3 | 0 | 0 | 0/9 | SHIPPING TWIN |
| `register_jdk25_patterns_natives` | `jdk25_patterns.rs:541` | 2 | 0 | 0 | 0/15 | SHIPPING TWIN |
| `register_letsgo_compat_natives` | `letsgo_compat.rs:29` | 3 | 0 | 0 | 0/4 | SHIPPING TWIN |
| `register_locale_natives` | `lib.rs:36810` | 0 | 0 | 0 | 0/0 | TOMBSTONE |
| `register_logging_natives` | `logging_shims.rs:1790` | 3 | 0 | 0 | 20/30 | SHIPPING TWIN |
| `register_m18_concurrent_fixes` | `util_concurrent_ext.rs:1733` | 4 | 0 | 0 | 2/41 | SHIPPING TWIN |
| `register_number_format_natives` | `lib.rs:35928` | 0 | 0 | 0 | 0/0 | TOMBSTONE |
| `register_pe_panama` | `panama.rs:293` | 13 | 2 | 1 | 38/52 | APP/ABSENT |
| `register_phase50_natives` | `phases_early.rs:3377` | 10 | 0 | 0 | 44/129 | SHIPPING TWIN |
| `register_phase51_natives` | `phases_early.rs:6662` | 17 | 0 | 0 | 76/173 | SHIPPING TWIN |
| `register_phase52_natives` | `phases_early.rs:11385` | 56 | 7 | 0 | 61/181 | REAL-JDK BYTECODE |
| `register_phase53_natives` | `phases_early.rs:14368` | 35 | 0 | 0 | 112/152 | SHIPPING TWIN |
| `register_phase54_natives` | `phases_early.rs:19481` | 41 | 0 | 0 | 149/200 | SHIPPING TWIN |
| `register_phase55_natives` | `phases_late.rs:262` | 17 | 1 | 0 | 32/90 | REAL-JDK BYTECODE |
| `register_phase56_natives` | `phases_late/streams.rs:24` | 46 | 0 | 0 | 42/139 | SHIPPING TWIN |
| `register_phase57_natives` | `phases_late/nio_file.rs:18` | 133 | 0 | 0 | 241/306 | SHIPPING TWIN |
| `register_phase58_natives` | `phases_late.rs:1692` | 27 | 3 | 1 | 25/170 | APP/ABSENT |
| `register_phase59_natives` | `phases_late.rs:1896` | 65 | 0 | 0 | 149/206 | SHIPPING TWIN |
| `register_phase60_natives` | `phases_late.rs:2044` | 22 | 0 | 0 | 46/79 | SHIPPING TWIN |
| `register_phase61_natives` | `phases_late.rs:2921` | 31 | 2 | 1 | 45/121 | APP/ABSENT |
| `register_phase62_natives` | `phases_late.rs:3883` | 24 | 1 | 0 | 29/109 | REAL-JDK BYTECODE |
| `register_phase63_natives` | `phases_late.rs:3933` | 21 | 1 | 0 | 47/68 | REAL-JDK BYTECODE |
| `register_phase64_natives` | `phases_late.rs:4160` | 13 | 2 | 0 | 26/82 | REAL-JDK BYTECODE |
| `register_phase65_natives` | `phases_late.rs:5649` | 21 | 1 | 0 | 15/81 | REAL-JDK BYTECODE |
| `register_phase66_natives` | `phases_late.rs:5746` | 15 | 1 | 0 | 25/71 | REAL-JDK BYTECODE |
| `register_phase67_natives` | `phases_late.rs:5795` | 67 | 6 | 3 | 158/219 | APP/ABSENT |
| `register_phase68_natives` | `phases_late.rs:6621` | 70 | 14 | 0 | 290/372 | REAL-JDK BYTECODE |
| `register_phase69_natives` | `phases_late.rs:6701` | 29 | 4 | 1 | 5/70 | APP/ABSENT |
| `register_phase70_natives` | `phases_late.rs:7463` | 24 | 0 | 0 | 22/77 | SHIPPING TWIN |
| `register_phase71_natives` | `phases_late.rs:7515` | 20 | 2 | 0 | 74/171 | REAL-JDK BYTECODE |
| `register_phase72_natives` | `phases_late.rs:9408` | 33 | 7 | 1 | 92/155 | APP/ABSENT |
| `register_phase_d_natives` | `lib.rs:42988` | 6 | 1 | 0 | 0/24 | REAL-JDK BYTECODE |
| `register_quarkus_arc_natives` | `quarkus_arc.rs:389` | 0 | 0 | 0 | 0/7 | SHIPPING TWIN |
| `register_s1_classloading` | `servlet.rs:1668` | 5 | 0 | 0 | 17/22 | SHIPPING TWIN |
| `register_s2_nio` | `servlet.rs:4877` | 13 | 0 | 0 | 130/164 | SHIPPING TWIN |
| `register_s3_http_client` | `servlet.rs:8011` | 2 | 0 | 0 | 4/4 | SHIPPING TWIN |
| `register_security_natives` | `lib.rs:37094` | 4 | 1 | 0 | 38/41 | REAL-JDK BYTECODE |
| `register_serialization_natives` | `serialization.rs:4982` | 21 | 10 | 0 | 2/95 | REAL-JDK BYTECODE |
| `register_slf4j_natives` | `logging_shims.rs:2680` | 25 | 4 | 4 | 48/102 | APP/ABSENT |
| `register_t25_natives` | `util_time.rs:5745` | 11 | 1 | 0 | 1/29 | REAL-JDK BYTECODE |
| `register_t310_scripting` | `t3_impl.rs:1057` | 6 | 2 | 0 | 2/9 | REAL-JDK BYTECODE |
| `register_t311_i18n` | `t3_impl.rs:1456` | 3 | 0 | 0 | 1/2 | SHIPPING TWIN |
| `register_t312_tooling` | `t3_impl.rs:1555` | 13 | 10 | 0 | 0/15 | REAL-JDK BYTECODE |
| `register_t31_concurrent_extras` | `util_concurrent_ext.rs:2798` | 4 | 2 | 0 | 2/28 | REAL-JDK BYTECODE |
| `register_t31_structured_concurrency` | `t3_impl.rs:1831` | 0 | 0 | 0 | 0/0 | TOMBSTONE |
| `register_t38_jndi` | `t3_impl.rs:30` | 4 | 2 | 0 | 7/16 | REAL-JDK BYTECODE |
| `register_t39_stax` | `t3_impl.rs:517` | 8 | 6 | 0 | 11/26 | REAL-JDK BYTECODE |
| `register_time_extras_natives` | `util_time.rs:2090` | 9 | 1 | 0 | 1/115 | REAL-JDK BYTECODE |
| `register_time_natives` | `util_time.rs:226` | 4 | 0 | 0 | 16/89 | SHIPPING TWIN |
| `register_tls_natives` | `tls.rs:3491` | 20 | 2 | 0 | 53/113 | REAL-JDK BYTECODE |
| `register_unsafe_define_class` | `unsafe_natives.rs:1374` | 2 | 0 | 0 | 12/13 | SHIPPING TWIN |
| `register_vector_api_natives` | `vector_api.rs:1871` | 0 | 0 | 0 | 0/136 | SHIPPING TWIN |

### 4.4 Verdict totals

| verdict | families | meaning for `--jdk-only` |
|---|---:|---|
| **SHIPPING TWIN** | 34 | A different registrar serves every class in the shipping mode. Correct to gate; the drift in §4.2 is the residual risk. |
| **REAL-JDK BYTECODE** | 24 | Has exclusive classes; all exist in JDK 25 and declare no `native` method, so JDK bytecode serves them. Correct to gate. |
| **APP/ABSENT** | 11 | Some exclusive classes do not exist in JDK 25 (third-party or renamed). Nothing can reference them; inert. |
| **TOMBSTONE** | 4 | Registers nothing. |

A verdict here is a claim about **reachability, not correctness**. It says the
shipping mode is not missing a capability *because of this registrar's gating*.
It does not say the registrar is right.

### 4.5 Two families worth reading individually

**`register_t31_structured_concurrency`** (`t3_impl.rs:1831`) is a TOMBSTONE in
the strong sense — a `pub(crate) fn` whose entire body is a comment:

> T16.4: StructuredTaskScope / Subtask / ShutdownOnSuccess / ShutdownOnFailure
> natives are owned by `jdk25_concurrency::register_jdk25_concurrency_natives`
> … This function intentionally does not register competing stubs — registering
> them here caused the canonical 8-field layout to be overridden with the
> earlier 2-field stubs, silently breaking `close()`, `result()`, and
> `throwIfFailed()` contracts.

Its named owner, `register_jdk25_concurrency_natives`, is itself synthetic-only
— and says so, in a doc comment that already reasons per mode:

> This registrar is synthetic-only (`lib.rs:24110`, inside
> `register_synthetic_overrides`), and that is the right scope rather than a
> limitation: in real-JDK mode `vm_exec.rs::thread_start`'s `eetop` witness
> refuses the fabricated read outright…

That is the shape the allow-list asks every new row to imitate, and it is
evidence the discipline already exists in the tree — it was just never
enforced.

**`register_slf4j_natives`** (`logging_shims.rs:2680`) is the clean
"two implementations, one per mode" case. `register_slf4j_binder_stubs_pub`
(`logging_shims.rs:2050`) is on the shipping path — `vm_init.rs:2605` and
`:3273`, and it is the last entry of `VM_INIT_SEQUENCE`. The synthetic-only twin
shares **48 of its 102** triples with the shipping side and adds
`org/slf4j/MDC`, `Marker`, `MarkerFactory` and `org/apache/logging/log4j/Logger`
on top. In `--jdk-only` the binder stubs win and those four classes are
unserved; since none exists in the JDK, only an application that bundles slf4j
notices, and such an application supplies its own bytecode.

---

## 5. What is NOT a defect, stated so nobody re-derives it

* **`register_pe_panama` is not fixable by deleting its call.** The obvious
  one-liner — drop `register_pe_panama(registry);` at `lib.rs:24180` so both
  modes run `foreign_ffm`'s corrected twin — would take **14 triples with it**
  that `foreign_ffm` does not register (52 registered, 38 shared). Deleting them
  removes a capability from synthetic-JDK mode with no fallback, which is the
  `flag≠mode` error in the other direction. The fix needs a triple-level diff
  this lane did not take. **No nomination.**
* **`sun/nio/ch/IOUtil`, `sun/nio/ch/FileDispatcherImpl`,
  `sun/nio/fs/UnixNativeDispatcher` and `java/net/PlainSocketImpl` are served.**
  They surfaced as "exclusive" in a first pass that only looked at
  `native-builtins`; all four are registered by `native-io`, which
  `vm_init` calls via `register_io_natives`. Any census scoped to one crate
  will manufacture this false positive.

---

## 6. The gate — `native-builtins/tests/registrar_reachability.rs`

A source witness reading the working tree through `env!("CARGO_MANIFEST_DIR")`,
**not** `include_str!` (which bakes a compile-time snapshot — the same frozen
model the gate exists to prevent). No VM boot; a `#[cfg(feature = ...)]` test
can only guard the configuration it is compiled into, and the whole defect is
that one configuration is invisible from the other.

Four tests:

1. **`the_scanner_is_not_vacuous`** — eight measured floors (file counts, `fn`
   count, pass count, direct-child count, shipping-set size, external-reference
   count, and the byte size of the extracted `register_synthetic_overrides`
   body) plus the two-sided control: `register_pe_panama` MUST be synthetic-only,
   `register_p64_hex_format` MUST NOT be.
2. **`the_allow_list_is_well_formed`** — no duplicates, every family is in the
   closure, every reason is ≥40 characters so a row cannot be added with an
   empty justification.
3. **`no_new_synthetic_only_family`** — the synthetic-only direct children must
   equal the 73-entry `DELIBERATE_SYNTHETIC_ONLY_FAMILIES` **exactly**. Its
   failure message asks the three-way per-mode question rather than inviting an
   allow-list entry.
4. **`no_registrar_silently_orphaned_into_the_synthetic_arm`** — the full
   284-name transitive closure must match **exactly**. This is the one test 3
   cannot make: a pass that loses its shipping call site usually sits inside an
   already-allow-listed family's subtree and inherits the exemption.
   `register_p64_hex_format` lives under `register_phase64_natives`, which is
   allow-listed — so only an exact set over the closure catches its regression.

Both lists **ratchet in both directions**. A name leaving is good news
(something was promoted) and still fails, because a stale exemption is a hole.

---

## 7. Mutation table — and the two mutants that were themselves wrong

`cargo` was forbidden to this lane, so the gate's **algorithm** was mutation-checked
through a Python mirror that reads `DELIBERATE_SYNTHETIC_ONLY_FAMILIES`,
`SYNTHETIC_ONLY_CLOSURE`, the floors and the control names **out of the `.rs`
file itself**, so reference and gate cannot drift. Mutations are applied
in-memory; the working tree was never perturbed.

| # | mutation | expected | observed | tests that fired |
|---|---|---|---|---|
| — | **control** (pristine tree) | green | **green** | none; 171/553 files, 18,540 fns, 727 passes, 73 direct, 284 closure, 407 shipping |
| M1 | a registrar moved into `register_synthetic_overrides` (shipping call site deleted, call added inside) | fail | **fail** | `vacuity` (negative control), `families` (unlisted), `closure` (entered) |
| M2 | one family removed from the allow-list | fail | **fail** | `families` — `unlisted: [register_graalvm_compat_natives]` |
| M3 | a pass whose name does **not** start with `register_` (`wire_up_layout_extras`), called only from `register_synthetic_overrides` | fail | **fail** | `families`, `closure` — both name it. This is the F30 hole, closed. |
| M4a | parser drift: the keying type renamed away (`NativeMethodRegistry` → `NativeRegistry`) | fail | **fail** | `vacuity` (`passes=0`, `shipping=0`, `external=0`, positive control), `families`, `closure` |
| M4b | locator drift: `register_synthetic_overrides` no longer a column-0 `fn` | fail | **fail** | `vacuity` only — `synthetic_overrides_body=0`. **`families` and `closure` stayed green**, which is precisely why the body-size floor exists. |
| M4c | a comment quoting the signature verbatim above `register_builtins` (the F30 blindness, as a defence check) | **green** | **green** | none — comments are blanked before the locator runs |
| M5 | the W8-C15-2 promotion reverted (`register_p64_hex_format` call deleted) | fail | **fail** | `vacuity` (negative control), `closure` (entered) |
| M6 | `register_pe_panama` given a shipping call site | fail | **fail** | `vacuity` (positive control, `direct=72`), `families` (stale), `closure` (7 names left, the whole `pe_*` subtree) |

**M4a and M4b were miswritten on their first run and both reported the gate
green.** M4a renamed the type to `NativeMethodRegistry`**`V2`**, which still
satisfies `sig.contains("NativeMethodRegistry")` — a no-op mutant. M4b doubled a
space after `pub`, which the header parser skips — also a no-op. Neither was a
gate weakness; both were the probe's setup being code that can be wrong. They
are recorded because a mutation table whose mutants do not mutate is the same
vacuous green the gate exists to prevent, and it took a second look to see it.

---

## 8. What this lane did NOT do

1. **The drift in §4.2 is un-gated.** 2,412 triples where the mode decides which
   implementation runs is a bigger exposure than the reachability question this
   file closes, and it is the exposure that actually produced the
   `structLayout` bug. Gating it needs a triple-level census that resolves
   descriptors built with `format!` — W7-5 §0 records a 60% over-statement from
   exactly that blind spot, so a literal-only comparison must not be trusted.
2. **Triage is at CLASS granularity, not METHOD.** "No capability gap" means no
   *class* exclusively served by a synthetic-only family declares a `native`
   method. A family whose class is also registered by a shipping pass, but where
   the shipping pass registers *fewer methods*, would not appear. That gap is a
   subset of item 1.
3. **Scope is `native-builtins` only.** W7-5 counted 301 across five crates
   against my 284 in one; ~17 synthetic-only registrars live in
   `native-collections`, `native-io`, `native-awt` and `vm/src`. The gate does
   not see them.
4. **The Rust gate has not been compiled or run.** See §9.
5. **`register_vector_api_natives`, `register_quarkus_arc_natives`,
   `register_locale_natives`, `register_number_format_natives` and
   `register_functional_extras_natives` report 0 classes** because their class
   names are built dynamically or come from `const` bindings the class extractor
   does not resolve. Their triple counts (136, 7, 0, 0, 0) come from the
   descriptor-level extractor, which does resolve `const`s. Their verdicts rest
   on the triple data, not the class data.

---

## 9. Verified vs assumed

**Verified.**

* The 284/73 enumeration, the call graph, and every count in §3 — computed twice
  by independent scripts that agree, and a third time by the gate's Python
  mirror (284, 73, 407, 61, body 104,359 bytes).
* Every JDK-presence and `native`-method answer in §4 — `java` against
  HotSpot 25.0.3+9-LTS on this host, `Class.forName` + `Modifier.isNative`.
* `register_p64_hex_format`'s promotion, and that `lib.rs:19378` sits inside
  `register_essential_natives_with_shims` — read directly.
* `register_p67_foreign_memory` having both a shipping and a synthetic-only
  caller — read directly.
* The two brief premises in §0 — checked against W7-5 and against the source.
* The mutation table in §7 — every row run.
* The gate file parses and is `rustfmt`-clean — `rustfmt --check` on a copy,
  and a token-level diff confirming formatting changed nothing else.
* **One bug in the gate's own header parser, found by reading it after the
  mutation table was already green, and fixed.** The `extern "C" fn` branch
  consumed the ABI string with a "skip alphanumerics" loop; on blanked source
  that renders as `extern " " `, so the loop walked straight through the `fn`
  that follows and every `extern` definition was dropped from the population.
  Measured impact on this census: **none** — `native-builtins/src` has 6
  `extern "..." fn` definitions and **0** of them takes a `NativeMethodRegistry`
  — so it was a latent hole, not a wrong number. It is recorded because the
  mutation table did not catch it: none of the nine mutants introduced an
  `extern` registrar, and a green mutation table is not a proof of a correct
  parser.

> **VERIFIED AGAINST A BINARY 2026-09-02.** "Assumed / not verified" listed
> **"That `registrar_reachability.rs` compiles and passes. It was never given to
> `cargo` ... Run it before treating this gate as live."** It has now been run,
> on a build from this tree:
>
> ```text
> cargo test -p cratonvm-native-builtins --test registrar_reachability   5 passed, 0 failed
> cargo test -p cratonvm-native-builtins --test essential_wiring_ratchet 5 passed, 0 failed
> ```
>
> It compiles and passes, so **the gate is live** and the mirror held: no type
> error, no borrow error, no mis-remembered `std` API. The floors in §6 are
> satisfied as written — the instruction was to re-take them rather than relax
> them if the numbers moved, and they did not move.
>
> Two things this does NOT settle, both still on the same list: the runtime cost
> is still "estimated seconds, not measured", and the shipping-root definition is
> still unchecked against `vm_init`'s real-JDK arm. Running a gate proves it runs.

**Assumed / not verified.**

* **That `registrar_reachability.rs` compiles and passes.** It was never given
  to `cargo`. Its logic is mirrored by the Python reference that produced §7,
  but a mirror is not a compiler: a type error, a borrow error, or a `std` API
  I mis-remembered would show up only on the first `cargo test -p
  cratonvm-native-builtins --test registrar_reachability`. **Run it before
  treating this gate as live.** If the numbers move, the floors in §6 are the
  values to re-take, not to relax.
* Its runtime cost. It reads ~78 MB of Rust source once per test process
  (`OnceLock`-shared across the four tests). Estimated seconds, not measured.
* That the shipping-root definition (external reference outside
  `native-builtins/src`, minus test trees) matches `vm_init`'s real-JDK arm
  exactly. It is deliberately *wider* — wider means fewer names called
  synthetic-only, so the census under-reports rather than over-reports.
* `workspace_files` moved from 553 to 569 between runs, so other lanes are
  editing this worktree. No count that matters changed.
