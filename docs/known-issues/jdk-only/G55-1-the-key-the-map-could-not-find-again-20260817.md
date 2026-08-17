# G55-1 — the key the map could not find again

**Status:** FIXED IN SOURCE. Both files **TYPE-CHECK CLEAN** (`rustc`, lib and
`--test` configuration — §6). The pure logic **RUNS GREEN** (12 tests, §6). The
**after-state on the VM is NOT MEASURED and could not be**: the only binary on
this host is `C:/craton/target-rel4/release/cratonvm.exe`, built from
`cb2ade4fd`, which predates every edit here *and* predates `dd5d5430e`'s `sbidx`
fix. Every "after" below is labelled **PREDICTED** at each occurrence.

What **is** MEASURED, today, on both VMs:

* **188 probe rows** across four probes (`scratchpad/g55/G55{Probe,Fp,Ord,Ctrl}
  .java`, no lambdas, no `invokedynamic`, one class file per VM): 158 + 16 + 5 +
  9. **59 diverge**, and they are exactly **six** defects plus **two**
  non-surrogate ones the ASCII control isolates (§2).
* The before-state of `RJdkBridge1`, `--only=surrog`, and all seven green
  vectors (§0).
* Native-registry ownership for every method touched (§3).

**Owned files:** `native-builtins/src/properties_sidetable.rs` and
`native-collections/src/lib.rs`, and nothing else. Everything else is a
NOMINATION in §8.

```text
$ git status --short
 M native-builtins/src/properties_sidetable.rs
 M native-collections/src/lib.rs
?? scratchpad/
```

---

## 0. The headline

One root cause: **a Rust `str` cannot hold an unpaired UTF-16 surrogate**, so
every path that routes Java text through `String`/`&str` rewrites it to
`U+FFFD`. That single fact produced two *different kinds* of failure, and the
quieter one is the worse one:

| | what you see | MEASURED |
|---|---|---|
| **N1 — corruption** | `Properties.getProperty` hands back `va<U+FFFD>b` for a value stored as `va<U+DC00>b` | visibly wrong string |
| **N2 — silent lookup failure** | `HashMap.get(k)` returns **`null`** for the *same object* `put` was just given | nothing wrong-looking at all |

N2 is strictly worse and generalises past `Properties`: `put` files the entry
under `map_hash_key`, which asks the VM for the units; the `get` fast path
hashed the *decoded* text. The two hashed different bytes, so the entry was
filed in one bucket and looked for in another. There is no exception, no wrong
value, no log line — the map simply forgot a key it is still holding.

A third shape fell out of the same sweep and is worse again in one respect,
because it produces a **false positive** rather than a miss: `List.contains`,
`List.indexOf`, `Deque.contains` and `Map.containsValue` all answered "found"
for a string the collection does **not** hold, because both operands decoded to
the same `U+FFFD` (§2, defect **D5**).

| | before (MEASURED, `cb2ade4fd`) | after (PREDICTED) |
|---|---|---|
| `RJdkBridge1` | **236 checks, dies in `sbidx`** | see §7 — this lane's binary cannot show it |
| `RJdkBridge1 --only=surrog` | **dies at `Properties.setProperty(lone key, lone value)`** | **passes that step**; next unmeasured row is `TreeMap with a lone surrogate key` (§7) |
| `RCollections` | PASS, 53 | unchanged |
| `RJdkCollections` | PASS, 69 | unchanged |
| `RStrings` | PASS, 46 | unchanged |
| `RJdkStringCodePoints` | PASS, 186 | unchanged |
| `RJdkMapViews` | PASS, 74 | unchanged |
| `RChmKeySetView` | PASS, 4061 | unchanged |
| `RJdkHello` | PASS, 41 | unchanged |

All seven green vectors re-run today on `cb2ade4fd` at exactly those counts.
They are the *before* state; this binary cannot report an *after*.

---

## 1. The blocker, isolated

`RJdkBridge1.surrog` (22 checks) reaches row 8 and dies:

```text
CK RJdkBridge1 surrog-step=Properties.setProperty(lone key, lone value)
AssertionError: the value must come back with its unpaired low surrogate intact
```

MEASURED, both VMs, `scratchpad/g55/G55Probe.java`:

```text
                             ORACLE                CRATONVM
p.setProperty("ka\uD800b", "va\uDC00b")
p.getProperty(sameObject)    va<u+dc00>b           va<u+fffd>b       CORRUPTED
p.getProperty(equalObject)   va<u+dc00>b           va<u+fffd>b       CORRUPTED
p.keySet()                   {ka<u+d800>b}         {ka<u+fffd>b}     CORRUPTED

r.setProperty("\uD800","hi"); r.setProperty("\uDC00","lo")
r.size()                     2                     1                 KEYS MERGED
r.getProperty("\uD800")      hi                    lo                WRONG ENTRY
```

The second block is the `Properties`-shaped instance of N2, and it is the row
that matters most: two keys that Java calls different collapsed into one, the
second `setProperty` silently overwrote the first, and a later lookup answered
another key's value. The vector does not test it. An application would not
notice it either.

---

## 2. The sweep: 188 rows, six surrogate defects, two others

Method is `HANDOFF-20260814` §4 with G53-1's refinement — probe the whole family
on both VMs first, rather than fixing the first failing assertion and re-running.
Four probes, one pass:

| probe | rows | diverging | what it isolates |
|---|---|---|---|
| `G55Probe.java` | 158 | 44 | `Properties` × {get,set,put,remove,containsKey,keySet,stringPropertyNames,propertyNames,keys,elements,entrySet,toString,size,load,store,defaults} and `HashMap`/`Hashtable`/`LinkedHashMap`/`TreeMap`/`CHM` × {get,put,containsKey,remove,keySet,values,containsValue,hashCode} — each with a lone high, a lone low, a valid pair, a pair split across two keys, and a key equal-by-`equals` to a stored one |
| `G55Fp.java` | 16 | 10 | the FALSE-POSITIVE direction: does a collection holding `a\uDC00b` claim to contain `a\uD800b`? |
| `G55Ord.java` | 5 | 3 | `String.compareTo` is by code UNIT; `TreeMap`/`TreeSet` order |
| `G55Ctrl.java` | 9 | 2 | ASCII control — separates surrogate defects from ones that are simply broken |

### The six surrogate defects

| # | rows | MEASURED before | ORACLE | site |
|---|---|---|---|---|
| **D1** | 21 | `U+FFFD` on every read-back; two lone-surrogate keys MERGE | units preserved; two keys | `properties_sidetable.rs` — the store was `IndexMap<String, String>` |
| **D2** | 10 | `HashMap.get`/`Hashtable.get` → **`null`** for the same object | the value | `native_hashmap_get_string_fast` — hash and equality from decoded text |
| **D3** | 6 | `Map`/`Set.hashCode()` stable and wrong (2124877 vs 1839336) | JDK value | `element_hash_code` — String arm re-encoded a decoded `String` |
| **D4** | 4 | `TreeMap` merges two lone-surrogate keys; `keySet` shows `U+FFFD` | two keys | `TreeKey::Str(String)` |
| **D5** | 7 | `contains`/`indexOf`/`containsValue` answer **found** for a value not held | not found | `values_equal` — `read_string(a) == read_string(b)` |
| **D6** | 3 | `TreeSet`/`TreeMap` sort a supplementary character AFTER `U+FFFF` | BEFORE it | `TreeKey`'s `Ord` and `natural_compare`, both code-POINT order |

**D6 needs no surrogate exotica at all.** `String.compareTo` compares `char`s,
i.e. UTF-16 code units, and a supplementary character begins with a unit in
`0xD800..=0xDBFF` — below `U+E000..=U+FFFF`. Rust's `String: Ord` compares code
points, where it is above them. MEASURED both VMs:

```text
TreeSet of {U+10000, U+E000, U+FFFF}
  ORACLE    [U+10000, U+E000, U+FFFF]
  CRATONVM  [U+E000, U+FFFF, U+10000]     firstKey AND lastKey both wrong
```

Any `TreeMap`/`TreeSet` keyed by text containing an emoji or a CJK-ext character
enumerates in the wrong order. `String.compareTo` **itself is correct** on
CratonVM (the probe checks it directly); only the collections' own comparison
was not.

### The two the ASCII control isolates as NOT surrogate defects

Both reproduce with plain `"aXb"`, so they are unrelated to this lane's cause
and are left alone. Recorded because they were measured, not because they were
fixed:

| call | ORACLE | CRATONVM |
|---|---|---|
| `Properties.entrySet()` element `toString()` | `kab=vab` | `java.util.Map$Entry@5adb` |
| `Hashtable.hashCode()` | `96064` | `1206` |

`HashMap`/`LinkedHashMap`/`TreeMap`/`CHM`/`HashSet` `hashCode` all answer
`96064` on ASCII, so `Hashtable`'s is its own defect and not `element_hash_code`'s.
See §8 N4/N5.

### And one pre-existing divergence in `Properties.store`

MEASURED: HotSpot writes `\uD800` (upper-case hex, `saveConvert`'s `toHex`);
CratonVM's `save_convert` has always used `{:04x}`, so it writes `\ud800`.
`load` is case-insensitive, so the round-trip is intact and nothing observed it
— but it is a real difference in the bytes `store` produces, for **every**
non-Latin-1 character, not just surrogates. Not fixed here: the change is one
character but it contradicts an existing test's expectation and belongs to a
`store`-family sweep, not this one. §8 N6.

---

## 3. Ownership, established before writing

`--dump-native-registry` under `--jdk-only` (schema 4), 10 691 natives.

| method | `owns_slot` | registered by |
|---|---|---|
| `Properties.{getProperty×2, setProperty, get, put, remove, containsKey, getOrDefault, size, keySet, keys, elements, entrySet, propertyNames, stringPropertyNames, load×2, store×2, clear, contains, containsValue, forEach, equals}` | **true** | `native-builtins/src/properties_sidetable.rs` |
| `Properties.{getProperty, get, containsKey, entrySet, …}` | **false** | `native-collections/src/lib.rs` (overwritten) |
| `HashMap.{get, put, containsKey, remove, keySet, …}` | **true** | `native-collections/src/lib.rs:10077+` |
| `Hashtable.{get, put, remove, …}` | **true** | `native-collections/src/lib.rs:53803+` |
| `LinkedHashMap.*`, `TreeMap.*`, `ConcurrentHashMap.*` | **true** | `native-collections/src/lib.rs` |
| `Hashtable.{keys, elements}`, `Hashtable.clone` | **true** | `native-builtins/src/deprecated_util.rs` — **not mine** |

The brief's `LAST-WRITE-WINS BOUNDARY` prediction holds exactly: for every
`Properties` triple the winning registration is `properties_sidetable.rs`, and
`native-collections`' same-named registration carries `owns_slot: false`. Both
halves of this lane's assignment are therefore in the two owned files.

`invocations` was `0` for the `Properties` rows, as the brief predicted; it was
not used for anything.

**Reachability**: none of these sites sits behind `use_synthetic_jdk` — the
probes exercise them under `--jdk-only` against the real JDK image and every
divergence above is a live measurement through the code being edited, not an
inference from the source.

---

## 4. The fix — `properties_sidetable.rs`

The side-table's element type changes from `String` to **`JavaText`**, a newtype
over `Vec<u16>`:

```rust
type PropsMap = indexmap::IndexMap<JavaText, JavaText, BuildHasherDefault<FxHasher>>;
```

Its derived `Eq`/`Hash`/`Ord` are *exactly* Java's `String` contracts, because
those are defined on code units — unlike `String`'s `Ord`, which is by code
point and is D6.

Two new primitives, both **delegating** to what the crate already has, because
the recurring wrong conclusion here is "`NativeContext` needs a
`create_string_from_utf16`" and it does not:

* `read_java_text` — the type guard is still `ctx.read_string` (plus
  `java_string_hash_code` as a VM-side second opinion), and the **units** come
  from `lang_string::read_string_chars`. No third decode.
* `create_property_string` — well-formed text takes the **unchanged**
  `ctx.create_string` path, so interning and object identity are bit-for-bit
  what every green `Properties` vector already measured; only content a `&str`
  cannot carry is rerouted through `lang_string::sb_string_from_units`
  (`new_object` + `NativeContext::init_string_from_units`).

Converted with it: `put_kv`/`get_kv`/`remove_kv` (which keep their `&str`
signatures as *adapters* for the genuinely-Rust-text callers — the cross-module
`pub` API, the system-property store, diagnostics — over new `_units` cores),
`snapshot_kv`, `ordered_snapshot_kv`, `reorder_by`, `chm_key_order`,
`side_key_set`, `chm_extra_entries`, `mirror_loaded_entries_to_properties_backend`,
`store_parsed_entries`, `build_string_collection`, `build_enumeration`, and every
native that reads or writes a Java `String`. Every `pub`/`pub(crate)` signature
other modules use is **unchanged**.

Two places got simpler rather than more complex:

* **`unescape_inner` now emits units**, and the special case that existed only
  to work around a `String` return value is *deleted*: it used to greedily pair
  a high `\uXXXX` escape with the low one after it and fold them into one Rust
  `char`, because that was the only way a supplementary character could survive
  — and a lone half then had nowhere to go. Each escape is now one unit, which
  is literally what the JDK's `loadConvert` does. `peek_low_surrogate` and its
  speculative-clone lookahead are gone.
* **`save_convert` now walks units**, which is the shape `saveConvert` was
  always written in (it walks `char`s). One deliberate divergence, documented
  at the site: an unpaired surrogate is written `\uXXXX` even for
  `store(Writer)`, where HotSpot writes the raw unit through the Writer's
  encoder. There is no `char` to write; the escape re-loads to the same unit, so
  the round-trip this function exists to protect is intact. `U+FFFD` is not.

The stale "LIMITATION / CROSS-FILE FOLLOW-UP" note that stood above `unescape`
— *"exact preservation requires storing the value as `[u16]` units … that path
is not reachable from this `String`-typed pipeline"* — is discharged and
replaced.

## 5. The fix — `native-collections/src/lib.rs`

Five sites, and the theme is **stop deciding things about Java text from a
decoded `&str`**:

1. **`native_hashmap_get_string_fast`** — hash via `ctx.java_string_hash_code`,
   equality via `ctx.java_strings_equal`. The hash is now *literally the same
   call* `map_hash_key` makes, which is what makes "`put` and `get` agree"
   structural instead of coincidental. `java_string_hash_code` also replaces
   `read_string` as the "is this a `java.lang.String`" gate. A chain node whose
   key the comparison could not read now returns the whole lookup to the general
   ladder rather than reporting "absent" — the same refusal
   `native_chm_get_string_chain` already makes, for the reason
   `chm-get-misses-stored-key-in-process-RETIRED-20260804.md` records.
2. **`element_hash_code`** — `ctx.java_string_hash_code`, so the collection
   `hashCode` contracts and the bucket hash cannot disagree about one String.
3. **`values_equal`** — `ctx.java_strings_equal`.
4. **`TreeKey::Str(Vec<u16>)`**, built through `tree_key_from_decoded_string`.
5. **`natural_compare`**'s String arm — `compare_decoded_strings`.

`native-collections` has no dependency on `native-builtins`, so
`lang_string::read_string_chars` is out of reach and adding a decoder here would
be the second encoding this codebase keeps warning about. Sites 1–3 need no
units reader at all: `java_string_hash_code` and `java_strings_equal` are
already on `NativeContext` and are already answered VM-side from the receiver's
character storage. Sites 4–5 do need text, so they are gated on one shared,
**sound** predicate:

```rust
fn decode_is_faithful(text: &str) -> bool { !text.contains('\u{FFFD}') }
```

Text with no `U+FFFD` provably survived the decode. Text with one may be a
genuine `U+FFFD` or a mangled surrogate, and no `&str` comparison can tell —
so both sites **refuse** and hand the decision to the units-exact path (the
array/`compareTo` fallback). Conservative: a genuine `U+FFFD` key takes the slow
path. That is the right trade for content the representation cannot hold.

The `&str`-indexed HashMap node memo is gated on the same predicate, so a
mangled text can never enter it. The CHM memo needed no change — it is keyed by
key-object identity and its stored text is never compared (verified in
`vm_exec.rs:11275`).

## 6. Verification

**`rustfmt --edition 2021 --check`**, hunk-for-hunk against the same file at
`HEAD`: `properties_sidetable.rs` 9 pre-existing hunks → **9**;
`native-collections/src/lib.rs` 89 → **89**. No new formatting deviations.
Neither file gained a CR.

**Type-checked with `rustc`** — not `cargo`, which the brief forbids; see
`37acb6acb`, *"the rule stands; it was never a no-compiler rule"*:

| crate | config | result |
|---|---|---|
| `native-collections` | `--crate-type lib --emit=metadata` | **clean** |
| `native-collections` | `--test --emit=metadata` | **clean** |
| `properties_sidetable.rs` + real `test_utils.rs` | `--emit=metadata` | **clean** |
| same | `--test --emit=metadata` | **clean** |

The `properties_sidetable` check is against a stub crate root
(`scratchpad/g55/propcheck/`) carrying the **real signatures**, copied from
`native-builtins/src/{lib.rs, util_concurrent_ext.rs}`, for the eight
`crate::`/`super::` items the module reaches for, plus the real bodies of the
three `lang_string` helpers it now calls. That is a faithful check of everything
except those stubbed bodies.

**Executed**: linking a full test binary against the prebuilt release rlibs
fails on unresolved generic instantiations (the rlibs were built with different
codegen settings), so the pure logic runs in a standalone harness,
`scratchpad/g55/pure/harness.rs` — the algorithms copied verbatim:

```text
running 12 tests ... test result: ok. 12 passed; 0 failed
```

covering: a lone surrogate survives `unescape_inner`, a well-formed pair stays
two units and still renders as one supplementary character, `parse_properties`
keeps the unit, two lone-surrogate keys stay two map entries and a removal takes
only one of them, `reorder_by` does not conflate them, `JavaText` orders by unit
where a `String` orders by code point, `save_convert_units` writes an escape that
`load` reads back as the same unit, both extracted `decode_is_faithful` gates,
`TreeKey` ordering and distinctness — plus the *unchanged* escape and
`save_convert` grammars and the gh-11892 `reorder_by` ordering, so the
regression direction is covered too.

**Unit tests added in the real files**, all asserting **UTF-16 units**:

* `properties_sidetable.rs`: `unescape_units_keeps_a_lone_surrogate_as_itself`,
  `parse_keeps_a_lone_surrogate_escape_as_its_unit`,
  `two_keys_differing_only_in_their_unpaired_surrogate_stay_two_keys`,
  `reorder_does_not_conflate_two_lone_surrogate_keys`,
  `java_text_orders_by_code_unit_where_a_rust_string_orders_by_code_point`,
  `save_convert_units_writes_a_lone_surrogate_as_a_reloadable_escape`,
  `save_convert_still_writes_a_well_formed_pair_as_two_escapes`,
  `create_property_string_materialises_an_unpaired_surrogate_intact`,
  `create_property_string_leaves_well_formed_text_on_the_unchanged_path`,
  `read_java_text_reads_units_and_refuses_a_non_string`. The existing
  `unescape_lone_surrogate_becomes_replacement_not_dropped` is KEPT and
  re-commented: `U+FFFD` is the right answer for the *lossy* view, and that test
  passes on the broken code, which is exactly why the units twin sits next to it.
* `native-collections/src/lib.rs`:
  `a_lone_surrogate_key_hashes_by_its_units_not_by_a_replacement_char`,
  `two_different_lone_surrogates_are_not_the_same_key_or_the_same_value`,
  `two_equal_lone_surrogate_strings_are_still_equal`,
  `a_string_compared_against_a_non_string_falls_through_rather_than_deciding`,
  `tree_key_refuses_a_string_whose_decode_may_have_lost_a_surrogate`,
  `tree_keys_order_by_code_unit_where_a_rust_string_orders_by_code_point`,
  `tree_keys_keep_two_lone_surrogates_apart`,
  `the_faithful_decode_rule_gates_both_the_key_and_the_comparison`,
  `natural_compare_puts_a_supplementary_string_before_u_ffff`.

  These needed a `MockCtx` that can model a String at all — its `read_string`
  was an unconditional `None`. It now consults a new `strings: HashMap<usize,
  Vec<u16>>` populated by `define_string(&[u16])`, and is **lossy exactly as the
  VM's reader is**, so a test can demonstrate the mangling the units-exact
  primitives avoid. `java_string_hash_code` and `java_strings_equal` are
  answered from the same units. Objects nothing called `define_string` for still
  answer `None`, so no existing test changes behaviour.

## 7. Where `RJdkBridge1` lands (PREDICTED)

`surrog` is 22 checks and dies on row 8 of ~12. This lane's two rows —
`getProperty` returns the value with `U+DC00` intact, and the key is found again
— are the assertion that aborts it, and both now hold at the unit level. So
**`surrog` should get past `Properties`**.

It is NOT predicted to go green on this lane alone. The rows after the abort
have never executed on either VM, and two of them are outside these files:

* `TreeMap with a lone surrogate key` — D4/D6 are fixed here, and the specific
  rows the vector asserts (`size()==3`, `firstKey()=="A"`,
  `lastKey()==U+FFFF`) should now hold for the right reason rather than by
  accident;
* `ArrayDeque.contains` / `Vector.indexOf` — D5 is fixed here;
* `new URI("http://h/a\uD800b")` and `new BigInteger("1\uD8002")` — **not mine**
  (§8 N1/N2);
* `new String(char[])` with a lone surrogate — `lang_string.rs`, and G53-1
  measured it already correct.

`RJdkBridge1` as a whole still needs `dd5d5430e`'s `sbidx` fix to be in a
binary before it can reach `surrog` at all; on `cb2ade4fd` it dies at 236.

## 8. NOMINATIONS

**N1 — `URI` with a lone surrogate in the path.** `RJdkBridge1.surrog` asserts
`new URI("http://h/a\uD800b").toString()` returns the path with `U+D800`
intact and throws nothing. Unmeasured (the family aborts before it). Owner: the
`java/net/URI` natives. Same root cause; the same two primitives
(`read_string_chars` / `sb_string_from_units`) are the fix if it diverges.

**N2 — `new BigInteger("1\uD8002")` must be a `NumberFormatException`.** The
vector's own wording: *"a Java throwable, NOT a Rust str conversion failure"*.
Unmeasured, same reason. Owner: `native-builtins`' BigInteger natives.

**N3 — `native-collections` has no units-preserving String reader, and cannot
get one.** It does not depend on `native-builtins`, so
`lang_string::read_string_chars` is unreachable, and a local decoder would be
the second encoding. The consequence is bounded but real: `HashMap.toString()`
/ `AbstractCollection.toString()` render `U+FFFD` for an unpaired surrogate,
because `obj_to_display_string` must build a Rust `String`. Three of the four
things this crate needs to ask about Java text already have units-exact
`NativeContext` methods answered VM-side (`java_string_hash_code`,
`java_strings_equal`, `init_string_from_units`); the missing fourth is a
*reader*. **This is not G9-1's `read_string_units` nomination restated** — that
one was correctly refused because `native-builtins` already has the reader. This
is the crate-boundary case that argument does not cover. Owner: `native-api`.

**N4 — `Properties.entrySet()` elements have no working `toString`.** MEASURED
with ASCII: HotSpot `[kab=vab]`, CratonVM `[java.util.Map$Entry@5adb]`. Not a
surrogate defect. The site is in **my** file (`native_properties_entry_set` →
`make_static_entry_set`), but the entry class and its `toString` are
`native-collections`' to register, and fixing it belongs with a `Map.Entry`
sweep rather than this one.

**N5 — `Hashtable.hashCode()` is wrong on ASCII.** MEASURED `1206` where
HotSpot and every sibling map say `96064`. `Hashtable` has no `hashCode` native,
so real JDK bytecode runs against synthetic storage. Not a surrogate defect.

**N6 — `Properties.store` writes lower-case `\uXXXX` hex.** MEASURED: HotSpot
`\uD800`, CratonVM `\ud800` — for every non-Latin-1 character, not just
surrogates. `load` is case-insensitive so nothing breaks; the bytes differ.
`save_convert`'s `{:04x}` → `{:04X}` is the whole fix, but it contradicts
`save_convert_unicode_escaping`'s expectation and belongs to a `store`-family
sweep with the oracle re-measured for comments, ordering and the date line.

**N7 — `Properties.load(Reader)` DROPS a raw unpaired surrogate.** The
accumulator is a Rust `String` built with `char::from_u32`, which returns `None`
for a surrogate, so the unit is silently dropped and the text shifts. Escaped
`\uD800` in a `.properties` file — the normal spelling, and what the probe and
the vector use — is fixed by this lane; a Reader that yields a raw unpaired
surrogate is not. Fixing it means making `parse_properties_text_inner` and
`split_key_value` units-typed, which is a second, larger change to the same
file. Site: `native_properties_load_reader`, in my file, deliberately not taken.

## 9. What I could not settle

* **The after-state on the VM.** No `cargo build`/`check`/`test` is permitted
  and no binary newer than `cb2ade4fd` exists here, so §0's after column and §7
  are PREDICTED. Both predictions are cheap to falsify: re-run
  `--only=surrog`; the `Properties` step should pass and the abort should move
  to a later row or disappear.
* **`--test` links, but does not run.** Both files type-check in the test
  configuration; the test *binaries* cannot be linked against the prebuilt
  release rlibs (unresolved generic instantiations from `hashbrown`,
  `cratonvm_types`). The 12 green tests in §6 are the copied pure logic, not the
  in-tree test bodies.
* **`Properties.store(Writer)` with an unpaired surrogate now differs from
  HotSpot in a new, documented way** — escape vs raw unit (§4). It round-trips;
  it is not byte-identical. No vector covers it.
* **The `surrog` rows after `Properties`** remain unmeasured on both VMs,
  because the family still aborts before them on this binary.
