# H4-1 — the map/set ownership cluster is not a tag, and the twelve cells are not reachable from one

**Status: FIXED-UNVERIFIED — no binary carrying these changes has been built or
run.** The changes are **comments only**: no registration, no `NativeKind`, no
behaviour. So the honest prediction for the arms is *byte-identical*, and §7
says how to falsify even that.

**Provenance: SOURCE-ONLY.** Every number below is a grep or a read of the tree
in worktree `C:/craton/cratonvm/.claude/worktrees/agent-a4b6d05477838b686`,
2026-08-20. I was not permitted to build, run, or measure. **No number in this
record is a measurement**, and nothing here contradicts a measurement except
where §5 shows the measurement was of a different mode than the conclusion drawn
from it.

The census in §1–§4 was first taken at `26e4b5db4` (the stale base this worktree
was created at — see §10d) and **re-taken after merging
`claude/jdk-only-mode-handoff-09b48c` at `e6d642f3b`**, which brought in the
whole H wave including `H0-2` and a `vm/src/jit/helpers.rs` change. **Every
count in §1 is identical across the two trees** (168 direct calls, 45/19/4/3
synthetic allocations, 6 JIT helpers — the JIT helpers only moved line numbers),
and the three `allowed_in` / `real_protected_stub_class` / `invoke_or_native`
facts §5b turns on were re-verified against the merged tree. Line numbers in
this record are post-merge; anchor on the enclosing function name.

**On the stub ratchet.** Nothing here moves it, because nothing here changes a
kind. When the cluster work does move, expect the shape `e6d642f3b` just
adjudicated for `sun/nio/fs/WindowsFileAttributes`: **a `Bridge` → `SyntheticStub`
retag makes the stub COUNT RISE while being the opposite of a regression.** The
two-column output plus a `CRATONVM_RATCHET_ROWS=1` row diff is what separates a
relabel from a new fake; a bare count cannot.

Lane H4, 2026-08-20.

---

## 0. The assignment, and what it turned into

The brief was: reproduce `G88-1` §5's whole-cluster retag of the map/set family,
extend it across the crate boundary to Properties/Hashtable now that contract §8
is lifted, and watch `H0-2` §4's twelve `instanceof` cells come right as proof
that the containers became real.

**None of the three is reachable by a `NativeKind` change, and each fails for a
different, checkable reason.** In one line each:

| part | verdict | the fact |
|---|---|---|
| 1 — move the map/set cluster | **not landable** | 168 direct Rust calls + 45 synthetic-`HashMap` allocation sites + 6 JIT direct helpers write this state without ever consulting the registry |
| 2 — extend to Properties/Hashtable | **not landable, and never blocked by §8** | `native-builtins/src/lib.rs` holds **zero** `Properties`/`Hashtable` registrations; the blocker is `System.getProperties()`'s null-`map` singleton |
| 3 — the twelve cells | **unreachable by any retag** | strict mode already refuses every producer; compatible mode discards the kind entirely |

`G88-1` §5's measured result stands as far as it goes and I am not disputing it.
What I dispute is the inference that its green vectors licence the change, and
§1 gives the population its vectors did not ask.

---

## 1. The cluster boundary is not the registry — 168 call sites say so

`G88-1` §5's finding was *"the unit of work is the ownership cluster, not the
registrar"*. Correct, and it stops one level too early: **the cluster is every
writer of a container's state, and most of them are not Java dispatch at all.**

`NativeKind::SyntheticStub` acts in exactly one place —
`NativeMethodRegistry::register_inner`'s `JdkOnly` arm, which returns before the
push to `registrations`/`slots`. It removes a *registration*. It does not remove
the Rust function, and it cannot see a caller that never dispatched.

Three populations call these natives without dispatching:

### 1a. 168 direct Rust calls, 18 files, two crates

```bash
grep -rn 'cratonvm_native_collections::native_map\|::native_hashmap\|::native_hs' \
  --include=*.rs native-builtins/src vm/src native-io/src
```

| entry point | sites |
|---|---:|
| `native_map_put_pub` | 42 |
| `native_map_init` | 34 |
| `native_map_get_pub` | 33 |
| `native_map_contains_key_pub` | 11 |
| `native_map_remove_pub` | 10 |
| `native_map_size_pub` / `native_map_key_set_pub` | 6 each |
| `native_map_entry_set_pub` / `_contains_value_pub` / `_clear_pub` | 4 each |
| `native_map_values_pub` / `_to_string_pub` / `_is_empty_pub` | 3 each |
| `native_map_keys_as_array`, `native_hashmap_get_exact` | 2 each |
| `native_hashmap_put_exact` | 1 |

By file, the holders that matter:

```text
  native-builtins/src/phases_early.rs             53
  native-builtins/src/phases_late/beans_jndi.rs   42
  native-builtins/src/locale_resources.rs         12
  native-builtins/src/phases_late/net_channels.rs 12
  native-builtins/src/phases_late/xml_json.rs     11
  native-builtins/src/reflect_annotations.rs      10
  native-builtins/src/phases_late/text_intl.rs     7
  native-builtins/src/phases_late/collections.rs   6
  native-builtins/src/{antlr_intrinsics,logging_shims}.rs 5 each
  native-builtins/src/{jca/provider_chain,test_frameworks,properties_sidetable}.rs 4 each
  vm/src/jit/helpers.rs                            3
  native-builtins/src/lib.rs                       2
  … 5 more files at 1–2
```

**Six of the 168 are in files this lane owns.** That number — not `G88-1` §6's
58 % — is the honest scale of what a `native-collections` (or even a
`native-builtins/src/lib.rs`) change can reach.

### 1b. 45 synthetic `HashMap` allocations outside the crate

```bash
grep -rho 'try_alloc_concurrent_synthetic([a-z_ *&]*, *"java/util/[A-Za-z$]*"' \
  --include=*.rs native-builtins/src vm/src native-io/src | sort | uniq -c
```

```text
  89 "java/util/ArrayList"      45 "java/util/HashMap"     19 "java/util/HashSet"
   4 "java/util/Hashtable"       3 "java/util/Properties"   2 "java/util/TreeMap"
```

`try_alloc_concurrent_synthetic` (`native-builtins/src/util_concurrent_ext.rs`)
resolves the **real** class id and allocates `max(requested, real_field_count)`
slots. So the object genuinely *is* a `java.util.HashMap` to the class manager;
what is missing is that its state was then written through 1a instead of through
real bytecode. Worked instance, `locale_resources.rs`:

```rust
let map = try_alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3)?;
cratonvm_native_collections::native_map_init(ctx, &[Value::Object(Some(map))]).ok();
…
ctx.set_field(obj_now, 0, Value::Object(Some(map_now)));   // handed to Java
```

### 1c. 6 JIT direct helpers

```bash
grep -rn 'cratonvm_native_collections::' --include=*.rs vm/src/jit/helpers.rs
```

```text
13059  native_chm_get              (in jit_concurrent_hashmap_get_direct)
13164  jit_overlay_hashmap_get
13185  native_hashmap_get_exact
13353  jit_overlay_hashmap_put
13538  native_hashmap_put_exact
13541  native_hashmap_get_exact
```

`jit_concurrent_hashmap_get_direct` builds a `NativeContextImpl` and calls
`native_chm_get` as a plain Rust function, "avoid[ing] the second, redundant
safe-native wrapper". It consults no policy and no kind. After a retag the
interpreter would read the real `table` and compiled code the (now unwritten)
side segments — **a tier-dependent wrong answer, which `run.sh` does not diff
for.** This is the `jit-thin-direct-helpers-reimplement-natives` species.

### The failure mode, and why a green arm cannot see it

Refuse `java/util/HashMap`'s registrations and every object produced by 1a/1b is
handed to real bytecode that reads a real, empty `table` and answers **absent**.
Not an exception: a silently empty map. `G88-1` §5's three green vectors
(`RCollections`, `RJdkMapViews`, `RChmKeySetView`) construct no map through any
of the 168 sites, so they asked nothing about it and reported a pass.

`HANDOFF-20260819.md` §3 names this shape and says it is the only thing that
catches it: *"a gate whose stated population is wider than its measured one … It
always reads as success. Nothing catches it but running the wider thing."* The
wider thing here is the Spring / H2 / Tomcat suites, not the 102-vector arm —
`locale_resources`, `beans_jndi`, `xml_json` and `net_channels` are exactly
their paths.

---

## 2. §5's ten registrars — the list I actually found, and how it differs

The brief was right that "ten" is a description of an experiment, not a
guarantee. Reconstructing the set from `G88-1` §5's own words (containers, view
carriers, iterators, map entries, bulk ops) gives exactly ten functions, all of
which exist today, all of which set their **own** `set_category(Bridge)` at
their head — so the `G85-1` call-site-wrapper trap does not apply to any of
them, and each would have to be edited at its head:

| # | registrar | line (at `26e4b5db4`) | own `set_category`? |
|---|---|---:|---|
| 1 | `register_hashmap_natives` | 10561 | yes |
| 2 | `register_linked_hashmap_natives` | 38442 | yes |
| 3 | `register_hashset_natives` | 16026 | yes |
| 4 | `register_concurrent_hashmap_natives` | 50437 | yes |
| 5 | `register_map_view_carrier_natives` | 5085 | yes |
| 6 | `register_set_view_carrier_natives` | 16148 | yes |
| 7 | `register_chm_key_set_view_natives` | 53703 | yes |
| 8 | `register_iterator_natives` | 17436 | yes |
| 9 | `register_map_entry_natives` | 19950 | yes |
| 10 | `register_bulk_ops_natives` | 41428 | yes |

An eleventh moves implicitly and is not in §5's list:
`register_map_conditional_mutators` (3 rows, no `set_category` of its own) is
called from `register_hashmap_natives`, `register_linked_hashmap_natives` and
twice from `register_properties_natives`, so it inherits its **caller's**
category — moving with HashMap and LinkedHashMap automatically, and staying
`Bridge` for Hashtable/Properties automatically. That is the one place the
existing structure already does the right thing.

**Four of the ten span more than one ownership family, so a whole-registrar move
is itself a partial retag one level down:**

* `register_bulk_ops_natives` — 8 rows over **five** containers: `ArrayList` ×2,
  `HashSet` ×3, `LinkedList`, `Vector`, `ArrayDeque`. Only the three `HashSet`
  rows are in the map/set cluster. Refusing `ArrayList.removeAll` while
  `register_arraylist_natives` still owns the elements is precisely the split
  §5 was written to warn about.
* `register_iterator_natives` — `java/util/ArrayList$Itr` (ArrayList family) and
  the `MAP_KEY_ITR_CARRIERS` block (HashMap family), in one function.
* `register_map_view_carrier_natives` — `MAP_VIEW_CARRIERS` holds
  `HashMap$Values`, `LinkedHashMap$LinkedValues`, **`TreeMap$Values`**,
  **`TreeMap$EntrySet`**, **`Hashtable$ValueCollection`**,
  `ConcurrentHashMap$ValuesView`: four families.
* `register_set_view_carrier_natives` — `SET_VIEW_CARRIERS` adds
  **`Hashtable$KeySet`** and **`Hashtable$EntrySet`** to the same problem.

The TreeMap row is the sharpest instance and is worth spelling out because it is
mechanical. A view carrier is minted by a live native (`alloc_view_carrier`)
with its list state at *undeclared* slots and `this$0` left **null**. Refuse the
carrier's registrations while its producer (`register_tree_map_natives`) is
still `Bridge`, and real `TreeMap$Values.size()` bytecode runs and dereferences
that null `this$0`. §5 moved these registrars whole and its vectors did not
build a `TreeMap` view.

**So the split has to be per carrier family, keyed on whether the producing
registrar moved — not per registrar.** That is a strictly *safer* shape than
§5's, because dispatch keys on class: any class left `Bridge` behaves exactly as
today.

---

## 3. The edge that couples HashMap's iterators to Hashtable

This one is not in any record and it is why the two clusters cannot be
sequenced independently.

`register_set_view_carrier_natives` registers `iterator` →
`native_hs_iterator` for **every** `SET_VIEW_CARRIERS` entry, including
`java/util/Hashtable$KeySet` and `$EntrySet`. `native_hs_iterator` mints its
result through `key_itr_carrier_for`, whose fallback arm is:

```rust
    match (linked, entryset) {
        (true,  false) => "java/util/LinkedHashMap$LinkedKeyIterator",
        (true,  true)  => "java/util/LinkedHashMap$LinkedEntryIterator",
        (false, false) => "java/util/HashMap$KeyIterator",
        (false, true)  => "java/util/HashMap$EntryIterator",
    }
```

A `Hashtable$KeySet` receiver is not `linked`, so it takes the third arm.
**A live Hashtable-family carrier mints a `HashMap$KeyIterator`.**

Now refuse the four `MAP_KEY_ITR_CARRIERS` (as any HashMap-cluster move must,
because `HashMap.keySet().iterator()` has to become real) while
`Hashtable$KeySet` is still `Bridge`. The Hashtable carrier keeps minting a
`HashMap$KeyIterator` whose natives are gone; real
`HashMap$HashIterator.hasNext()` reads its own `next` field, finds null, and
reports **an empty iteration**. No exception. `Properties.keySet()` iteration
silently yields nothing.

Two ways out, both larger than a tag:

* move the Properties/Hashtable cluster in the same commit (§4 says why that is
  not a tag either); or
* give the Hashtable family its own iterator carrier.
  `java.util.Hashtable$Enumerator` is what HotSpot actually answers here, so
  this would *fix* an existing `getClass()` divergence rather than invent one,
  and `native-builtins/src/deprecated_util.rs` already carries
  `HASHTABLE_ENUMERATOR_CLASS` and its ctor descriptor. Nominated (N3), not
  taken: it is a mint-site change with its own layout question and I cannot
  build.

---

## 4. Part 2 — the Properties/Hashtable cluster, and the blocker G88-1 named wrongly

### 4a. The crate split reproduces; the FILE claim does not

`G88-1` §6: *"The ownership cluster spans two crates, and the larger half lives
in `native-builtins`* — the crate whose `lib.rs` contract §8 forbids editing
this wave*"*, with `native-builtins 67 / native-collections 49`.

The **49** reproduces exactly: `register_properties_natives`
(`native-collections/src/lib.rs:54435`) makes 43 direct registrations on
`java/util/Properties` and `java/util/Hashtable`, plus two calls to
`register_map_conditional_mutators` at 3 rows each. 43 + 6 = 49.

The **67** does not live where the record says. Class-position registrations,
2026-08-20:

```bash
grep -rn '"java/util/Properties",$\|"java/util/Hashtable",$\|("java/util/Properties",\|("java/util/Hashtable",' \
  --include=*.rs native-builtins/src | sed 's/:.*//' | sort | uniq -c
```

```text
   4 native-builtins/src/deprecated_io_util.rs
   6 native-builtins/src/deprecated_util.rs
  32 native-builtins/src/properties_sidetable.rs
   3 native-builtins/src/wildfly_naming.rs
```

plus the `let ht = "java/util/Hashtable"` loop registrations in the two
`deprecated_*` files. **`native-builtins/src/lib.rs`: zero.** Its only two hits
on the string are method *descriptors* (`"(Ljava/util/Properties;)V"` at 10528,
`"()Ljava/util/Properties;"` at 10615).

`HANDOFF-20260819.md` §1 already states the general form of this correction —
*"Contract §8 … names **one file**, not the crate"* — and `G88-1` §6 is the
record that conflated the two. **Contract §8 was never the structural blocker
for this cluster.** Lifting it (which the user did on 2026-08-20) does not
unblock the row, because the row was not blocked there.

### 4b. What the blocker is

`System.getProperties()` (`native-builtins/src/lib.rs:10612`) returns a
*synthetic* `Properties` singleton. `register_properties_sidetable`'s own header
records what it is and what happens without it, verbatim:

> *"`System.getProperties()` … hands back a 'lightweight synthetic Properties
> object' whose inherited Hashtable/ConcurrentHashMap backing is deliberately
> never populated — real JDK 25 `Properties`/`Hashtable` bytecode dereferences a
> `map` field that is permanently null on this object, so EVERY one of these
> overrides … is the only thing that makes the synthetic object behave like a
> real Map at all."*

and the measured consequence of dropping them, on 2026-07-14:
`InternalError: null property: java.home` out of `java.util.Locale.<clinit>` —
i.e. any real-JDK-mode program that touches `Locale`. Retagging
`register_properties_sidetable` `SyntheticStub` reproduces that under
`--jdk-only` **by construction**, because `register_inner`'s `JdkOnly` arm drops
the registration outright. That is the same drop mechanism, reached from the
other side.

### 4c. The producer populations for Part 2

1. `System.getProperties()`'s synthetic singleton (`native-builtins/src/lib.rs`).
2. 3 `try_alloc_concurrent_synthetic(_, "java/util/Properties", n)` and 4
   `"java/util/Hashtable"` sites outside `native-collections`.
3. **`java.security.Provider extends Properties`.**
   `native-builtins/src/jca/provider_chain.rs` reads
   `Hashtable.table:[Ljava/util/Hashtable$Entry;` and `Hashtable.count:I`
   directly (lines 285, 448), so the whole JCA registry is a second consumer of
   this state. Nothing in `G88-1` mentions it.
4. This cluster's own 4 direct calls into `native-collections`' map natives from
   `properties_sidetable.rs`, and its view-minting through
   `make_live_values_list` / `make_static_entry_set` — the §3 edge.

`RJdkBridge1`'s standing complaint (*"equals/isEmpty are Hashtable's and must
ignore the defaults table"*) is a **correctness** statement about
`native_properties_equals` / `native_properties_is_empty`, and it is independent
of the tag. **Its wording is also wrong about JDK 25, which matters because it
points at the wrong body.** Read out of the image rather than assumed
(`unzip -p "$JDK/lib/src.zip" java.base/java/util/Properties.java`, JDK
25.0.3+9 at `C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot`):

```java
    // Hashtable methods overridden and delegated to a ConcurrentHashMap instance
    @Override public int size()      { return map.size(); }
    @Override public boolean isEmpty() { return map.isEmpty(); }
    @Override public synchronized boolean equals(Object o) { return map.equals(o); }
    @Override public synchronized int hashCode()           { return map.hashCode(); }
```

`Properties` **does** declare all four (`javap -p java.util.Properties`
confirms), and they delegate to the private
`transient volatile ConcurrentHashMap<Object,Object> map`, not to `Hashtable`'s
table. The *assertion* the vector makes — ignore `defaults` — is right; the
*mechanism* it names is not, and "these are Hashtable's" would send a fixer to
`register_properties_natives`'s `ht` block instead of to the three natives that
actually answer. Nominated as N4.

This also sharpens §4b: the field these four delegate to is the same `map` that
`System.getProperties()`'s synthetic singleton leaves permanently null.

---

## 5. Part 3 — the twelve cells cannot be moved by a retag, in either mode

`H0-2` §5 says the principled fix for its twelve divergent `instanceof` cells is
`G88-1` §5's cluster move: *"When `Map.of(...)` returns a real
`ImmutableCollections$Map1`, `is_subclass_of` walks a real chain and all twelve
cells come right."* **A `NativeKind` change cannot produce that state, and this
is settled from the tree in two steps.**

### 5a. Under `--jdk-only`, every producer is ALREADY refused

Every allocator of a `cratonvm/internal/Unmodifiable*` carrier is already
`SyntheticStub`, by three different mechanisms, all in
`native-collections/src/lib.rs`:

| producer | how it is tagged |
|---|---|
| `register_factory_natives` (`List.of`/`Set.of`/`Map.of`, 36 rows) | `r.set_category(SyntheticStub)` **at its own head**, line 20748 — the call-site `with_category` wrapper above it is redundant, not the mechanism |
| the six `Collections.unmodifiable*` rows | explicit inner `set_category(SyntheticStub)` window inside `register_collections_extras_natives` |
| `List.copyOf` / `Set.copyOf` / `Map.copyOf` | explicit `register_with_kind(…, SyntheticStub)` |

`register_inner`'s `JdkOnly` arm drops all of them, so strict mode never reaches
`alloc_immutable_wrapper` and already answers the twelve cells from real
`java.base` bytecode.

**Corroborated by the arm results already in the tree**, and the derivation is
written out so nobody has to redo it. `regression-suite/run.sh:408`:
`SUITE=all` is `CORE_CLASSES $JDKONLY_CLASSES`, and `RImmutableFactoryTypes` is
in `CORE_CLASSES` (line 177). `HANDOFF-20260819.md` §6 reports
`CRATONVM_ARGS=--jdk-only` at **102 / 102** and `SUITE=all` at **97 / 102** —
the same 102-vector denominator, so both rows are `SUITE=all`, and both include
`RImmutableFactoryTypes`. It is one of the five that fail in the compatible
column. **Same vector, same binary, two modes, two answers**, which is only
possible if strict mode is already taking the real path.

(`H0-2` quotes the strict arm at 104/104; the denominator has grown by two since
the handoff. The argument is about which column the vector fails in, not about
the total.)

This makes `H0-2`'s own parenthesis — *"the `--jdk-only` arm is 104/104 with all
twelve cells below still wrong, because strict mode does not take this path"* —
self-contradictory as written. If strict mode does not take the carrier path,
the cells are not wrong there; what is true is that **the arm does not ask**,
which is `H0-2` N1's point. The two readings are different claims and the
sentence merges them.

### 5b. Under `Compatible`, the kind is discarded

```rust
    pub fn allowed_in(self, mode: CompatibilityMode) -> bool {
        match mode {
            CompatibilityMode::Compatible => true,
            CompatibilityMode::JdkOnly => !matches!(self, NativeKind::SyntheticStub),
        }
    }
```

The only compatible-mode site that reads the kind at all is `invoke_or_native`'s

```rust
    let real_protected_stub = synthetic_stub_native
        && crate::runtime::interpreter::real_protected_stub_class(effective_class);
```

and `real_protected_stub_class_common` is a twelve-entry `matches!` —
`ReentrantLock`, `LinkedBlockingDeque`, `AtomicBoolean`, `EnumSet`,
`ThreadPoolExecutor`, `Instant`, `ZonedDateTime`, `FileInputStream`,
`Cleaner`, `Cleaner$Cleanable`, `ManagementFactory`, `StringJoiner`. **No
`java/util` collection and no `cratonvm/internal/*` class is on it**, and the
other disjunct (`real_bytecode_selector().prefers_real`) is the `CRATONVM_REAL`
env selector, unset by default.

So no tag on any registrar changes what `Map.of` does under `--real-jdk`.

### 5c. The rule, stated so it is not rediscovered

`HANDOFF-20260819.md` §1's trap runs **both ways**:

> Retiring stubs lowers the compatible-mode backlog and moves strict mode by
> exactly zero.
>
> **Retagging `Bridge` → `SyntheticStub` moves compatible mode by exactly
> zero.**

`H0-2` §4 is a compatible-mode measurement, so its remedy must be a
compatible-mode change: retiring the carrier at its producers (`P4A` N1b option
(a), done *properly* — i.e. deleting the natives, not relabelling them), or
option (c). Not a tag. `H0-2` §5's "that work is now unblocked" conclusion
therefore does not follow from the §8 lift.

**Predicted, and falsifiable:** if someone lands the whole map/set retag and
re-runs `AbstractChain.java` under `--real-jdk`, **zero of the twelve cells
move**; under `--jdk-only`, **all twelve are already right today**, before any
change. The cheapest falsification costs one command and no build: run
`AbstractChain` under `--jdk-only` on the *current* binary. If any of the twelve
diverges there, §5a is wrong and I want to know.

---

## 6. What I did NOT move, and why

**Nothing.** No tag moved, no registration changed. The three commits are
comments. Specifically declined, each for a stated reason:

| candidate | reason |
|---|---|
| the ten registrars of §5, whole | §1 — 168 + 45 + 6 producers outside the registry; §2 — four of the ten span other families |
| the HashMap / LinkedHashMap / HashSet family, split per class | §3 — the shared `MAP_KEY_ITR_CARRIERS` are minted for Hashtable-family receivers by a producer that stays `Bridge`; silent empty iteration |
| `register_properties_sidetable` | §4b — reproduces the 2026-07-14 `java.home` bootstrap failure by construction |
| `register_concurrent_hashmap_natives` (the one closed cluster) | its three prerequisites are out-of-file (§8) and the JIT one is a **silent tier divergence**, which is the failure class this record exists to stop. Landing it without the `vm/src/jit/helpers.rs` gate would be the fourth instance in a week of a change whose gate is narrower than its blast radius. Fully specified in-source and in §7b so it is one session's work for whoever has build rights |
| teaching `INSTANCEOF` to read the immutable marker | `H0-2` §5's own argument (second copy of one rule), and §5b makes it unnecessary to litigate: the opcode is not where the defect lives |

---

> **VERIFIED AGAINST A BINARY 2026-09-02.** The banner says "no binary carrying
> these changes has been built or run". §7's runnable checks have now been run.
> Splitting them by what is still falsifiable matters here, because most of §7
> quotes counts that have since moved for reasons this record cannot touch.
>
> **§7a.1 — it compiles.** `cargo check -p cratonvm-native-collections -p
> cratonvm-native-builtins` finishes clean, no errors and no
> `rustdoc::private_intra_doc_links` complaint. That was the real risk in a
> comments-only change: §7a.1 lists nine private intra-doc links and says the fix
> is to drop the brackets if a lint denies them. Nothing denied them.
>
> **§7b — the CHM dial screen passes, and nothing it failed on is CHM's.**
>
> ```text
> CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/concurrent/ConcurrentHashMap
>   85 of 88 passed; failed: RMapGcStress RJdkVarHandleNullCoord RJdkVarHandleModeSupport
> ```
>
> `RMapGcStress` is the long-standing mode-independent failure (`H9-1` §8's own
> standing prediction). The two `RJdkVarHandle*` vectors were **added on
> 2026-09-02** by an unrelated varhandle lane, three weeks after this record.
> Arming the ConcurrentHashMap shadow dial produced **no CHM-related failure**,
> which is what §7b exists to screen for.
>
> **§7a.2 and §7a.3 are STALE and cannot be re-checked**, and re-freezing them
> here would be worse than saying so:
>
> ```text
>                                 this record        2026-09-02
> stub ratchet, mgmt / no-mgmt    1626 / 1615        1645 / 1634
> rows, mgmt / no-mgmt           13225 / 12857      13897 / 13529
> --jdk-only                      102 / 102          119 / 125
> SUITE=core                       61 /  62           81 /  85   (88 by the last run)
> ```
>
> §7a.2's instruction is "Registry must be UNCHANGED ... A comment cannot move
> any of these; if one moves, the cause is elsewhere in the merge, not here."
> Every number moved, and the cause IS elsewhere: three weeks of other lanes
> adding registrations and vectors. The check was sound and is now unmeasurable —
> the same expiry as `H3-1` §5's `−7` and `W7-30` §11's `+8`. What survives is
> that the screen is green apart from failures with named, unrelated owners.

## 7. VERIFICATION PLAN

### 7a. For what actually landed (comments only)

1. `cargo check -p cratonvm-native-collections -p cratonvm-native-builtins`.
   **Note the standing rule: `cargo check -p X` builds the LIB only.** The doc
   comments added here use intra-doc links to items in the same module
   (`native_chm_get`, `native_chm_values`, `native_chm_entry_set`,
   `key_itr_carrier_for`, `native_hs_iterator`, `MAP_KEY_ITR_CARRIERS`,
   `MAP_VIEW_CARRIERS`, `register_set_view_carrier_natives`,
   `register_map_view_carrier_natives`) — all private, all in scope. If any lint
   config denies `rustdoc::private_intra_doc_links`, these are the lines to
   look at and the fix is to drop the brackets.
2. **Registry must be UNCHANGED.** As re-frozen by `e6d642f3b`
   (`native-builtins/tests/stub_ratchet.rs`): stubs 1626 management / 1615
   no-management, rows 13225 / 12857. No row's `kind` may differ, and
   `regression-suite/bridge-ratchet.sh`'s two-column output must be identical.
   A comment cannot move any of these; if one moves, the cause is elsewhere in
   the merge, not here.
3. All three arms verdict-identical to `HANDOFF-20260819.md` §6:
   `--jdk-only` 102/102, `SUITE=all` 97/102 with the same five,
   `SUITE=core` 61/62.

**Anything else is a regression caused by a comment, which would itself be the
finding.**

### 7b. The next step, specified so it needs no re-derivation

**Pre-flight, no build required.** `HANDOFF-20260819.md` §2's dial gives a free
proxy for a refusal on every row that has bytecode:

```bash
CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/concurrent/ConcurrentHashMap \
  bash regression-suite/run.sh      # 36-vector screen, current binary
```

Baseline for that screen is 36/36 (`HANDOFF-20260819.md` §2). The dial and a
refusal agree wherever bytecode exists and differ only on abstract-interface
rows — which is exactly the `java/util/concurrent/ConcurrentMap` block the
change is told to keep `Bridge`. **A red screen here kills the CHM cluster for
one command.** Repeat for `java/util/HashMap`, `java/util/HashSet`,
`java/util/LinkedHashMap` before believing anything about those families; note
`HANDOFF-20260819.md` §2 already reports `all` at **1/36**, so `java/util/` is
not in the clean band and a green per-class screen would itself be news.

**If the CHM cluster is then built** (the only family closed under minting —
see the cluster note on `register_concurrent_hashmap_natives`), the registry
rows to diff and their expected `kind`:

| class | rows | before | after |
|---|---|---|---|
| `java/util/concurrent/ConcurrentHashMap` | all but the `ConcurrentMap` block | `bridge` | `synthetic-stub` |
| `java/util/concurrent/ConcurrentHashMap$KeySetView` | all | `bridge` | `synthetic-stub` |
| `java/util/concurrent/ConcurrentHashMap$ValuesView` | the `MAP_VIEW_CARRIERS` rows | `bridge` | `synthetic-stub` |
| `java/util/concurrent/ConcurrentHashMap$EntrySetView` | the `SET_VIEW_CARRIERS` rows | `bridge` | `synthetic-stub` |
| **`java/util/concurrent/ConcurrentMap`** | **5** (`size`,`get`,`put`,`remove`,`containsKey`) | `bridge` | **`bridge` — must NOT move** |
| `java/util/HashMap`, `$KeySet`, `$Values`, `$EntrySet`, `$KeyIterator`, `$EntryIterator` | all | `bridge` | **`bridge` — must NOT move** |

`G85-1`'s three-part rule applies and the third part is the one that gets
skipped: the tag moved (dump), the behaviour holds (three arms), **and the
surface was exercised** (`invocations > 0` on the moved rows in the *pristine*
dump — `G33-1`: `invocations == 0` proves nothing, and `class-not-loaded` is the
absence of a verdict).

**Vectors expected to flip: none.** `RChmKeySetView` passes today and must still
pass; the gain is shadow retirement, not a green. Per `HANDOFF-20260819.md` §6
the acceptance criterion is **verdict-neutral, not green**.

### 7c. The twelve cells — the two predictions I want measured separately

Re-create the probe (30 lines; the HotSpot column is `H0-2` §4). 22 receivers ×
9 type tests.

| run | prediction | falsified by |
|---|---|---|
| `AbstractChain` under `--jdk-only`, **current binary, no change** | **all twelve cells already MATCH HotSpot** | any divergence — which would mean a producer of `cratonvm/internal/Unmodifiable*` is still live in strict mode and §5a missed it |
| `AbstractChain` under `--real-jdk`, after **any** retag of the map/set cluster | **zero of the twelve move** (all still divergent) | any cell flipping — which would mean a compatible-mode path reads `NativeKind` that §5b did not find |

These are different claims from `H0-2` §5's "all twelve come right", and both
are one command each.

---

## 8. OUT-OF-FILE EDITS REQUIRED

None for what landed. The following are required **before** any map/set retag,
and none is in a file this lane owns.

**O1 — `vm/src/jit/helpers.rs`, gate or delete the six direct helpers.**
Blocking for the CHM cluster (`native_chm_get`, line 13059) and for any HashMap
move (`native_hashmap_get_exact` 13185/13541, `native_hashmap_put_exact` 13538,
`jit_overlay_hashmap_get` 13164, `jit_overlay_hashmap_put` 13353). Line numbers
post-merge at `e6d642f3b`; this file moved by ~100 lines in the H wave, so
anchor on `jit_concurrent_hashmap_get_direct`.

Current text, `jit_concurrent_hashmap_get_direct`:

```rust
            let result = {
                let mut ctx = crate::vm::NativeContextImpl {
                    shared: vm,
                    thread: &mut *thread,
                };
                cratonvm_native_collections::native_chm_get(&mut ctx, &values)
            };
```

The correct shape is a guard at the **bind** site, not here: the helper must not
be direct-bound when the receiver class's natives were refused. The predicate
already exists in kind form — the binder should decline when
`find_with_kind(class, method, desc)` yields nothing, which under `--jdk-only`
is exactly the post-refusal state. Stated as the requirement rather than as a
patch, because the bind site is in a file I could not read into and a wrong
patch here is worse than none.

**O2 — the 45 + 19 + 4 + 3 `try_alloc_concurrent_synthetic` producers**
(`native-builtins/src/{phases_early,phases_late/*,locale_resources,logging_shims,antlr_intrinsics,reflect_annotations,test_frameworks,http2}.rs`)
must be migrated to real construction —
`ctx.new_object_initialized("java/util/HashMap", "()V", &[])` plus
`invoke_virtual("put", …)` — before their family's tag moves. This is the actual
content of `G88-1` N3 and it is a multi-session migration, per file, each with
its own vector.

**O3 — `native-builtins/src/jca/provider_chain.rs`** reads
`Hashtable.table`/`Hashtable.count` by field (lines 285, 448). Part of the
Properties cluster; must be read from the real layout before Properties moves.

---

## 9. NOMINATIONS

**N1 — the cluster map belongs in `regression-suite/probes/cluster-map.py`, and
it needs a third axis.** `G88-1` N4 built it from a registry dump and noted the
per-file join over-merges. The deeper gap is that a registry dump **cannot see
any of §1's three populations** — they never register. Adding a static pass that
harvests `cratonvm_native_collections::native_*` call sites and
`try_alloc_concurrent_synthetic(_, "<class>", _)` sites per class would make the
map say "cluster C has R registrations, D direct Rust writers and A synthetic
allocators", which is the number that decides whether C is movable. Two greps
and a join; it is the instrument this record had to be instead of.

**N2 — the ConcurrentHashMap cluster is the one to do first, and it is
specified.** Closed under minting, its container's real `<init>` body is empty
(so a `try_alloc_concurrent_synthetic` object is a legitimate fresh CHM to real
bytecode, unlike `HashMap` whose ctor must set `loadFactor`), and it has exactly
one out-of-file blocker (O1). Full prerequisites are written at
`register_concurrent_hashmap_natives` in the tree.

**N3 — give the Hashtable family its own iterator carrier.**
`key_itr_carrier_for`'s `(false, false)` arm answers `HashMap$KeyIterator` for a
`Hashtable$KeySet` receiver, where HotSpot answers `java.util.Hashtable$Enumerator`.
This is a live `getClass()` divergence *today*, independent of any retag, and
fixing it decouples §3. `native-builtins/src/deprecated_util.rs` already carries
`HASHTABLE_ENUMERATOR_CLASS` and `HASHTABLE_ENUMERATOR_CTOR`.

**N4 — `RJdkBridge1`'s `Properties.equals`/`isEmpty` complaint is a correctness
bug, not a cluster bug, and can be fixed today — but not the way its own text
says.** JDK 25's `Properties` **declares** `size`, `isEmpty`, `equals` and
`hashCode` and delegates all four to its private `ConcurrentHashMap map` (§4c
quotes the source). So the oracle is "answer exactly what an equivalent
`ConcurrentHashMap` of the string entries would answer, `defaults` invisible" —
which is what the vector wants and is *not* what "they are Hashtable's" would
lead a fixer to implement. Sites: `native_properties_equals`,
`native_properties_is_empty` and the `size` row in
`properties_sidetable.rs`. `G88-1` §6 read this failure as evidence that the
cluster spans two crates; it is evidence that three natives have the wrong
contract. **Both may be true, but the second is one afternoon and does not need
the cluster.** Add `hashCode` to the probe while there — nothing has asked it.

**N5 — `H0-2` N1 should land before any cluster change.** Extending
`RImmutableFactoryTypes` by `instanceof AbstractCollection` and
`instanceof RandomAccess` is nine lines, and until it exists the arms cannot see
eleven of twelve cells — so §7c's second prediction has to be measured with an
ad-hoc probe rather than by the suite.

---

## 10. Where I found an existing record or the brief wrong about the tree

Each with the grep that settles it.

**10a. `G88-1` §6 — "the larger half lives in `native-builtins/src/lib.rs`",
the crate/file conflation.** That file holds **zero** class-position
`java/util/Properties` or `java/util/Hashtable` registrations (§4a's grep). The
crate ratio 67/49 is right; the attribution is not; and the §8 lift therefore
does not unblock this row. `HANDOFF-20260819.md` §1 states the general
correction (*"It names one file, not the crate"*) — the two documents were
already inconsistent before this lane.

**10b. `H0-2` §5 — "that work is now unblocked … all twelve cells come right".**
Neither half survives §5. Strict mode already answers all twelve from real
bytecode (three explicit `SyntheticStub` sites, corroborated by
`--jdk-only` 102/102 vs `SUITE=all` 97/102 on the same vector), and compatible
mode discards the kind (`allowed_in(Compatible) => true`, twelve-entry
allow-list with no `java/util` collection on it).

**10c. `H0-2`'s framing sentence — "the `--jdk-only` arm is 104/104 with all
twelve cells below still wrong, because strict mode does not take this path"** —
is self-contradictory. If strict mode does not take the carrier path, the cells
are not wrong there. The true statement is `H0-2` N1's: the arm does not ask.

**10d. My own brief — "you are in your OWN isolated git worktree branched from
`claude/jdk-only-mode-handoff-09b48c`". It was not.** This worktree was created
at `26e4b5db4`, the **dev tip**, so `H0-1`, `H0-2`, `H1-1`, `H2-1` and `H3-1`
were absent and `H0-2` — whose §4 is half my acceptance criteria — had to be
read with `git show 56e5346df:…`. Lane H6 hit the same stale base in parallel,
so this is a **worktree-creation defect, not a one-off**: two of two lanes
checked were affected. Resolved here by merging
`claude/jdk-only-mode-handoff-09b48c` (`e6d642f3b`) after the second commit;
clean, no conflicts, and `git diff 26e4b5db4 e6d642f3b -- native-collections
native-builtins/src/lib.rs native-builtins/src/properties_sidetable.rs` touches
none of the three files this lane edited. Every §1 count was re-taken on the
merged tree and is unchanged.

The trap worth naming: **`native-builtins/tests/stub_ratchet.rs` does not parse
at `26e4b5db4`** (H3-1 repaired it), so any lane on the stale base that opened
that file would have been reading a file the compiler rejects. Nothing in this
record depends on it.

**10e. The brief's "§5's list of ten … not a guarantee it is still ten".** It is
still exactly ten, all present, all with their own `set_category` at their head
(§2's table) — so the `G85-1` inert-retag trap does **not** apply to any of
them, which is the one piece of good news in this record. An eleventh
(`register_map_conditional_mutators`) moves implicitly with its callers and is
absent from §5's list.

**10f. `G88-1` §5's claim that the two still-failing vectors are "outside the
cluster, failing the same way for the same reason one level out".** Half right.
`RJdkBridge1`'s failure is inside a native's *contract* (N4), not outside the
cluster's *boundary*; and §3 shows the Hashtable half is not "one level out" at
all — it shares the HashMap family's iterator carriers.

**10g. `RJdkBridge1`'s own failure text — "equals/isEmpty are Hashtable's".**
They are not, on JDK 25: `Properties` declares `size`, `isEmpty`, `equals` and
`hashCode` itself and delegates all four to its private `ConcurrentHashMap map`
(§4c quotes the image). The assertion the vector makes is right; the mechanism
it names would send a fixer to the wrong registrar. A *vector text* correction
rather than a record one, and it costs four words.

**10h. And one thing I asserted and then had to withdraw**, recorded because
the correction is the useful part. I first wrote §4c as *"`Properties` declares
no `equals`, `isEmpty` or `size`; all three are `Hashtable`'s"* — reasoning from
`Properties extends Hashtable` and from the vector's own wording, without
opening the image. `javap -p java.util.Properties` says otherwise in one line,
and the source says why (JDK 9 moved `Properties` off `Hashtable`'s table onto
a private `ConcurrentHashMap`). **A supertype relationship is not a
declaration**, and a record's quoted failure text is not an oracle for the JDK.
