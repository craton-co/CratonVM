# JDK-only mode — object-layout audit

| | |
|---|---|
| **Status** | Wave 1, measurement. No enforcement lands here. Conversions are listed per site; most sites are annotated, not changed. |
| **Normative source** | [`feature-designs/jdk-only-mode.md`](feature-designs/jdk-only-mode.md) §1, §5, §10 |
| **Answers** | the [`jdk-only-native-review.md`](jdk-only-native-review.md) checklist line *"No object field is accessed by assumed synthetic slot index"* — that box cannot be ticked without this evidence base |
| **Related** | [`jdk-only-runtime-services.md`](known-issues/jdk-only/runtime-services-blocker-inventory.md) P0 rows *Residual synthetic native set* and *Direct `ensure_synthetic_class` calls* |
| **Evidence base** | Real layouts in this document were read off a JDK 25 image (`jdk-25.0.3.9-hotspot`) with `javap -p`, walking the superclass chain by hand. Rows without that evidence are marked **unknown** and say what would settle them. |

> **This audit is a prerequisite for wave 2, not cleanup after it.** The
> conversion of index-based field access must land *before* the corresponding
> stub is dropped. Doing it in the other order produces silent field
> corruption, which is far more expensive to debug than a clean failure.

---

## 1. The dangerous list — read this first

These are the classes where **dropping the stub (or letting real bytes win)
produces a silent wrong-field read or write, not a clean failure.** Ranked by
how quiet the failure is and how much of the tree sits behind it. Every "real
layout" column below is `javap`-verified against JDK 25 unless it says
otherwise.

| # | Class | Assumed (synthetic) slots | Real JDK 25 slots | What goes wrong, silently |
|---|---|---|---|---|
| 1 | `java/lang/StringBuilder`, `java/lang/StringBuffer` | `_f0` = value(`char[]`), `_f1` = count | inherited from `AbstractStringBuilder`: `value`@0, `coder`@1, `maybeLatin1`@2, `count`@3 (`StringBuilder` itself declares no instance field) | **Already happened, twice.** `sb_set_count` (`native-builtins/src/lang_string.rs`) writes `Int(0)` to slot 1 and `count` to slot 2 whenever the object has ≥3 slots — on JDK 25 that is `coder := 0` (forces LATIN1) and `maybeLatin1 := count`. Its own doc comment records the two outcomes: an infinite `while (sb.length() < n)` spin during `java.desktop` clinit, and a poisoned cglib method signature that aborted `AotIntegrationTests#endToEndTestsForBeanOverrides`. The current mitigation is a **dual write** — keep the wrong index writes so CratonVM's own natives stay self-consistent, and additionally mirror by name. Under `JdkOnly` the index half becomes pure corruption. |
| 2 | `java/lang/Throwable` **and 21 subclasses** (`Exception`, `RuntimeException`, `Error`, `NullPointerException`, `IOException`, …) | `_f0` = message, `_f1` = cause | `backtrace`@0, `detailMessage`@1, `cause`@2, `stackTrace`@3, `depth`@4, `suppressedExceptions`@5 | Slot 0 on real bytes is `backtrace`, an opaque VM object. Reading it as the message yields `None` from `read_java_string` → an **empty message with no error**. Writing it clobbers the backtrace. Exception text is the primary diagnostic channel for every other wave, so this failure mode actively hides the failures the other agents are hunting. One instance fixed in this change (§4). |
| 3 | `java/util/HashMap`, `java/util/LinkedHashMap` | `MAP_FIELD_BUCKETS`@0, `..._SIZE`@1, `..._CAPACITY`@2 | `keySet`@0, `values`@1 (both from `AbstractMap`), `table`@2, `entrySet`@3, `size`@4, `modCount`@5, `threshold`@6, `loadFactor`@7 | Half-loud, half-silent. `getfield table` reading the synthetic `Int(capacity)` surfaces as `expected object reference, got int(16)` — that half is loud. The other half is not: the bucket array written to slot 0 lands in `keySet`, so `keySet()` returns the bucket array and `values()` returns the size. Partially mitigated in `native-collections/src/lib.rs` by dual-storage (the "mirror the bucket array into the real `table` slot" comment) and by parking view markers at slots 14/15/16, deliberately above every real `HashMap` field. The mitigation is per-call-site, not structural. |
| 4 | `java/util/TreeMap`, `java/util/TreeSet` | `TM_FIELD_DATA`@0, `..._SIZE`@1, `..._COMPARATOR`@2 | `keySet`@0, `values`@1, `comparator`@2, `root`@3, `size`@4, `modCount`@5, `entrySet`@6, … | Fully silent. Slot 1 (`size`, an `int`) lands on `values`, a reference field: `values()` then returns a boxed count. Slot 2 collides with the real `comparator` by pure coincidence of position. `TreeMap` is called out in the memory record as **not** covered by the `HashMap` dual-storage fix — it uses a separate `ts_` side table. |
| 5 | `java/util/ArrayList` | `AL_FIELD_DATA`@0, `AL_FIELD_SIZE`@1 | `modCount`@0 (from `AbstractList`), `elementData`@1, `size`@2 | The element array written to slot 0 lands on `modCount`, an `int`; the size written to slot 1 lands on `elementData`, a reference. `native-collections/src/lib.rs` already carries an explicit comment that real `ArrayList.size` is at slot **2**, with a name-resolved path and a synthetic fallback — so this one is known and half-fixed. The fallback is still reachable. |
| 6 | `java/lang/Class` (the mirror) | VM-internal: `ClassId` as `Int` at slot 0 | `cachedConstructor`@0 — a **reference** field | Not a mis-numbered slot but an **overlay**: a VM-internal `Int` written on top of a live JDK reference field. Verdict **unknown**, ranked high — see §4 for the three pieces of evidence that would settle it. `native-builtins/src/lang_class.rs::mirror_class_id` / `mirror_class_name` read slots 0 and 1 as a fallback, so the convention leaks out of the VM crate. |
| 7 | `javax/management/ObjectName` | `instance_fields(1)` | `_canonicalName`@0, `_kp_array`@1, `_ca_array`@2, `_propertyList`@3, `_compressed_storage`@4 | This is the concrete class behind the P0 *JMX real path* row: with stubs dropped, WildFly's first `getPlatformMBeanServer()` NPEs deep in real `javax.management` bytecode with `ObjectName._ca_array` null. A 1-slot stub cannot hold five fields, so four of them are permanently null and the padding path (§2.3) silently swallows any write past the end. |
| 8 | `java/net/InetSocketAddress` | `instance_fields(3)` (addr / port / hostname); allocated as 2 **and** 3 at different native sites | `holder`@0 only — one field, of type `InetSocketAddress$InetSocketAddressHolder` | Named in the padding comment in `ClassManager::define_class_with_options` as the motivating example. Slot 0 gets an address where a holder object belongs; slots 1–2 exist only because of the padding. `getPort()` on real bytecode dereferences `holder` and gets whatever slot 0 holds. |
| 9 | `java/util/Properties` | `instance_fields(16)`, plus `PROPS_FIELD_DEFAULTS`@3 | `defaults`@0, `map`@1 — two fields | Fourteen of the sixteen slots exist only because of padding. `defaults` is written at slot 3 where the real class has nothing, and the real `defaults` at slot 0 is never written. Already has its own history (the live-`values()` view fix and the side-table cap that silently dropped loads). |
| 10 | `java/util/regex/Matcher` | `instance_fields(6)` | `parentPattern`@0, `groups`@1, `from`@2, `to`@3, `lookbehindTo`@4, `text`@5, `acceptMode`@6, `first`@7, `last`@8, … (20+) | Six anonymous slots overlay eleven real ones. Nothing collides *type*-wise in an obvious way, so every mismatch is silent. |
| 11 | `java/lang/foreign/ValueLayout$Of*`, `java/lang/foreign/AddressLayout` | 2 fabricated slots (`byteSize`@0, `byteAlignment`@1) | **interfaces — zero instance fields.** The real carrier is `jdk/internal/foreign/layout/ValueLayouts$Of*Impl`. | Not a slot-numbering bug at all: it is class fabrication. `vm/src/vm/vm_util.rs::make_prepared_value_layout` allocates an object of an *interface* type and invents two slots, and the surrounding code suppresses the real `ValueLayout.<clinit>` so the genuine layouts are never built. Under §1 of the contract this is a `CompatibilityClassRequested` violation, not a layout conversion. |
| 12 | `java/io/FileInputStream` (fallback arm only) | legacy `Int(1)` marker at slot 1 | `fd`@0, `path`@1, `channel`@2, `closeLock`@3, `closed`@4, `isRegularFile`@5 | Writing `Int(1)` into `path`. Dead on real bytes (the by-name resolve above it succeeds), but it is the shape of a landmine: a fallback that fires only when name resolution fails, i.e. exactly when the layout is least understood. |

**Second, quieter failure mode — write discard, not wrong field.** Removing a
`synthetic_stub_fields` arm without first converting its consumers does *not*
throw. `synthetic_stub_fields`' own doc comment records why:

> a native written against the factory shape that writes past the end of a
> `new`-created object has its write **silently discarded** by `set_field` —
> five separate bugs of exactly that shape were found in two days
> (`HttpExchange`, `HttpServer`, `sun/net/httpserver/HttpServerImpl`,
> `DatagramSocket`, `DatagramPacket`/`Preferences`).

So wave 2 has two silent failure modes to avoid, in opposite directions:
convert too late and you get wrong-field reads; shrink the layout too early and
you get vanished writes.

---

## 2. How the assumption works

### 2.1 Fabrication: anonymous slots

`classloading/src/class_manager.rs::synthetic_stub_fields(name)` returns a field
table for well-known class names. Its helper is explicit about the contract:

```rust
/// Helper to create N unnamed instance fields (for synthetic objects
/// whose native code accesses fields by index, not by name).
fn instance_fields(n: usize) -> Vec<ClassFileField>   // -> _f0 .. _f{n-1}, all Ljava/lang/Object;
```

Every field is named `_fN` and typed `Ljava/lang/Object;`. There is no name to
resolve and no descriptor to check, so *the index is the only thing that
carries meaning*. A sibling helper `pad_to(fields, total)` appends `_fN` slots
after a hand-written named list.

**Scale.** 131 arms in `synthetic_stub_fields` produce anonymous slots
(`instance_fields`) or pad a named list with them (`pad_to`); 117 of those arm
lines name a class in the `java/`, `javax/`, `jdk/`, `sun/` or `com/sun/`
namespace — i.e. a class that **has real JDK bytes**. The whole table mentions
279 distinct class names in those namespaces.

### 2.2 Leakage: the index crosses into native code

`native-api/src/registry.rs` gives every native two ways to reach a field:

```rust
fn get_field(&self, obj: ObjectRef, index: usize) -> Value;          // positional
fn get_field_by_name(&self, obj: ObjectRef, field_name: &str) -> Value;  // named
fn resolve_field_index(&self, class_name: &str, field_name: &str) -> Option<usize>;
```

The trait doc is candid that `index` is unvalidated and the caller owns it. The
positional form is used ~8,700 times across ~160 files. Most of those are
either VM-internal types or indices derived at runtime from
`resolve_field_index` / `FieldMetadata::slot_index`, which are fine. The audited
hazard is the subset where the index is a **literal or a hard-coded constant
naming a JDK class's field** — the `*_FIELD_*` / `*_SLOT` / `*_IDX` constant
families inventoried in §3.

The companion is the allocation side: `alloc_synthetic(ctx, "<class>", n)` and
`alloc_concurrent_synthetic(ctx, "<class>", n)` create an object of a named
class with a hard-coded slot count. Roughly 460 distinct `(class, count)` pairs
exist across 30+ files.

### 2.3 The padding path — why this is live in `--real-jdk` today

This is the part that makes the audit urgent rather than hypothetical.
`ClassManager::define_class_with_options` — the **normal real-bytecode define
path** — ends its layout computation with:

```rust
let stub_fields = synthetic_stub_fields(name);
let stub_instance_count = /* non-static count */;
let stub_total = stub_parent_fields + stub_instance_count;
let num_total_fields = num_total_fields.max(stub_total);
```

Every real class whose name appears in `synthetic_stub_fields` is **padded up to
the synthetic slot count**. Real `java.net.InetSocketAddress` is allocated with
3 slots instead of 1; real `java.util.Properties` with 16 instead of 2. The
padding exists so that natives written against the synthetic indices do not have
their writes discarded — the comment says so directly. The consequence is that
in real-JDK mode today a class carries **both** layouts simultaneously: the real
fields at their real indices, and a tail of unnamed slots, with natives writing
into whichever the constant says.

That is why "the class became real" is not the trigger for breakage. The trigger
is any change to *which* accessor wins, and `--jdk-only` changes exactly that:
concrete bytecode beats a registered bridge (contract §7 order rule 3).

### 2.4 What the existing gate does and does not check

`t9c_synthetic_field_tables_cover_their_factories` (`vm/tests/tier1_tests.rs`)
asserts, for every literal `alloc_*_synthetic(ctx, "<class>", n)` site in the
tree, that `synthetic_stub_instance_field_count(class) >= n`. It is a genuine
invariant and it catches the write-discard mode.

It says nothing about correctness against the real JDK. Both `ArrayList => 2`
and `TreeMap => 3` pass the gate while addressing the wrong fields. **A green
T9C is not evidence for this audit.**

### 2.5 The three shapes, which need different fixes

Distinguishing these matters, because only the first is a "conversion".

1. **Mis-numbered slot.** The class is real and the field exists under a name;
   only the index is wrong. → Convert to `resolve_field_index` /
   `get_field_by_name`, or a cached slot table. Mechanical.
2. **Overlay.** A VM-internal value is deliberately written on top of a real JDK
   field (`Class` mirror slot 0 = `cachedConstructor`; the `HashMap` view
   markers at 14/15/16). → There is no correct index. Move to a VM side table
   keyed by `ObjectRef`, or delete the write if the reverse map already covers
   every reader.
3. **Fabrication.** There is no real field to name, because the real type is an
   interface or the class does not exist (`ValueLayout$Of*`, the
   `org/jboss` / `io/quarkus` / `org/xnio` families). → Not a layout problem.
   It is a `CompatibilityClassRequested` violation under contract §5, and it
   closes by loading the real class or by refusing, never by renumbering.

---

## 3. Per-site table

Line numbers are given only for the three files this audit owns; everything else
is cited by file and symbol, because several of those files are being edited
concurrently this wave and line numbers will drift.

### 3.1 Owned files

| Site | Accessing code | Class(es) | Verdict | Action taken / evidence needed |
|---|---|---|---|---|
| `vm/src/vm/vm_object.rs:41–178, 337–389, 428–538` | `create_java_string*` / `try_alloc_java_string_object_from_ascii` / `populate_java_string_fields` write slots 0–3 | `java/lang/String` | **safe** | Verified: JDK 25 declares `value, coder, hash, hashIsZero` in that order under `java/lang/Object`, so 0–3 are correct. Annotated with a block anchor at the `CODER_*` constants. Not converted: hottest allocation path in the VM, indices demonstrably correct, so conversion would add risk without removing a defect. Positional dependency documented — pre-9 `String` would put `hash` at slot 1. |
| `vm/src/vm/vm_object.rs:640–727` | `read_java_string_inner` reads slots 0/1 | `java/lang/String` (speculatively: any object) | **safe** | Same layout evidence. Deliberately must stay index-based: this reader is called on receivers that may not be Strings, so a named lookup would resolve `value` off the wrong class and defeat the shape check. Annotated; the `coder ∈ {0,1}` + `num_fields >= 4` guards are what make it safe. |
| `vm/src/vm/vm_object.rs` `get_or_create_class_mirror` | `set_field(mirror, 0, Int(class_id))` | `java/lang/Class` | **unknown** (rank 6 above) | Overlay onto `cachedConstructor` (a reference). Annotated with three settling experiments: (a) call `getDeclaredConstructor` twice under `--real-jdk` and observe the `cachedConstructor != null` fast path; (b) run with `CRATONVM_DBG_OVERLAY`, whose `overlay_write_is_destructive` hunter exists for exactly this; (c) census whether `mirror_class_id`'s slot-0 fallback ever fires when the reverse map is populated. If (c) is zero, the fix is deletion, not relocation. |
| `vm/src/vm/vm_object.rs` `get_or_create_primitive_mirror` | `set_field(mirror, 0, Int(-1))` | `java/lang/Class` | **unknown** | Same overlay; resolve with the above. A primitive mirror has no legitimate `cachedConstructor` reader, so this one can move to the `primitive_mirrors` side table unconditionally once (c) is known. Annotated. |
| `vm/src/vm/vm_object.rs` `resolve_class_mirror_slots` | resolves `name` / `modifiers` / `primitive` / `reflectionData` / `classRedefinedCount` / `classLoader` **by name**, gated against the allocated field count | `java/lang/Class` | **already converted** | This is the reference implementation for the whole audit: named resolution on real bytes, a legacy fixed-slot table on the synthetic branch, and an out-of-range guard so a 2-slot synthetic mirror is never written past. Copy this shape. |
| `vm/src/vm/vm_util.rs` `stdin_stream` fallback | `set_field(in_obj, 1, Int(1))` | `java/io/FileInputStream` | **breaks-under-strict** (dead arm) | Slot 1 is `path:String`. Unreachable once real bytes are authoritative because the by-name resolve above it succeeds. Annotated; under `JdkOnly` it should become a structured failure rather than a fabricated write. |
| `vm/src/vm/vm_util.rs` `make_prepared_value_layout` | `set_field(obj, 0/1, Long)` | `java/lang/foreign/ValueLayout$Of*`, `AddressLayout` | **breaks-under-strict** (fabrication, shape 3) | Annotated at length. Explicitly marked *do not convert to named lookup* — there are no real fields to name. Wave-2 fix is to drop the preseed and let the real `<clinit>` run, which first requires `jdk/internal/misc/UnsafeConstants` to be backfilled with real platform values. |
| `vm/src/vm/vm_util.rs` clinit-cause tracer | `get_field(cause_obj, 0)` read as the detail message | `java/lang/Throwable` + subclasses | **breaks-under-strict** | **CONVERTED.** Generalised the existing `cause_idx_of` by-name closure to `field_idx_of(obj, name)` and resolved `detailMessage` through it, falling back to slot 0 so the synthetic-stub path (`_f0` = message, no name to resolve) is unchanged. Diagnostic-only path, so the conversion is low-risk and behaviour-preserving in both modes. |
| `vm/src/vm/vm_util.rs` clinit-cause tracer | `cause` index | `java/lang/Throwable` | **already converted** | Was already by-name before this change; now shares the generalised closure. |
| `vm/src/vm/vm_util.rs` ICU post-clinit fixup | `set_field(mode_impl, 0, …)` | `jdk/internal/icu/text/NormalizerBase$ModeImpl` | **safe** | Verified: `final class`, superclass `java/lang/Object`, exactly one instance field (`private final Normalizer2 normalizer2`). Slot 0 unambiguous. Annotated — note the *fixup itself* is a compatibility substitution and is in scope for the wave-2 `CompatibilityClassRequested` sweep even though its arithmetic is right. |
| `vm/src/vm/vm_util.rs` MSC post-clinit fixup | `set_field(ai_obj, 0, Int(1))`, `try_alloc_object(aid, 1)` | `java/util/concurrent/atomic/AtomicInteger` | **safe** | Verified: one instance field (`private volatile int value`); superclass `java/lang/Number` declares none. Both the index and the hard-coded count of 1 are correct. Annotated with a warning that the count does **not** generalise (`AtomicMarkableReference` / `AtomicStampedReference` are not 1-field). |
| `vm/src/vm.rs` (whole file) | ~500 `heap.get_field(obj, <literal>)` / `set_field(obj, <literal>, …)` | many | **safe** (unreachable) | Every one is inside `#[cfg(all(test, feature = "synthetic-jdk"))] mod tests`, which is 99% of the file. `synthetic-jdk` is a build-time feature excluding the real class library; `--jdk-only` is a runtime policy on a real image (contract §1). The fixtures write and then assert on their own slots, so they are self-consistent by construction. Annotated at the module header with the caveat that several fixtures encode layouts that are *wrong* for real bytes (`Throwable` message at 0, `StringBuilder` count at 1) and must never be copied into production code. |

### 3.2 Cross-tree class families — verdicts

Not owned by this audit; recorded so wave 2 has a starting inventory. "Real
slots" is `javap`-verified where stated.

| Class | Assumed | Real (JDK 25) | Verdict |
|---|---|---|---|
| `java/lang/String` | 0–3 = value/coder/hash/hashIsZero | identical | **safe** |
| `java/util/HashSet` | `HS_FIELD_MAP`@0 | `map`@0 (`AbstractSet`/`AbstractCollection` contribute none) | **safe** |
| `java/util/Optional` | `OPT_FIELD_VALUE`@0 | `value`@0 | **safe** |
| `java/util/OptionalInt/Long/Double` | present@0, value@1 | matches | **safe** — already fixed in-tree, with the breakage recorded at the constant |
| `java/time/Instant` | epochSec@0, nano@1 | `seconds`@0, `nanos`@1 | **safe** |
| `java/util/Random` | `RND_FIELD_SEED`@0 | `seed`@0 (then `nextNextGaussian`, `haveNextNextGaussian`) | **safe** for the seed |
| `java/io/ByteArrayInputStream` | buf/pos/mark/count = 0–3 | identical (`InputStream` has no instance fields) | **safe** |
| `java/io/FileDescriptor` | `fd`@0, `handle`@1 (stub is `instance_fields(4)`) | `fd`@0, `handle`@1, `parent`@2, … | **safe** for 0/1 |
| `java/lang/ref/Reference` + `Weak`/`Soft`/`Phantom` | referent@0, queue@1 | `referent`@0, `queue`@1, `next`@2, `discovered`@3 | **safe** for 0/1 |
| `java/lang/StringBuilder`/`StringBuffer` | value@0, count@1 | `value`@0, `coder`@1, `maybeLatin1`@2, `count`@3 | **breaks** — rank 1 |
| `java/lang/Throwable` + 21 subclasses | message@0, cause@1 | `backtrace`@0, `detailMessage`@1, `cause`@2, … | **breaks** — rank 2 |
| `java/util/HashMap`, `LinkedHashMap` | buckets@0, size@1, capacity@2 | `keySet`@0, `values`@1, `table`@2, `entrySet`@3, `size`@4, `modCount`@5, `threshold`@6, `loadFactor`@7 | **breaks** — rank 3, partially mitigated |
| `java/util/TreeMap`, `TreeSet` | data@0, size@1, comparator@2 | `keySet`@0, `values`@1, `comparator`@2, `root`@3, `size`@4, `modCount`@5 | **breaks** — rank 4, *not* covered by the HashMap mitigation |
| `java/util/ArrayList` | data@0, size@1 | `modCount`@0, `elementData`@1, `size`@2 | **breaks** — rank 5, half-fixed |
| `java/lang/Class` (mirror) | ClassId `Int`@0, name@1 | `cachedConstructor`@0, `name`@1 | **unknown** — rank 6 (overlay) |
| `javax/management/ObjectName` | 1 anonymous slot | 5 real instance fields | **breaks** — rank 7 |
| `java/net/InetSocketAddress` | 3 anonymous slots | `holder`@0 only | **breaks** — rank 8 |
| `java/util/Properties` | 16 anonymous, `defaults`@3 | `defaults`@0, `map`@1 | **breaks** — rank 9 |
| `java/util/regex/Matcher` | 6 anonymous | 20+ named, `parentPattern`@0 … | **breaks** — rank 10 |
| `java/lang/foreign/ValueLayout$Of*`, `AddressLayout` | 2 fabricated | **interface, 0 fields** | **breaks** (fabrication) — rank 11 |
| `java/io/BufferedOutputStream` | out@0, buf@1, count@2 | real class has an extra `closed`@1, shifting `buf`→2, `count`→3 | **mitigated** — `bos_slots()` picks at runtime; the legacy indices remain as the synthetic fallback |
| `java/nio/ByteBuffer` (and `CharBuffer`) | `BB_FIELD_*` 0–4; duplicated as `BUF_FIELD_*` in `charset.rs` and again 0–3 in `xnio_conduits.rs` | real `Buffer` layout differs | **mitigated, fragile** — `charset.rs::set_pos` does a dual write (`set_field_by_name("position")` *and* the index). Three independent copies of the same constant family is itself a defect. |
| `java/util/StringJoiner` | `SJ_FIELD_*` 0–4 | resolved by name via `SjRealLayout` | **mitigated** |
| `java/util/concurrent/ConcurrentHashMap` | `CHM_FIELD_SEGMENTS`@0, mask@1 (stub is `instance_fields(16)`) | `table`@0 … (`Object` superclass, but 8+ fields) | **unknown**, likely breaks — memory record notes CHM is allocated as `alloc_synthetic("HashSet", 1)` with no view markers, i.e. not covered by the HashMap fix |
| `java/util/concurrent/*` (locks, latches, queues, executors, `CompletableFuture`, `Phaser`, `StampedLock`) | `*_FIELD_*` constants, 1–4 slots each | not measured | **unknown** — these classes are dominated by `AbstractQueuedSynchronizer` state; needs per-class `javap` |
| `java/time/*` (`LocalDate`, `LocalTime`, `LocalDateTime`, `Duration`, `Period`, `ZoneId`, …) | `util_time.rs` `*_FIELD_*` families | only `Instant` measured | **unknown** |
| `java/lang/invoke/*` (`MethodHandle`, `VarHandle`, `MemberName`, `Lookup`) | `VH_FIELD*`, `LK_FIELD_COUNT`, … | not measured; `classloader.rs` states the `MethodHandle` synthetic slots are deliberately **anchored past** the real instance-field count | **mitigated by anchoring** for `MethodHandle`; **unknown** for the rest. Anchoring is a legitimate overlay strategy but silently breaks if the real class ever grows. |
| `javax/net/ssl/*`, `java/security/*`, `java/net/http/*` | large `*_FIELD_COUNT` families (`SSLEngine` = 14, `SSLContext` = 12) | not measured | **unknown** |
| `org/jboss/**`, `io/quarkus/**`, `io/smallrye/**`, `org/xnio/**`, agroal, ironjacamar, infinispan, cglib | ~30 distinct classes with `*_SLOT` families | **no real bytes exist** | **safe** for layout purposes — but forbidden outright under contract §1 as enterprise fallback stubs. They close via the class-fabrication route (§5 of the contract), not this audit. |

### 3.3 A separate signal: inconsistent arity for the same class

Distinct hard-coded slot counts for one real class, at different native sites,
means at least one of them is wrong about that class's shape — and the T9C gate
only checks the maximum. Worst offenders:

`java/net/URI` (1, 2, 5, 6, 7, 18) · `javax/net/ssl/SSLSession` (2, 3, 4, 6, 8) ·
`java/util/HashSet` (0, 1, 2, 3, 8) · `java/util/ArrayList` (1, 2, 3, 4) ·
`java/util/HashMap` (1, 2, 3, 8) · `javax/net/ssl/SSLContext` (1, 2, 4, 12) ·
`java/nio/ByteBuffer` (3, 5, 6) · `java/lang/foreign/MemorySegment` (2, 3, 6) ·
`java/util/Locale` (3, 32) · `java/util/Properties` (2, 16) ·
`java/lang/module/ModuleDescriptor` (2, 16) · `java/net/URL` (6, 13) ·
`java/lang/Class` (1, 2) · `java/io/FileDescriptor` (1, 4).

Treat any class in this list as **unknown until measured**, regardless of what
its `synthetic_stub_fields` arm says.

---

## 4. What could not be determined without runtime evidence

Listed with the exact experiment, so nobody has to re-derive it.

1. **Is the `java/lang/Class` mirror slot-0 overlay destructive?** Run under
   `--real-jdk` with `CRATONVM_DBG_OVERLAY`; the existing
   `overlay_write_is_destructive` hunter in `vm/src/vm/vm_exec.rs` reports a
   primitive written to a reference slot. Separately, count slot-0 fallback hits
   in `native-builtins/src/lang_class.rs::mirror_class_id` when the reverse map
   is populated. Zero hits ⇒ delete the write.
2. **Which of the 117 JDK-namespace anonymous arms are actually reached?** The
   schema-v2 native census (contract §4, `invocations` per entry) run over the
   regression suite *and* the differential corpus, cross-referenced with
   `--dump-class-origins`. A class whose stub is never invoked and whose real
   bytes always load is a free deletion; one with nonzero invocations needs the
   conversion first.
3. **Does the padding in `define_class_with_options` currently rescue any real
   class from a discarded write?** Instrument the `num_total_fields.max(stub_total)`
   line to log when `stub_total > num_total_fields` for a class with real bytes,
   then run the corpus. That list *is* the set of classes where removing the
   arm before converting the natives causes silent write loss.
4. **Do the `java/util/concurrent`, `java/time`, `java/lang/invoke`,
   `javax/net/ssl` and `java/security` constant families address the right
   fields?** Purely mechanical: `javap -p` each class across the declared
   17 / 21 / 25 matrix, walk the superclass chain, compare against the constant.
   Not done here because the answer must be per-feature-version and this audit
   had one image available.
5. **Is `java/lang/String`'s positional layout stable across 17 / 21 / 25?**
   Verified on 25 only. Cheap to confirm on the other two; must be confirmed
   before anyone widens the supported matrix downward.

---

## 5. The ordered procedure for wave 2

Per class, or per coherent subsystem — never globally. The 2026-07-14 global
stub drop was reverted the same day; this ordering is the mitigation.

1. **Measure.** From the census (§4.2), record for this class: the real field
   layout on every declared feature version, every native constant and literal
   index that addresses it, and its invocation count. If the class has real
   bytes and zero invocations, skip to step 7.
2. **Classify each site** into the three shapes of §2.5 — mis-numbered,
   overlay, fabrication. They do not share a fix and must not share a PR.
3. **Give the stub real field names.** For a mis-numbered site, replace the
   `instance_fields(n)` arm with a named list using the **real JDK field names**
   in the real order. `synthetic_stub_fields` already documents this technique
   for `java/lang/reflect/*`: naming the fields is what makes
   `set_field_by_name` / `resolve_field_index_in_hierarchy` resolve instead of
   silently no-op. This step alone is behaviour-preserving in synthetic mode
   because index order is preserved.
4. **Convert the consumers to named access.** `resolve_field_index_by_class_id`
   where the caller holds the object (it survives multi-loader ambiguity, unlike
   the by-name-class form); a cached slot table on the model of
   `resolve_class_mirror_slots` where the site is hot. Overlays move to a VM
   side table keyed by `ObjectRef`; fabrications are refused, not renumbered.
5. **Verify both modes still pass** with the padding still in place. At this
   point the class works whether it is stub or real, which is the property the
   whole ordering exists to establish.
6. **Only now remove the padding.** Delete the `synthetic_stub_fields` arm, which
   drops the `num_total_fields.max(stub_total)` inflation for that class. Any
   write that was silently landing in a pad slot now goes nowhere — which is why
   this step comes after step 4, not before it.
7. **Then drop the stub natives**, per the disposition table in
   [`jdk-only-native-review.md`](jdk-only-native-review.md), lowering the ratchet
   baseline in the same change.
8. **Re-run T9C** (`t9c_synthetic_field_tables_cover_their_factories`). It must
   stay green — but remember §2.4: green means the factory and the table agree
   with each other, not that either agrees with the JDK.

**Ordering rule, stated once:** *convert, verify, unpad, then drop.* Every
transposition of those four produces a silent failure rather than a loud one.

### A gate worth adding

Nothing currently prevents a *new* hard-coded slot constant for a real JDK class
from being introduced. The natural companion to T9C is a test that, for each
`*_FIELD_*` / `*_SLOT` constant naming a `java/`, `javax/`, `jdk/` or `sun/`
class, asserts the constant equals the slot the real class actually resolves
that field to — skipped when no real image is configured. That turns this
document from a snapshot into an invariant. It is out of scope for wave 1
(measurement, not enforcement) and is recorded here as the follow-up.

**Half of it exists as of 2026-08-05 (wave-2 lane L4).**
`classloading/src/shadow_layout.rs` diffs `synthetic_stub_fields` — §2.1's
table, the one that decides every anonymous slot's meaning — against the real
layout of the loaded image, at class-define time, and reports every index where
the two disagree under `CRATONVM_DBG=overlay`. It is a runtime census rather
than a build-time gate, and it checks the **fabrication table**, not the
`*_FIELD_*` constants in the native crates, so the follow-up above still stands
for those. But it answers §4 question 4 (*do the constant families address the
right fields?*) for every class whose model is a **named** list, and it answers
§3.3 (*inconsistent arity for the same class*) directly by printing the model
size next to the real one.

What it cannot check is the case §2.1 creates: an `instance_fields(n)` arm
declares every slot `Ljava/lang/Object;`, which is a placeholder and not a
claim, so a real *reference* field at that index is unfalsifiable. **Converting
an arm from `instance_fields(n)` to a named list is therefore not cosmetic — it
is what makes the slot checkable**, and §5 step 3 already prescribes it for a
different reason. On its first run the diff found 23 disagreeing slots across 15
classes from the arms that *are* named, including `java/lang/ThreadGroup` with
`name`/`parent` and `daemon`/`maxPriority` both transposed — plus 129 slots
where the model's placeholder reference type meets a real primitive, over 73 of
the 156 modelled classes three small probes reach.

---

## 6. Verdict summary

| Verdict | Owned-file sites | Cross-tree class families (with evidence) |
|---|---|---|
| **safe** | 6 (incl. all of `vm/src/vm.rs`) | 8 |
| **breaks-under-strict** | 3 | 8 |
| **unknown** | 2 | 5 families + the long tail |
| **already named / mitigated** | 2 | 4 |

Converted in this change: 1 (the `Throwable.detailMessage` read in
`vm/src/vm/vm_util.rs`). Annotated in place with `// JDK-ONLY-LAYOUT:`: 10.
Everything else was left alone deliberately — a clearly annotated site is a
better wave-1 deliverable than a refactor that cannot be built or tested.
