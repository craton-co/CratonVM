# L2 — `native_map_init`'s raw `MAP_FIELD_*` branch → by-name

> **DONE 2026-08-04**, branch `fix/jdk-only-l2-mapinit-byname-20260804`.
> What was actually done and measured is in *Outcome* at the bottom; the
> sections above are the plan as written, left unedited so the two can be
> compared. **L10 can rebase now.**

**Owns:** `native-collections/src/lib.rs` (whole file — 55k lines, one owner)
**Gated on:** nothing.
**Conflicts:** L10 (real `ThreadPoolExecutor` init) is in the same file. **Land
L2 first**; it is the smaller change.
**Effort:** M
**Evidence:** [`jdk-only-fabricated-object-layouts-FIXED-20260810.md`](../../fixed-bugs/jdk-only-fabricated-object-layouts-FIXED-20260810.md) (RETIRED 2026-08-10)

## Goal

`native_map_init`'s legacy branch writes `MAP_FIELD_BUCKETS` / `MAP_FIELD_CAPACITY`
by raw absolute index. On a real JDK class those indices are different fields.
It is the surviving `Properties` slot-2 row (`Object` over an `int`, 3 hits) and
the whole `HashMap` family (~6,500 hits).

**Kind 2** — the real fields exist (`table`, `size`, `threshold`, `loadFactor`);
we compute their indices against the wrong layout. Fix shape already proven in
this file on 2026-08-04: `try_set_jdk_map_field` was resolving names against a
hard-coded `"java/util/HashMap"` and writing that index into any receiver;
switching it to `resolve_field_index_by_class_id(class_id_of_object(this), name)`
took three `Properties` rows to zero with every other row byte-identical.

## The part that needs care

The `HashMap` family is **documented benign** and must stay benign. Coercion of
our `Int` to `null` on a real `Map` subtype produces exactly the null-initialised
state real `Map` bytecode expects (S111r29), which is why the overlay hunter
suppresses it by default. Do not "fix" those 6,500 hits into something that
changes `Compatible` behaviour without measuring — the natives are the
authoritative implementation for `HashMap` and JDK bytecode does not run for it.

So the target is narrower than the hit count suggests: make the writes land on
the right fields *when the receiver is not one of ours*, and leave the
native-backed `HashMap` path alone.

## Steps

1. Read `native_map_init`'s two branches. `uses_native_hashtable_layout`
   (i.e. `CF_HASHTABLE_LAYOUT`) already excludes `Properties` deliberately — JDK
   25 backs it with a side `ConcurrentHashMap`. That exclusion is correct; the
   bug is in the *legacy* branch it falls into.
2. Convert `MAP_FIELD_BUCKETS` / `MAP_FIELD_CAPACITY` writes to by-name
   resolution on the receiver's class, keeping the raw write only when the
   receiver has our synthetic layout (predicate by **name**, never field count).
3. `native_props_init` writes `Value::Object(None)` to `PROPS_FIELD_DEFAULTS`,
   which is `loadFactor` on a real layout. **The hunter does not report this** —
   `overlay_write_is_destructive` ignores `Object(None)` over a primitive. Fix it
   even though it does not appear in the census, and see L4 for closing that
   blind spot.

## Verification

* A/B the census: `Properties` slot 2 goes 3 → 0. The `HashMap` rows may stay —
  if they change, prove the change is intended and that `Compatible` is
  unaffected.
* `cargo test --release -p cratonvm-native-collections --lib` (94 tests).
* Both probes vs HotSpot, both modes. Collections are exercised heavily by
  `JdkOnlyCensusLoadProbe`'s first section, so a regression here is loud.
* `Compatible` byte-for-byte: run the `test_classes` corpus under the pre-fix
  and post-fix binaries, normalise timestamps, diff.

## Done when

`Properties` slot 2 is gone, the `HashMap` family is unchanged or its change is
justified in the record, and the 94 collections tests pass.

---

## Outcome (2026-08-04)

### What the fix is

`receiver_table_slot(ctx, this)` resolves `table` **by name on the receiver's
own class id**, bounded by the receiver's allocated slot count.
`publish_map_table{,_volatile}` is the one idiom every bucket-publishing site
now uses:

* slot 0 (`MAP_FIELD_BUCKETS`) always — every other native map op reads it, and
  the native surface stays the authoritative `HashMap` implementation.
  **Superseded 2026-08-05**: that second store is what put a bucket array in
  `AbstractMap.keySet`, and the follow-up lane
  (`fix/map-model-slots-on-real-layout-20260805`) removed it by teaching the
  readers to ask `map_buckets_slot` instead. This bullet describes L2's
  intermediate state, not the tree;
* the receiver's real `table` when that is a different slot;
* the legacy `Int(capacity)` at slot 2 **only when the receiver has no `table`
  field at all**, which is what "our fabricated layout" means.

The predicate is a field NAME the real class declares, never a count — the
fabricated `cratonvm/util/MapViewBacking` is *wider* than a real `HashMap`, so
a count test would have been the fourth guard in this file's story that cannot
fail.

The plan named `native_map_init`. The same fixed-class lookup was in **five**
functions: `native_map_init`, `native_map_init_capacity`, `map_resize`,
`resync_view_set` and `map_state`'s bucket fallback, plus a read in
`hashmap_serialized_capacity`. Step 3 (`native_props_init`) became
`props_defaults_slot`, through which both writers and all three chain readers
now go.

### The scope note in *The part that needs care* was right, and narrower than it looks

The `HashMap` family did change, deliberately and measurably: the sites writing
`Int(cap)` at absolute slot 2 unconditionally (`new HashMap<>(map)`, `Map.of`,
`Set.of`, `newKeySet`, the `HashSet` backings) were writing an int into the
REAL `table` field of a real-layout receiver, while `native_map_init` next door
already stored the bucket array there. They now agree. Nothing reads the
dropped `Int`: `map_state` derives capacity from the array's length and only
falls back to slot 2 when there is no array.

### Measured

A/B, pre-fix vs post-fix binary, same workload, four probes × both modes,
JDK 25:

| row | pre | post |
|---|---:|---:|
| `java/util/Properties` slot 2 `Object` over `I` | 40 | **0** |
| `java/util/Properties` slot 3 `Object` over `F` | 12 | **0** |
| `java/util/HashMap` slot 2 `Int` over `[` | 66 | **0** |
| every other census row | — | byte-identical |

`JdkOnlyCensusLoadProbe` alone: `Properties` slot 2 **3 → 0**, the number this
doc predicted. Probe transcripts identical pre/post but for an ephemeral TCP
port and a timing line. `test_classes` corpus identical in `Compatible` mode
with timestamps normalised, all 10 members. 101 `--lib` tests
(94 + 7 new) green in both feature configurations, and all 12 integration
targets — including `gc_relocation_harness`, which had not compiled since
`gc_overlay_roots_for_collection` grew a parameter.

The benign `HashMap` slot-1 row moved 8,256 → 8,318. That is the
`stringPropertyNames` fix below building one more set per call, not this
change; the row's own spread on an unmodified binary is ±4 on a single probe
(1634 / 1636 / 1638 over three runs), so it does not resolve at that scale
anyway.

### Each new test was shown to fail

Seven unit tests, then the pre-fix resolver injected (resolve on a fixed
`java/util/HashMap`; always the raw props slot) — four fail, including the
flagship `Properties` one, and revert restores green. The flagship test needed
the fixture to declare the real `HashMap` layout *as well*: without a loaded
`HashMap` for the old code to find an index on, it passed vacuously. That is
the shape of the "guard that cannot fail" this feature keeps producing, caught
this time by doing the injection instead of assuming it.

### New instrument

`probes/MapLayoutMatrixProbe.java` — 78 deterministic, order-normalised lines
over the `defaults` chain, `HashMap.writeObject`'s `table` walk,
spliterator/view paths, resize boundaries and a `Hashtable` control section.
Run under HotSpot FIRST; it immediately falsified an assumption (the `defaults`
chain already worked through the side table, so this fix moved no *behaviour*
there — it stopped destroying `threshold` / `loadFactor`, which nothing
observes today and which is exactly why it is a latent defect and not a bug
report).

It also found, and this change does NOT fix, four divergences from HotSpot 25
that are not layout defects: `Properties.getProperty(null)` /
`setProperty(k, null)` / `put(null, v)` / `load(null)` do not throw NPE;
`HashMap` iteration raises no `ConcurrentModificationException`; `Hashtable`
accepts null keys and values; `new Hashtable<>(h).equals(h)` is false. They are
recorded in the evidence file. One divergence it did fix, in a second commit:
`stringPropertyNames()` did not walk the `defaults` chain while
`propertyNames()` did — `--dump-native-registry` named
`properties_sidetable.rs` as the live owner of the triple, so the
`native-collections` implementation that *does* walk it never runs.

### Still open, deliberately

* ~~**L4 gap 3.**~~ **CLOSED by L4, 2026-08-05.** The hunter now flags
  `Object(_)` over a primitive, and re-running the census on the dev tip shows
  no `Properties` slot-3 row — so step 3 is census-verified, not just
  unit-tested. L4's own commit names this write as gap 3's measured instance.
* ~~**Same-kind wrong-slot writes remain invisible** (L4 gap 2)~~ — **the
  `HashMap` half is FIXED, 2026-08-05**, in
  `fix/map-model-slots-on-real-layout-20260805`. It needed an instrument no
  census can provide: `probes/MapModelSlotProbe` reads `AbstractMap.keySet`
  reflectively under `--add-opens` and showed `keySet=ARRAY[Object]` on every
  CratonVM map where HotSpot has `null` — including after `keySet()` was
  called, where the JDK caches a `HashMap$KeySet`. `map_buckets_slot` /
  `map_size_slot` now address both model slots on the receiver's own layout;
  the `HashMap` slot-1 census row went 8,342 → 0 and `keySet` is null on every
  row. The general statement still stands for classes nobody has looked at:
  a same-kind wrong-slot access has no per-access tell, and L4's shadow-layout
  diff reports the disagreement without saying what the slot holds.

### Verified on

**Both hosts, independently, against the same Temurin 25.0.3+9 image.**

* Windows 11: release binaries from `d81e220b3` (pre) and this branch (post);
  101 `--lib` tests and all 12 integration targets green, 107 with
  `--features synthetic-jdk`.
* Azure Linux (`/data/data/wt-l2mapinit-20260804`, `wt-l2base-20260804`):
  release binaries from `2572ea9af` (pre — the dev tip this branch merges) and
  this branch (post). Census A/B identical to the Windows numbers row for row
  (`Properties` 2: 40 → 0, `Properties` 3: 12 → 0, `HashMap` 2: 66 → 0, every
  other row unchanged); probe transcripts identical pre/post but for an
  ephemeral port, a timing line, and the three `stringPropertyNames` lines that
  now MATCH HotSpot; `test_classes` corpus 10/10 identical in `Compatible`
  mode; 101 `--lib` tests green in the release profile and all 14 test targets
  green in the debug profile.

`JdkOnlyBreadthProbe` is byte-identical to HotSpot on Linux but differs on
Windows in one `DecimalFormat` grouping separator — a pre-existing,
host-specific locale divergence, present on both arms and not this change's.

A full `cargo test --release -p cratonvm-native-collections` (every target, not
just `--lib`) was killed twice on the shared host with no compiler error and no
OOM line in `dmesg`, which is the failure mode the wave-2 README warns about;
the same command completes in the debug profile, and `--lib` completes in
release.
