# Fabricated object layouts leak into native code — index-based field access breaks silently when the class becomes real

**Status:** OPEN — JDK-only wave-2 work item, filed 2026-07-31, re-verified
against the re-landed tree the same day. **DANGEROUS: every instance is a
silent wrong-field read or write, never an exception.**

> **Evidence provenance.** The `drop_real_layout_synthetic` doc and all ten
> `JDK-ONLY-LAYOUT` markers are pre-existing or re-landed code and were read
> from `C:\craton\wt-jdk-only` (branch `feat/jdk-only-mode`) on 2026-07-31.
> Two corrections to the original filing are folded in below: the drop list
> names **six** drift families, not five, and the marker table's per-file
> verdict split was slightly off.

## What changed on 2026-08-04 — step 2 of four

*What specifically must change* lists four steps. Step 2 — **adjudicate the two
`unknown` verdicts in `vm/src/vm/vm_object.rs`** — is now partly answered, with
evidence rather than an argument.

Both are **overlays**, not mis-numbered slots: a VM-internal `Int` deliberately
written on top of `java.lang.Class`'s instance field 0, which JDK 25 declares as
`Constructor<T> cachedConstructor` — a *reference* slot. The question was never
"is this the right slot" but "does writing an `Int` where the image declares a
reference corrupt anything". The marker listed three checks. Two are run,
against a real JDK 21 image, and both are clean:

1. Three rounds of `getDeclaredConstructor()` on a nested class, interleaved
   with `String.class.getConstructor(String.class)` — so the second and third
   take the real bytecode's `cachedConstructor != null` fast path — all returned
   the right `Constructor`. Nothing raised, and no `expected object reference,
   got int(N)`.
2. The same run under `CRATONVM_DBG=overlay`, whose hunter
   (`overlay_write_is_destructive`) exists precisely to report a primitive
   written to a reference slot, reported nothing.

So the verdict stays `unknown` but **drops from ranked-HIGH**: the two checks
that would have shown live harm did not.

The third check — does anything still *depend* on the overlay — is the one whose
answer removes code rather than reassuring about it, and nothing in the tree
could answer it. `mirror_class_id` (`native-builtins/src/lang_class.rs`) is the
overlay's only reader outside the VM, a fallback behind the reverse map, and it
now reports its first hit under the same flag. **One broad real-JDK run makes
the verdict decidable.** A ten-class probe does not: silence over a small
workload is not silence over Spring Boot, and the marker says so rather than
inviting a deletion on thin evidence.

The primitive-mirror sibling (`Int(-1)` over the same slot) rides on that
finding: it is the easier of the two to retire if check 3 comes back zero,
because a primitive mirror has no legitimate `cachedConstructor` reader at all.

## What is still open — steps 1, 3 and 4, which are the bulk

* **Step 1, the sweep — it has a measured work list now (2026-08-04).** The
  sweep was scoped as "read four crates for index-based field access". It does
  not need reading first: **the runtime detector for exactly this defect already
  exists** and had never been run broadly. `CRATONVM_DBG_OVERLAY=1` reports a
  native writing a primitive to a reference slot *or* a reference to a primitive
  slot on a class loaded from real JDK bytes. Add `CRATONVM_DBG_OVERLAY_ALL=1`
  or the `java.util.Map` suppression hides the dominant family.

  Three probes, both modes, JDK 25. **The results are identical under
  `--real-jdk` and `--jdk-only`**, so this is a `Compatible`-mode defect too.
  Distinct `(class, slot, value kind, real descriptor)` sites. **The two
  `VarHandle` rows are struck through: fixed the same day, and the re-run
  confirms they are gone — 13 classes / 24 slots became 11 / 21.**

  | class | slot | writes | real desc | n |
  |---|---:|---|---|---:|
  | ~~`java/util/HashMap`~~ | ~~1~~ | ~~`Int`~~ | ~~`L`~~ | **FIXED 2026-08-05** — 8,342 → 0, the largest row in this table |
  | `java/util/HashMap$Node` | 2 | `Int` | `L` | 2,108 |
  | ~~`java/lang/invoke/VarHandle`~~ | ~~1~~ | ~~`Object`~~ | ~~`Z`~~ | **FIXED** |
  | ~~`java/lang/invoke/VarHandle`~~ | ~~0~~ | ~~`Int`~~ | ~~`L`~~ | **FIXED** |
  | ~~`java/util/HashMap`~~ | ~~2~~ | ~~`Int`~~ | ~~`[`~~ | **FIXED (L2, 2026-08-04)** |
  | ~~`java/lang/invoke/MemberName`~~ | ~~4~~ | ~~`Int`~~ | ~~`L`~~ | **FIXED (L3, 2026-08-05)** |
  | ~~`java/util/Properties`~~ | ~~7~~ | ~~`Float`~~ | ~~`L`~~ | **FIXED** |
  | ~~`java/util/Properties`~~ | ~~6, 5~~ | ~~`Int`~~ | ~~`L`~~ | **FIXED** |
  | ~~`java/util/Properties`~~ | ~~2~~ | ~~`Object`~~ | ~~`I`~~ | **FIXED (L2, 2026-08-04)** |
  | ~~`java/util/Properties`~~ | ~~3~~ | ~~`Object`~~ | ~~`F`~~ | **FIXED (L2)** — never in this table: no probe called `new Properties(defaults)` until 2026-08-04 |
  | ~~`ClassLoaders$PlatformClassLoader`~~ | ~~0, 3, 4, 6~~ | ~~`Int`~~ | ~~`L`~~ | **FIXED (L1, 2026-08-05)** |
  | ~~`ClassLoaders$AppClassLoader`~~ | ~~0, 3, 4, 6~~ | ~~`Int`~~ | ~~`L`~~ | **FIXED (L1, 2026-08-05)** |
  | ~~`java/util/Scanner`~~ | ~~3, 4~~ | ~~`Int`~~ | ~~`L`~~ | **FIXED (L3, 2026-08-05)** — and three more slots on the same class that this table never listed |
  | `java/net/URI` | 5 | `Object` | `I` | 2 |
  | `java/net/URI` | 2 | `Int` | `L` | 2 |
  | `java/net/Proxy` | 0 | `Int` | `L` | 2 |
  | `jdk/internal/math/FloatingDecimal$1` | 0 | `Object(None)` | `I` | 2 — **new 2026-08-05 (L4 gap 3)** |
  | `ReentrantReadWriteLock$Sync$ThreadLocalHoldCounter` | 0 | `Object(None)` | `I` | 2 — **new 2026-08-05 (L4 gap 3)** |

  **This table is the WRITE half, and after L4 it is the smaller half.** The
  widened detector's 135 sites include 70 reads and a separate per-class
  shadow-layout census whose rows are not `(value kind, real desc)` tuples at
  all; the section
  [below](#the-detector-was-widened-2026-08-05-and-the-number-went-up) carries
  them rather than stretching this table into a shape it was not built for.

  **13 classes, 24 distinct slots**, from three small probes. Reading it:

  > **Re-measured 2026-08-04 for lane L2**, with the three standing probes
  > plus `probes/MapLayoutMatrixProbe`: the tree BEFORE L2 shows **6 classes /
  > 15 slots**, and after L2 **5 classes / 12 slots**, with the `Properties`
  > family gone entirely. That is not comparable to the 13/24 above, and the
  > difference is not all L2's: these probes do not reproduce the
  > `HashMap$Node`, `URI` or `Proxy` rows at all — either something else fixed
  > them or nothing in this probe set exercises them. Only the three rows
  > named under `Properties` below are attributed to L2, each A/B'd on the
  > same workload against the pre-fix binary.

  * ~~The **`HashMap` family is the known-benign case**~~ — **half true, and
    the half that was false was the expensive half. FIXED 2026-08-05.**
    Coercion-to-null does land the real bytecode in the null-initialised state
    it expects, which is why slot 1 (`AbstractMap.values`) was harmless in
    OUTCOME. What that reasoning hid is the slot NEXT to it: the same
    fabricated model puts the bucket array in slot 0, `AbstractMap.keySet` —
    reference over reference, so no census could ever report it, and no
    coercion made it benign.

    `probes/MapModelSlotProbe` reads the field reflectively under
    `--add-opens java.base/java.util=ALL-UNNAMED` and settled it against
    HotSpot 25.0.3+9:

    | | HotSpot | CratonVM (pre-fix) |
    |---|---|---|
    | `keySet` on a fresh/filled/copied/sized map | `null` | `ARRAY[Object]` |
    | after `keySet()` was called | `HashMap$KeySet` | still `ARRAY[Object]` |
    | `values` | `null` | `null` (the coercion — genuinely benign) |

    The second row is the failure mode: real `HashMap.keySet()` is
    `if (ks == null) ks = new KeySet(); return ks`, so a non-null bucket array
    short-circuits the lazy init and hands the caller an `Object[]` where a
    `Set` is required. It never faulted because the natives shadow every
    reader — the same conditional safety `VarHandle` had, and item 3/7 is in
    the business of removing that shadow.

    Fixed in `fix/map-model-slots-on-real-layout-20260805`: `map_buckets_slot`
    and `map_size_slot` answer "where does THIS receiver keep its table / its
    count", and read and write both go through them, so they cannot drift. The
    slot-1 row went 8,342 → 0, `keySet` is `null` on every probe row, the
    `test_classes` corpus is byte-identical in `Compatible` mode, and
    `bench/HashMapOnly` at n=5M shows no regression (median 2956 → 2940 ms,
    min 2872 → 2575) because the change also removes a by-NAME class lookup
    per size access.

    **The lesson generalises past this row.** "Benign by coercion" is a claim
    about one slot's value kind, and it was used to wave off a whole family;
    the adjacent slot in the same model had no coercion to make it benign and
    no instrument that could see it. When a model is written over a real
    layout, ask what EVERY slot of the model lands on — `javap -p` and a
    reflective probe, not the census.
  * **`VarHandle` — FIXED 2026-08-04, and it was the worst of the set.** It
    mismatched in *both* directions on adjacent slots (`Int` over a reference at
    0, an `Object` over a `boolean` at 1). The frames say why that mattered:
    `MhUtil.findVarHandle`, reached from the `<clinit>` of
    `java.util.concurrent.atomic.AtomicBoolean`, `AtomicReference` and
    `java.io.ObjectInputFilter$Config`. Those are **real JDK classes whose
    `static final VarHandle` fields real bytecode uses**, and slot 0 on a real
    `VarHandle` is `vform` — so the VM was handing the JDK a `VarHandle` with a
    null `VarForm`. It did not fault only because our natives intercept every
    `VarHandle` operation and read the WP4.2 side table; the moment §7 step 3
    routes one of those to real bytecode — the direction this whole feature is
    going, on a path that already fires 3,344 times per run —
    `vform.getMethodHandle(…)` is an NPE.

    Fixed by writing the six synthetic slots only when the object actually has
    our layout. No metadata is lost: `vh_meta_put` runs on every allocation path
    and every reader consults it first.

    **The first attempt at that guard was inert and nearly shipped.** It tested
    `object_num_fields(vh) >= VH_FIELD_COUNT`, but `alloc_concurrent_synthetic`
    returns at least the requested slot count either way, so a count test cannot
    separate the layouts. It was caught by A/B against the pre-fix binary — 8
    writes before, 8 after — after a first reading compared a six-run aggregate
    (52) with a single run (8) and mistook the difference for a fix. The working
    predicate asks by **name**: a real `VarHandle` declares an instance field
    called `vform` and a fabricated stub does not. Verified 8 → 0 on the same
    probe, with both probes still byte-identical to HotSpot in both modes.

    Two lessons for the remaining 22 sites. **Field count does not identify a
    layout** — ask for a field the real class declares and the stub cannot.
    And **A/B the same workload against the pre-fix binary**; an aggregate and a
    single run are not comparable numbers, however much they look like a
    before/after.
  * **`Properties` — diagnosed 2026-08-04, not yet fixed, and the fix shape is
    already in the same file.** The frames put all four writes at
    `new Properties()`, and the values name themselves: slot 5 `Int(0)`, slot 6
    `Int(12)`, slot 7 `Float(0.75)` — `count`, `threshold` and `loadFactor`,
    i.e. `native_map_init` writing a `HashMap`-shaped layout by raw slot index
    onto a real `java.util.Properties`, where those slots are `defaults`/`map`
    references and slot 2 is an `int`. It is in the group item 1 calls the
    highest-risk in `native-collections`, and it is on the bootstrap path.

    `native-collections/src/lib.rs` already demonstrates the correct pattern
    twice, so this does not need inventing:
    `native_props_init_defaults` resolves `defaults` with
    `ctx.resolve_field_index("java/util/Properties", "defaults")` and writes
    *both* the model slot and the real one, with a comment explaining that on a
    real layout the inherited `Hashtable` fields push `defaults` onto
    `loadFactor`; and `try_set_jdk_map_field(ctx, this, "loadFactor", …)` is the
    by-name setter. `native_map_init` is the one still writing raw indices.

    **Three of the four rows FIXED 2026-08-04, and the root cause was one
    line.** `try_set_jdk_map_field` resolved every field name against a
    hard-coded `"java/util/HashMap"` and then wrote that index into `this`,
    whatever class `this` actually was. For a non-`HashMap` receiver the index
    names a *different field*. Its `slot < object_num_fields(this)` bound does
    not help: it stops an out-of-range write, not a wrong-field one — **the
    third guard in this file's story that looks protective and is not**, after
    the frozen divergence test and the field-count `VarHandle` predicate.

    Fixed with `resolve_field_index_by_class_id`, which walks the receiver's
    own hierarchy, so `loadFactor` on a `Properties` resolves through
    `Hashtable` to its true slot; the API's own doc comment already recommended
    it over the name-based form when the caller holds the object. Where the
    receiver's class does not declare the field, nothing is written.

    A/B on the same probe against the pre-fix binary: slots 5, 6 and 7 go 3 → 0
    each, every other row byte-identical, both probes still identical to
    HotSpot 25 in both modes, and 94 `native-collections` unit tests plus the
    four ratchets green. This changes `Compatible` mode too — from *writes the
    wrong field* to *writes the right field or none* — which is why it was
    A/B'd separately rather than riding on the `VarHandle` verification.

    ~~**Slot 2 survives**~~ — **FIXED 2026-08-04 (lane L2), together with the
    `HashMap` slot-2 row and a `Properties` slot-3 row this table never
    listed.** It came from the raw `MAP_FIELD_*` writes in `native_map_init`'s
    legacy branch, not from `try_set_jdk_map_field`, and the same fixed-class
    lookup was in four more places: `map_resize`, `resync_view_set`,
    `map_state`'s bucket fallback and `hashmap_serialized_capacity`.

    The shape is the one this record keeps describing:
    `resolve_field_index("java/util/HashMap", "table")` answers `2`, and index
    2 on a real `Properties` is the inherited `Hashtable.threshold`, an `int`.
    `receiver_table_slot` asks the receiver's own class instead, and
    `publish_map_table` is now the single idiom for publishing a bucket table:
    slot 0 always (the natives read it), the receiver's real `table` when that
    is a different slot, and the legacy `Int(capacity)` at slot 2 **only when
    the receiver has no `table` field at all** — which is what "our fabricated
    layout" means, asked by NAME. A slot count cannot answer it: the
    allocators size a fabricated map to at least `MAP_NUM_FIELDS` and a real
    one to at least its real field count, and the fabricated
    `cratonvm/util/MapViewBacking` is WIDER than a real `HashMap`.

    **The `HashMap` family did change, deliberately.** The sites that wrote
    `Int(cap)` at absolute slot 2 unconditionally — `new HashMap<>(map)`,
    `Map.of`, `Set.of`, `ConcurrentHashMap.newKeySet`, the `HashSet` backings —
    were writing an int into the REAL `table` field of a real-layout receiver,
    while `native_map_init` right next door already stored the bucket array
    there. They now agree. `map_state` derives capacity from the bucket
    array's length and only falls back to slot 2 when there is no array, so
    nothing reads what was dropped.

    A/B, pre-fix vs post-fix binary, four probes × both modes, JDK 25:

    | row | pre | post |
    |---|---:|---:|
    | `java/util/Properties` slot 2 `Object` over `I` | 40 | **0** |
    | `java/util/Properties` slot 3 `Object` over `F` | 12 | **0** |
    | `java/util/HashMap` slot 2 `Int` over `[` | 66 | **0** |
    | every other row | — | byte-identical |

    Measured twice, on Windows and on Azure Linux, against the same Temurin
    25.0.3 image and with each host's own pre-fix binary: the three rows above
    are identical on both, as is every other row. On `JdkOnlyCensusLoadProbe`
    alone the `Properties` slot-2 row goes **3 → 0**, which is the number the
    lane doc predicted. The benign `HashMap`
    slot-1 row moved 8,256 → 8,318; that is *not* this change — it is the
    `stringPropertyNames` fix below building one more set per call, and the
    row's own run-to-run spread on an unmodified binary is ±4 on a single
    probe (1634/1636/1638 over three runs), so it is not a stable signal at
    that resolution either way. Probe transcripts are identical pre/post
    except an ephemeral TCP port and a timing line; the 10-member
    `test_classes` corpus is identical in `Compatible` mode with timestamps
    normalised.

    ~~Note also that `native_props_init` writes `Value::Object(None)`~~ —
    **also fixed (L2 step 3)**, and the note stands as written about the
    detector: `overlay_write_is_destructive` only flags `Object(Some(_))` over
    a primitive, so the null write to slot 3 (= `loadFactor`) never appeared
    in any census and still would not. What DID appear, once a probe finally
    called `new Properties(defaults)`, is the sibling `Object(Some(_))` write
    from `native_props_init_defaults` — 12 hits, now 0. Both writers and every
    reader of the chain link now go through `props_defaults_slot`, which
    resolves `defaults` on the receiver and falls back to the model slot only
    for a receiver that does not declare the field. **The `Object(None)` half
    is verified by unit test, not by the census** — L4 gap 3 is what would
    make it visible, and it is still open.
  * **Both built-in class loaders — FIXED 2026-08-05 (lane L1). Kind 3, the
    one neither earlier fix reaches.** `alloc_classloader` wrote CratonVM's
    seven-slot loader model onto the object, and four of those slots were
    `Int`:

    | slot | synthetic meaning | real `ClassLoaders$AppClassLoader` |
    |---:|---|---|
    | 0 | `CL_LOADER_TYPE` | `parent`, a `ClassLoader` |
    | 3 | `CL_CLASSES_LOADED` | `nameAndId`, a `String` |
    | 4 | `CL_IS_PARALLEL_CAPABLE` | `parallelLockMap`, a `ConcurrentHashMap` |
    | 6 | `CL_LOADER_ID` | `classes`, an `ArrayList` |

    Reached from `Thread.currentThread()` → `current_thread_object` →
    `get_or_create_system_cl` while initialising `contextClassLoader`, which is
    why the Java stack said `BufferedWriter.initialBufferSize()` and why
    grepping found nothing. Named by `CRATONVM_DBG=overlay-bt`, which exists
    because of this site.

    **`resolve_field_index_by_class_id` cannot fix these.** `loadFactor` on a
    `Properties` has a real counterpart to resolve to; `CL_LOADER_TYPE` and
    `CL_LOADER_ID` are VM-internal bookkeeping with **no real JDK field at
    all**. There is nowhere correct to put them in a real loader's layout, so
    on a real image they must not be in the object.

    The fix is a `LoaderMeta` side table keyed by the loader OBJECT
    (`classloader.rs`), plus a `cl_has_synthetic_layout` predicate that gates
    every slot write and every raw-slot READ fallback. Two deliberate
    departures from the `vh_meta_*` shape it copies, both forced by facts about
    loaders: the key is the object, not `identity_hash_code` (an
    address-derived hash recurs once a collection reuses the region, which is
    why `loader_namespace_id_store` had already moved off it), and the table is
    NOT a GC root (rooting user loaders would pin every one of them and defeat
    loader unloading). It is pruned and remapped by
    `gc_reconcile_defining_loaders` alongside the namespace store.

    **Measured, A/B against the pre-fix binary, JDK 25, Azure Linux, both
    probes:** the eight `ClassLoaders` rows go **8 → 0** and every other row in
    the table above is byte-identical across the two arms (2,188 / 16 / 7 / 3 /
    1 / 1). `probes/L1LoaderIdentityProbe` output is identical between the arms
    and matches HotSpot 25 on loader identity
    (`X.class.getClassLoader() == getSystemClassLoader()`, the TCCL, and the
    `getParent()` walk).

    **The three REFERENCE slots were the same defect, one kind quieter, and
    are fixed too.** L1's brief said to leave `CL_PARENT_REF` (1),
    `CL_NAME_REF` (2) and `CL_DEFAULT_DOMAIN` (5) alone because "they are
    references with real counterparts and are already written by name too".
    That is a factual error: the by-name write and the index write go to
    DIFFERENT fields. Slot 1 is `name:String` and was receiving the parent
    `ClassLoader`; slot 2 is `unnamedModule:Module` and was receiving the name
    `String`; slot 5 is `package2certs:ConcurrentHashMap` and was receiving a
    `ProtectionDomain`. A reference into a reference slot, so
    `overlay_write_is_destructive` cannot see any of it — this is exactly the
    "same-kind write" blind spot named two paragraphs below, sitting in the
    function this record is about. It was live: `classloader_parent` falls back
    to slot 1 whenever the by-name `parent` is null, which is the platform
    loader's case, so **it returned the platform loader's own name String as
    its parent** to every caller that walks the chain
    (`builtin_loader_reachable`, `parent_namespace_id`, Tomcat's
    `while (j.getParent() != null)`).
  * **`Scanner` and `MemberName` — FIXED 2026-08-05 (lane L3), and between them
    they are two more instances of this record's own warning that the table is a
    floor.**

    `MemberName` slot 4 is **kind 3**, and the unusual sub-case where kind 3
    needs no side table. `vmindex` is `@Injected` in HotSpot — the class file
    declares no field for it — and index 4 is `method`, a `ResolvedMethodName`.
    Four natives wrote an `Int(1)` "resolved" sentinel there
    (`native_mhn_resolve`, `native_mhn_init`, `alloc_resolved_member_name`,
    `lookup_reveal_direct`), and **the value never reached the object in any
    layout**: `coerce_field_value_by_descriptor` maps an `Int` written to an `L`
    slot to `Object(None)`, which is exactly the condition this census reports,
    so the row is its own proof of inertness. Both vmindex readers already
    answered 0. The sentinel is now written only on our fabricated layout, keyed
    by name on the real class's `method` field, and `probes/L3MemberNameProbe`
    is byte-identical pre-fix and post-fix in both modes. A doc comment on the
    write called it "critical" because of a `SplitConstantPool`
    `ConstantPoolException("Bad CP index: 0")`; whatever that was true of, it
    cannot have been this write.

    `Scanner` is **kind 2 for four slots and kind 3 for the fifth**, and the
    writer was not where the lane brief said. `overlay-bt` named
    `native_scanner_init_string` in `native-io/src/lib.rs` — a FIVE-slot model —
    while the brief was written from a THREE-slot model in
    `native-builtins/src/phases_early.rs` that is dead in every build
    configuration (registered from `register_synthetic_overrides` at
    `vm_init.rs:1426` and overwritten by `register_io_natives` at 1428; all 35
    live `java/util/Scanner` registry entries name `native-io`). The dead copy
    is deleted.

    **Two of the five wrong writes were in this table; three were invisible.**
    Against `javap`: model slot 0 (input `String`) landed on `buf`, a
    `CharBuffer`; slot 1 (position) on `position`, right by coincidence; slot 2
    (delimiter `Pattern`) on `matcher`. Slots 0 and 2 are reference-over-
    reference — kind 5, which `overlay_write_is_destructive` cannot see. The
    two rows that WERE visible were also lossy, not merely misplaced:
    `useRadix(16)` was coerced to null, so `radix()` always answered 10 and
    `nextInt()` on `"ff"` threw `InputMismatchException`. `position`,
    `delimPattern`, `radix` and `closed` now resolve by name on the receiver;
    the input text, which has no real counterpart, moved to an identity-keyed
    side table.

    A/B on the pre-fix binary, both probes × both modes, JDK 25, Azure Linux:
    `Scanner` slots 3 and 4 go **1 → 0** each and `MemberName` slot 4 **7 → 0**,
    with the benign `HashMap` slot-1 row byte-identical (1651 / 537 in both
    arms) and no other row present in either. `probes/L3ScannerLayoutProbe` and
    `probes/L3MemberNameProbe` are byte-identical to HotSpot 25 in `--real-jdk`
    and `--jdk-only`; the Scanner probe fails on the pre-fix binary, which is
    what makes it evidence rather than decoration.
  * `URI` and `Properties` each mismatch in both directions, which rules out a
    single off-by-one against one layout.

  ~~Two limits, so nobody reads this as complete. The detector covers
  `NativeContextImpl::set_field` only: **reads are uninstrumented, and a
  same-kind wrong-slot write is invisible**~~ — **both closed 2026-08-05 as lane
  L4**, together with the `Object(None)`-over-a-primitive blind spot noted under
  `Properties` above. Three probes is still not Spring Boot; every count here
  remains a floor, for that reason rather than the detector's.

  ### The detector was widened 2026-08-05, and the number went up

  Lane [L4](../../feature-designs/jdk-only-wave2/L4-overlay-detector-blind-spots.md)
  closed the three gaps. Same three probes, both modes, JDK 25, A/B against a
  pre-fix binary built from the same tree:

  | | pre | post |
  |---|---:|---:|
  | distinct `(op, class, slot, value kind, real desc)` sites | 4 | **135** |
  | of which reads | 0 | **70** |
  | modelled classes reached / of those disagreeing | — | 156 / **73** |
  | disagreeing slots | — | **152** (129 by type class, 23 by name) |
  | slots where our model NAMES a different field than the image | — | **23** across 12 classes (10 accessed) |

  Every pre-fix row survives with its count (`MemberName` 50 → 50, both
  `Scanner` rows 2 → 2, `HashMap` slot 1 8,314 → 8,312, inside its own ±4
  spread), which is what makes the two numbers comparable rather than merely
  both large.

  Three things this record must now say differently:

  * **Kind 5 is findable.** The paragraph below that says "no amount of
    re-running the census will find another one" was true of the *old* detector
    and is false of this one. CratonVM's `synthetic_stub_fields` model is diffed
    against the real layout at define time (`classloading/src/shadow_layout.rs`),
    so a slot where our model says `name:String` and the image says
    `parent:ThreadGroup` is reported whether or not any value tag disagrees.
    **`java/lang/ThreadGroup` has both `name`/`parent` and `daemon`/`maxPriority`
    transposed** — the identical shape to the `ClassLoaders` defect L1 found by
    hand, in a class nobody had looked at. So are `java/lang/Thread` slot 5
    (`contextClassLoader` over `holder`), `java/security/ProtectionDomain` 1/2/3,
    `java/security/CodeSource` 1, `java/io/BufferedWriter` / `BufferedReader` /
    `InputStreamReader` / `OutputStreamWriter` slot 0, `java/lang/reflect/Field`
    / `Method` / `Constructor` slots 1/3/4/6, and `Collections$SingletonMap`
    0/1. Each is its own change with its own A/B, and the list is this lane's
    output. **Seven of the twelve are FIXED (2026-08-05)** — `ThreadGroup`,
    `java/lang/Thread`, `java/security/ProtectionDomain`,
    `java/security/CodeSource`, `java/io/BufferedReader`,
    `java/io/BufferedWriter` and `java/util/Collections$SingletonMap`. Five
    remain, and they are the ones that are not model rotations; see
    [§What is left](#what-is-left-and-why-each-one-is-not-a-rotation).

    ### The fourth batch — four more rotations, one root cause each

    * **`CodeSource`** repeated `ProtectionDomain`'s mistake in the class next
      to it: model `(location, certs)`, the `CodeSource(URL, Certificate[])`
      constructor order, against a declared `location, signers, certs, …`. Slot
      0 is `location` either way, which is why the dozen raw
      `get_field(cs, 0)` readers scattered across the tree were all correct and
      only slot 1 was wrong. Two raw slot-1 writes went by-name with it.
    * **`BufferedReader`** and **`BufferedWriter`** named the wrapped stream at
      index 0, where `java.io.Reader` puts `lock` and `java.io.Writer` puts
      `writeBuffer`. Both are at index 2.
    * **`Collections$SingletonMap`** named `k`/`v` at 0/1, where `AbstractMap`
      puts `keySet`/`values`.

    All four are model-only: every writer already went by name. One test now
    pins all six corrected models against the JDK's declaration order in one
    place, so the family cannot drift back one class at a time.

    ### What is left, and why each one is not a rotation

    | class | why it is not just a reorder |
    |---|---|
    | `java/io/InputStreamReader`, `OutputStreamWriter` | the model names `in`/`out`, which the real classes **do not declare at all** — the wrapped stream lives inside `sd:StreamDecoder` / `se:StreamEncoder`. Kind 3 or 4, and `servlet.rs` has raw slot-0 consumers gated to synthetic mode. |
    | `java/lang/reflect/Field`, `Method`, `Constructor` | the models are the real layouts minus the inherited `AccessibleObject`/`Executable` fields, so everything from index 1 shifts — across **58 raw slot accesses**, and `MockNativeContext`'s `mock_jdk_field_slot` encodes a **third** mapping that agrees with neither. |
    | `java/io/BufferedWriter` slot 0 (separate from its model) | `Files.newBufferedWriter` parks an fd `Int` there, i.e. in `Writer.writeBuffer`, and `bw_delegate_out` uses that slot's *value* as a layout discriminator. Kind 3 — wants a side table. Filed as [files-newbufferedwriter-parks-an-fd-in-writebuffer.md](files-newbufferedwriter-parks-an-fd-in-writebuffer.md). |

    ### The third worked example — `ProtectionDomain`, and what the mock hid

    The model was in the JDK **constructor's argument order**,
    `(CodeSource, PermissionCollection, ClassLoader, Principal[])`, while the
    class declares `codesource, classloader, principals, permissions`. Three of
    the four sat at the wrong index, all four are references, and the arm said
    so in as many words: *"Matches the constructor signature … that real JDK
    bytecode targets."* The signature was right and irrelevant. **A constructor
    signature is not a field layout** — worth its own line, because it is a
    plausible-looking way to derive a model and it produced a rotation rather
    than a swap.

    No live defect, again: `populate_protection_domain_fields` wrote slots 0..3
    and then the same four by name, with a comment noting the by-name pass was
    "authoritative — runs last". It was, so the raw pass was dead weight on a
    real layout and redundant on a fabricated one. The probe reads all four
    fields correctly on the pre-fix binary. Fixed by putting the model in
    declaration order and deleting the raw pass, which also deletes an ordering
    dependency nothing could see from the call site.

    **The finding worth carrying forward is in the test infrastructure.**
    Removing the raw pass turned an existing test red, and the reason was that
    `MockNativeContext::set_field_by_name` silently resolved `None` for any
    class with a fabricated model but no hand-written `mock_*_field_slot`
    helper. So under the mock, **the by-name half of every dual write in
    `native-builtins` did nothing** — every such native was tested on its raw
    half only, the half that is wrong precisely when the model and the image
    disagree. The same test also expected a `String` at `CodeSource` slot 0, a
    value the VM has not produced for as long as the by-name write beside it
    has existed. Both were artefacts of the blind spot.

    The mock's read and write paths now fall back to the production
    `synthetic_stub_field_model`; the third entry point,
    `resolve_field_index_by_class_id`, is deliberately left alone because
    wiring it moves five unrelated tests. See
    the retired `mock-native-context-by-name-writes-were-silent` write-up
    (RETIRED 2026-08-06; §4–§7 there add the mirror-image defect — the mock
    answering a *different* slot — and the fourth by-name entry point).

    **`java/security/CodeSource` is still open** and is the same shape: its
    model is `(location, certs)` — constructor order again — where the class
    declares `location, signers, certs, …`, so `certs` sits on `signers`. It
    has about a dozen raw `get_field(cs, 0)` / `get_field(cs, 1)` readers to
    audit, which is why it is its own change and not a rider on this one.

    ### The second worked example — `java/lang/Thread`, which had NO live defect

    Recorded because it is the opposite outcome to `ThreadGroup` and the two
    together are what a NAME row actually means.

    The model declared `contextClassLoader` at index **5**, which is `holder` on
    every real image (`threadLocals` and `inheritableThreadLocals` at 6 and 7
    were right by accident). `CRATONVM_DBG=overlay-bt` named the writer at the
    flagged site in one run: `populate_real_thread_holder`, storing the
    `FieldHolder` at slot 5 — **the correct field**. Every other accessor
    resolves on the receiver's own class. `probes/ThreadLayoutProbe.java`, run
    against the pre-fix binary in both modes, is byte-identical to the post-fix
    binary and matches HotSpot on every behavioural line, including a reflective
    read of all six leading fields.

    So: **no reproducible defect, and the change is hardening rather than a
    fix.** What it removes is a landmine and a false census row. Three things
    moved:

    * the model names `contextClassLoader` at 4, where the image has it, and
      leaves 5 anonymous. Slot 5 is deliberately **not** named `holder`:
      `populate_real_thread_holder` uses `get_field_by_name(this,
      "holder").is_none()` to detect the fabricated layout, so declaring it
      would silently disable that fallback;
    * the fabricated-only virtual-thread flag moved 4 → 5 so it stops sharing a
      slot with a field that is shared with the image. **Moving the model
      without moving the flag was the obvious half-fix and it is wrong** — a
      test asserts the two do not overlap, and it fails on exactly that
      intermediate state;
    * `is_virtual_synthetic` in `vm_exec.rs` asked `num_slots() >= 5`, which
      every real `Thread` satisfies (19 fields), so it was reading a real
      `contextClassLoader` and comparing it to `Int(1)` — correct only because a
      reference can never match. It now asks whether the receiver declares
      `eetop`, a name the real class has and the stub does not. The fifth
      count-based layout guard this record has had to correct.

    The `java/lang/Thread` row goes 2 → 1 in the census; the survivor is
    `slot 1 value=Long(3) real=tid:J`, a correct write flagged only because an
    anonymous model slot declares `Ljava/lang/Object;`. That is the
    map-entry-not-a-defect family, and it is why `verdict=NAME` is the
    actionable filter.

    ### Reading a NAME row — `ThreadGroup`, worked through

    **A NAME row says the model and the image disagree. It does not say which
    of the two is wrong, and for `ThreadGroup` the answer was BOTH, in
    different places.** Tracing it is the whole job; four of the six reported
    access sites turned out not to be defects at all.

    The natives (`native-builtins/src/phases_late/concurrent.rs`) go through
    `tg_slot`, which resolves the field **by name first** and only falls back
    to a hard-coded index. On a real image every `ThreadGroup` declares all
    four names, so the fallback is never reached and those writes were already
    landing on the right fields — they were reported only because the *model*
    they were being compared against was transposed. Fixing them would have
    entrenched the bug; the fix was to correct the model
    (`synthetic_stub_fields` now declares the real `parent, name, maxPriority,
    daemon` order) and the fallback constants with it, under a test that
    asserts the two tables agree **and** that they are the JDK's order — so a
    future transposition of both together still fails.

    Behind those four sat one real defect, on the one path that used a raw
    index: `SecurityManager.getRootGroup` wrote the fields by name and then
    wrote them **again** by raw index 0..3 in the legacy order, guarded by
    `object_num_fields >= 4`. The raw pass ran second and won. Measured on the
    pre-fix binary through `MethodHandles.findVirtual` (reflection cannot see a
    registered native — `getDeclaredMethod` scans the real class's metadata and
    answers `NO_SUCH_METHOD`):

    | | pre-fix | post-fix / HotSpot |
    |---|---|---|
    | `getName()` | **null** | `system` |
    | `getParent()` | **a `java.lang.String`** | null |
    | `getMaxPriority()` | **0** | 10 |
    | `isDaemon()` | **true** | false |

    A `String` returned where every caller expects a `ThreadGroup` — the same
    live shape as L1's `classloader_parent`. The guard it sat behind is the one
    this record keeps describing: `object_num_fields >= 4` stops an
    out-of-range write, not a wrong-field one. That is the fourth such guard in
    this file's story.

    **Two lessons for the remaining eleven classes.** A NAME row is a lead, not
    a verdict: read the writer before changing it, because a by-name writer
    under a wrong model produces rows that are noise. And a probe is not an
    oracle until it goes red on the pre-fix binary —
    `probes/ThreadGroupLayoutProbe.java` was byte-identical across the two arms
    on every section until it reached `getRootGroup`, because everything else
    resolves by name.
  * **The `Object(None)` half is no longer verified only by unit test.** Two new
    cross-type WRITE rows appear that the pre-fix binary is silent on, both
    `Object(None)` over an `int`: `jdk/internal/math/FloatingDecimal$1` slot 0
    and `ReentrantReadWriteLock$Sync$ThreadLocalHoldCounter` slot 0. The
    `Properties` slot-3 row the lane predicted does **not** appear — L2 step 3
    had already removed that write. The prediction was stale, not wrong.
  * **`java/util/Scanner`'s model is `instance_fields(5)`, not 3**, and slots 3
    and 4 are the real `delimPattern` and `hasNextPattern`, both
    `java.util.regex.Pattern` references. That settles L3 step 3's "either the
    model grew or a different writer is involved" without running a tracer.

  What the widened detector **still** cannot see, so the next reader does not
  re-derive it: an anonymous `_fN` model slot over a real *reference* field is
  unfalsifiable — the model declares `Ljava/lang/Object;` and so does every
  reference field in the JDK. That is the residual half of kind 5, and the way
  to shrink it is to name more of `synthetic_stub_fields`, not to run the census
  again. A class with no arm in that table is outside the diff entirely.

  ### The 19 open slots are FOUR defects, not nineteen

  Classified 2026-08-04 by tracing each writer (`CRATONVM_DBG=overlay-bt` names
  the Rust frame; the Java frames mislead). Each kind has a different fix, and
  applying the wrong one is silent:

  | # | kind | tell | fix | status |
  |---|---|---|---|---|
  | 1 | synthetic slots written onto a real layout | the real class declares a field our model does not have | write the slots only when the layout is ours, keyed on a field name the real class declares | **`VarHandle` fixed** |
  | 2 | right field, index computed against the **wrong class** | a hard-coded class name in the index lookup — or, as in `Scanner`, no lookup at all, just our model's index | `resolve_field_index_by_class_id` on the receiver | **`Properties` 5/6/7, 2, 3 and `HashMap` 2 fixed** (L2); **`Scanner` 1/2/3/4 fixed** (L3); `URI` open |
  | 3 | VM-internal value with **no real field at all** | the constant has no JDK counterpart (`CL_LOADER_ID`) | side table keyed by the object, as `vh_meta_put` does — or no storage at all, if the value never survived its own write | **`ClassLoaders` ×2 fixed** (L1, 2026-08-05); **`Scanner` slot 0 and `MemberName` slot 4 fixed** (L3, 2026-08-05) |
  | 4 | right field, **wrong representation** | real field is a reference, ours is a primitive | convert (`int` → the `Proxy.Type` enum constant) | `Proxy` open |

  Kind 3 is the one that cannot be fixed by resolving harder: there is nowhere
  correct in a real layout to put a `CL_LOADER_ID`. Kind 4 likewise — resolving
  `java.net.Proxy.type` by name finds a real field, and writing our `int` into
  it is still wrong, because the real field holds a `Proxy$Type` **enum
  reference**.

  **Kind 5, added 2026-08-05 by the L1 fix — and made findable the same day by
  L4.** A VM-internal reference written into a real reference slot. Same
  wrong-field write as kind 1, but the value-tag predicate only flags
  cross-type-class coercions, so nothing in the census above reported it. The
  three `ClassLoader` reference slots (1/2/5) were all of this kind, and one of
  them was returning a `String` where every caller expected a `ClassLoader`.

  **The original filing of this paragraph said "no amount of re-running the
  census will find another one" and that the only instrument is a behavioural
  probe. That was true of the detector as it stood and is no longer true.** The
  shadow-layout diff compares CratonVM's `synthetic_stub_fields` model against
  the real layout by NAME, which is a signal the value's type tag does not
  carry — and on its first run it found 23 such slots across 12 classes,
  including `java/lang/ThreadGroup` with two field pairs transposed (fixed the
  same day — see the worked example above for why four of its six reported
  access sites were not defects). What is
  still unfindable is the narrower case where the model slot is **anonymous**
  (`_fN`, declared `Ljava/lang/Object;`): there the model asserts nothing, so
  there is nothing to disagree with, and only a behavioural probe diffed against
  the host JDK (`probes/L1LoaderIdentityProbe`) or reading the writer against
  `javap` will do. Any file that writes a hand-numbered slot model onto a class
  that can become real still has that exposure. Naming the model's fields is
  what converts it into something checkable.

  Two things found while classifying, both worth fixing alongside:

  * **The synthetic `URI` model is duplicated**, with identical constants, in
    `native-builtins/src/http2.rs` and `native-builtins/src/servlet.rs`. Two
    copies of a layout is how the `real_protected_stub` allow-lists drifted.
  * ~~`native_map_init`'s legacy branch still writes raw `MAP_FIELD_*`
    indices~~ — **done 2026-08-04 as lane L2**, with its own A/B; see the
    `Properties` bullet above for the numbers. It did want its own change:
    the same fixed-class lookup turned out to be in five functions, and the
    conversion had to keep the fabricated layout intact for receivers that
    genuinely have it, which is a predicate on a field NAME and not on a slot
    count.
  * **Found while verifying L2, not layout defects, not fixed** — recorded
    here so a later reader does not have to re-derive them.
    `probes/MapLayoutMatrixProbe` diffs byte-for-byte against HotSpot 25
    except for these, identical in both modes and unchanged by L2:
    `Properties.getProperty(null)` / `setProperty(k, null)` / `put(null, v)` /
    `load((InputStream) null)` return normally where the JDK throws
    `NullPointerException`; `HashMap` iteration does not raise
    `ConcurrentModificationException` when the map is structurally modified
    mid-iteration (the probe's loop then runs to 99 entries where HotSpot
    stops at 50); `Hashtable` accepts null keys and null values; and
    `new Hashtable<>(h).equals(h)` is `false`. Each is a semantics change on a
    shared hot path — `native_map_put` backs both `HashMap` and `Hashtable` —
    so each wants its own change and its own A/B.
* **Step 3**, replacing the two `breaks-under-strict` sites in `vm_util.rs`.
  Note the `ValueLayout` one cannot be converted at all — the marker is explicit
  that there are no real fields to name, so it is a
  `CompatibilityClassRequested` violation, not a slot-numbering bug, and fixing
  it means letting the real `ValueLayout.<clinit>` run.
* **Step 4**, making `safe` verdicts checkable rather than asserted. They are
  claims about JDK 25 that nothing in the build re-checks; a JDK upgrade should
  fail a test, not corrupt an object.

## What is wrong

A large amount of CratonVM native and VM-internal code reaches into Java objects
by **slot index** (`heap.set_field(obj, 1, …)`, `heap.get_field(obj, 3)`) rather
than by resolving the field by name. Those indices were chosen against
CratonVM's own fabricated layout for the class. The moment the class is loaded
from real JDK bytes — which is the entire premise of `--jdk-only` — the index
still resolves, still type-checks, and now points at a *different field*.

There is no fault, no exception and no log line. The object is simply wrong
afterwards.

## Evidence

### The registry already maintains a hand-curated list of classes where this happens

`native-api/src/registry.rs` (~4391), the `drop_real_layout_synthetic` field
doc, names **six** class families whose synthetic natives had to be dropped
wholesale in real-JDK mode *because of layout drift alone*. Note the mismatch
inside the doc itself: its opening sentence enumerates *"`java/util/StringJoiner`,
`java/io/StringReader`, `java/util/EnumSet`, `LinkedBlockingDeque`, and
`ScheduledThreadPoolExecutor`"*, while the prose that follows also describes
`Pattern`/`Matcher` and never returns to `ScheduledThreadPoolExecutor`. Treat
the enumeration as incomplete in both directions until someone reconciles it
against `set_drop_real_layout_synthetic`'s actual effect.

* **`java/util/StringJoiner`** — registered with a fake 5-field layout
  (`delim/prefix/suffix/elements-ArrayList/emptyValue`); the real class has 7
  (`prefix/delimiter/suffix/elts[]/size/len/emptyValue`). The synthetic `add`
  reads slot 3 — real `elts`, null — and no-ops, so `size` never moves and
  `toString` renders just prefix+suffix. **A silently empty join, not a crash.**
* **`java/util/EnumSet`** — "the same problem in a more dangerous form": the
  fallback native surface manufactures an abstract `java/util/EnumSet` receiver
  with a two-field synthetic layout, so `EnumSet.of(...)` / `allOf(...)` return
  an empty object with `iterator() == null`.
* **`java/util/concurrent/LinkedBlockingDeque`** — the four-slot fake
  blocking-queue layout leaves real final fields (`lock`, `notEmpty`) null;
  Tomcat's `WriteBuffer.clear()` then fails inside `LinkedBlockingDeque.clear()`.
* **`java/io/StringReader`** — the real class wraps a final `Reader r`; the
  synthetic constructor writes the old `(content, pos, length)` slots, leaving
  `r` null before `mark()` delegates.
* **`java/util/regex/Pattern` / `Matcher`** — the legacy regex natives allocate
  real-layout objects but write the old synthetic slots, leaving fields such as
  `Matcher.locals` uninitialised.

Note what that list is: the classes where the drift was severe enough to be
noticed and worked around. It is a sample, not a census.

### The wave-1 `JDK-ONLY-LAYOUT:` marker sweep

Wave 1 introduced a `// JDK-ONLY-LAYOUT: <verdict>` marker with the verdicts
`safe`, `unknown`, `breaks-under-strict` and `converted`. As of 2026-07-31 there
are **10 markers across 3 files**, distributed as follows (corrected against the
re-landed tree — the original filing put two `safe` verdicts in `vm_object.rs`
where there is one plus a file-level anchor):

| File:line | Verdict | Site |
|---|---|---|
| `vm/src/vm/vm_util.rs:251` | `breaks-under-strict` | `FileInputStream` fallback: writes `Int(1)` into slot 1, which on a real `java/io/FileInputStream` is `path:String`. Flagged "dead arm on real bytes" |
| `vm/src/vm/vm_util.rs:2069` | `breaks-under-strict` | FFM `ValueLayout` preseed: assumes slot 0 = `byteSize` on an object of an **interface** type that has zero instance fields |
| `vm/src/vm/vm_util.rs:1822` | `converted` | `Throwable` cause-chain walk, converted from raw slots to name lookup |
| `vm/src/vm/vm_util.rs:2756`, `:3572` | `safe` | `NormalizerBase$ModeImpl`, `AtomicInteger`, verified against JDK 25 |
| `vm/src/vm/vm_object.rs:28` | `safe` (**file-level anchor**) | the `java/lang/String` slot convention — slots 0..3 = `value`/`coder`/`hash`/`hashIsZero` — asserted to be the *real* JDK 9+ declaration order, not a synthetic invention. Every `String` slot literal in the file inherits this verdict |
| `vm/src/vm/vm_object.rs:689` | `safe` | speculative `String` shape probe; deliberately kept index-based, because a named lookup would resolve `value` off whatever class the receiver actually is and defeat the shape check |
| `vm/src/vm/vm_object.rs:1014` | **`unknown`, ranked HIGH** | class-mirror populator writes an `Int` over slot 0 of a real `java.lang.Class`, which JDK 25 declares as `Constructor<T> cachedConstructor` — a *reference* slot. An **overlay**, not a mis-numbering: the safety claim rests on this VM's reference-vs-primitive decode, not on HotSpot's |
| `vm/src/vm/vm_object.rs:1212` | **`unknown`** | primitive-mirror `Int(-1)` marker over the same slot 0. The marker says to resolve both together, and notes a primitive mirror has no legitimate `cachedConstructor` reader, so it can move to the `primitive_mirrors` side table if the overlay proves destructive |
| `vm/src/vm.rs:34` | `safe` (**whole file**) | ~500 raw slot accesses, all inside `#[cfg(all(test, feature = "synthetic-jdk"))]`. A verdict about *reachability*, not quality: `synthetic-jdk` is a build feature that excludes the real class library, whereas `--jdk-only` is a runtime policy on a real image, so none of it is reachable from a strict run |

The two `breaks-under-strict` sites are worth reading in full; both are exactly
the shape described above. The two `unknown` verdicts are a *different* hazard
and should not be triaged with the same instinct: they are **overlays** — a
VM-internal value written deliberately on top of a real JDK field — where the
question is not "is this the right slot" but "does writing an `Int` where the
image declares a reference corrupt anything". The `Throwable` one is the
clearest illustration of the ordinary failure mode:

> A synthetic `java/lang/Throwable` stub is `instance_fields(2)` — `_f0` =
> message, `_f1` = cause — but the REAL JDK declares `backtrace`,
> `detailMessage`, `cause`, `stackTrace`, `depth`, `suppressedExceptions`, so
> real slot 0 is `backtrace` and the message lives at slot 1. Reading slot 0 as
> the message against real bytes is a silent wrong-field read.

That one was fixed (converted to a name-walking lookup). The others were not.

**10 markers is the count of sites someone got to, not the count of sites that
exist.** The marker sweep covered `vm/src/vm/`; `native-builtins`,
`native-collections` and `native-io` — where the great majority of index-based
field access lives — were not swept.

## Why it was not fixed in wave 1

Wave 1 is measurement, not deletion (contract §10). Two of the three verdicts
are also not mechanical:

* The `ValueLayout` site cannot be "converted to named-field lookup" at all —
  the marker says so explicitly: *"there are no real fields to name. It is a
  `CompatibilityClassRequested` violation, not a slot-numbering bug."* Fixing it
  requires letting the real `ValueLayout.<clinit>` run, which requires
  `jdk/internal/misc/UnsafeConstants` to be backfilled with real platform values
  before class preparation.
* The `safe` verdicts are only safe *against JDK 25*. They are assertions about
  a specific image, and nothing in the build re-checks them.

## What specifically must change

1. **Finish the sweep.** Extend the `JDK-ONLY-LAYOUT:` marker discipline to
   `native-builtins`, `native-collections`, `native-io` and `vm/src/native/`.
   Until that is done, the 10 markers understate the problem by an unknown
   factor.
2. **Adjudicate the two `unknown` verdicts** in `vm/src/vm/vm_object.rs`.
3. Replace `breaks-under-strict` sites with name-resolved field access
   (`find_field_recursive(cid, "fd", &cm.class_store)` — the pattern the
   `FileInputStream` site already uses on its *primary* path, with the raw slot
   only as a fallback), or with a structured refusal under
   `CompatibilityMode::JdkOnly` where there is no real field to name.
4. Make `safe` verdicts checkable rather than asserted: a startup or test-time
   assertion that the named field really is at the assumed index for the loaded
   image, so a JDK upgrade fails a test instead of corrupting an object.

## How to verify a fix

* Per site: load the real class and assert `find_field_recursive(cid, name)`
  returns the index the code assumes. A `safe` claim that cannot be expressed as
  such an assertion is not verified, it is remembered.
* End to end: the five `drop_real_layout_synthetic` classes are the ready-made
  regression corpus. A correct fix should let each of them keep its native
  surface *without* the drop — `StringJoiner.add()` moving `size`,
  `EnumSet.of()` returning a non-null iterator, `LinkedBlockingDeque.clear()`
  surviving Tomcat's `WriteBuffer.clear()`.
* `--dump-class-origins` (contract §5) tells you which classes came from real
  bytes in a given run; any index-based access to a class listed `boot-image` or
  `application-classpath` is by definition suspect.

## Blast radius if done wrong

Converting an index to a name lookup is safe but not free — the resolution walks
the superclass chain and these are hot paths. Converting the *wrong* index
(picking the field the code was accidentally hitting rather than the one it
meant) preserves today's behaviour and silently entrenches the bug.

The dangerous direction is dropping a native surface without checking the real
bytecode is self-contained: `StringJoiner` is documented as safe to drop because
"the real bytecode is self-contained and correct", but that is a per-class
finding, not a general rule.

## Related

* `docs/known-issues/jdk-only/README.md` — index.
* The `StringJoiner` divergence between the two real-protected-stub allow-lists
  is a *separate* consequence of the same class's layout drift; see
  real-protected-stub allow-lists diverge (`jdk-only-real-protected-stub-allowlists-FIXED-20260804.md`) (reconciled 2026-08-04).
* [`docs/jdk-only-object-layout-audit.md`](../../jdk-only-object-layout-audit.md)
  — the companion audit. The original filing recorded that this file did not
  exist; **it does now**, and it is the right starting point for the sweep in
  step 1. Read it before extending the marker discipline into a new crate, so
  the verdict vocabulary stays consistent.
