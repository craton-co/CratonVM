#[allow(unused_imports)]
use super::*;

// `sun/nio/ch/Util.getTemporaryDirectBuffer(I)Ljava/nio/ByteBuffer;` and
// `.offerFirstTemporaryDirectBuffer(Ljava/nio/ByteBuffer;)V` were REMOVED from
// this table 2026-09-22 (H2 census): this wave's own doc comment above says the
// retirement rests on single-shot lane-4 probes (`invocations > 0`, zero strict
// diff), which cannot exercise the bug that makes these two natives necessary —
// `sun/nio/ch/Util$BufferCache` is a real `ThreadLocal`, but several CratonVM
// VM-worker threads can observe the SAME cache instance, and its unsynchronised
// `count`/`start`/ring-array bytecode then hands out a `count` that claims a
// null array slot. The caller's next `buf.capacity()` NPEs with "buf is null"
// (or, depending on timing, a stale/reused buffer throws
// `BufferUnderflowException`) — reproduced on 8 H2 classes
// (`TestReadOnly`/`TestCluster`/`TestUrlJavaObjectSerializer`/
// `TestAutoReconnect`/`TestJakartaServlet`/`TestPageStoreCoverage`/
// `TestPgServer`/`TestServlet`) once `--real-jdk` became the default
// (2026-09-20) sent these two back to the retired real bytecode. Re-registered
// as `NativeKind::Intrinsic` in `native-io/src/direct_buffer.rs` — the
// reviewed-correct exception, not a `Bridge` this table's gate can downgrade —
// because this native returns exactly what the real bytecode would, just
// without the race.
//
// SUPERSEDED 2026-09-23: the race was the carrier-thread-local map, not the
// cache. All four `Util` rows are retired again, together with that map, in
// `RETIRED_SHADOW_UTIL_TEMP_BUFFER_TRIPLES` below.

/// Every retired-shadow table, in one slice, so a gate can walk the whole
/// population instead of naming one wave.
///
/// Added 2026-09-10 for
/// `registry::tests::real_layout_bridge_keeps_are_not_retired_shadows`. That
/// test needs the ROWS, not the predicate: it asks, of each retired triple,
/// whether real-JDK mode would have KEPT it had the `Bridge` -> `SyntheticStub`
/// re-tag in [`crate::registry::NativeMethodRegistry::register`] not run first.
/// A `yes` means the table is silently disarming a `keep_real_*_bridge` arm in
/// a mode the retiring lane never measured — see the "a retirement is
/// mode-blind and a keep arm is not" section on [`RETIRED_SHADOW_L5_TRIPLES`],
/// which is the case that prompted this.
///
/// **Add every new table here.** Forgetting is not caught by the sorted/unique
/// tests, which are per-table; the cost of the omission is that the new wave is
/// simply not asked the question.
/// L1 wave 9 -- the `LinkedHashMap` family and what was left of `HashMap`,
/// 168 triples, on a carrier that now carries its own state.
///
/// # The one blocker two families had been waiting on since wave 3
///
/// Wave 7 measured this family on THREE trial binaries and refused it, and the
/// refusal was not about the store: `lhm_set` mirrors `head`, `tail`, `size`
/// and `table` into the real fields, `lhm_alloc_node` binds a real
/// `java/util/LinkedHashMap$Entry`, and on the wave-7 control every one of
/// them reads back identical to HotSpot 25.0.4+7. What refused it was the
/// WRITE direction, in two forms that looked like a dilemma:
///
/// ```text
///   all 102 retired      LinkedSequencedShadowSweep DIES, 74 of 104 rows,
///                        NPE `"this.this$0" is null` in
///                        LinkedHashMap$LinkedHashIterator.nextNode
///   the 12 iterator      no crash, 6 probes worse: every mutation through a
///   rows withdrawn       SequencedMap default is LOST
/// ```
///
/// Both horns are the same object. This crate mints its map iterators under
/// real class names and drives them from five snapshot fields appended past
/// the declared ones (`key_itr_base`); the class's OWN fields -- `this$0`,
/// `next`, `current`, `expectedModCount`, `index` -- were left at their
/// allocation defaults. That is invisible while the natives registered on
/// those classes are the only reader, and a retirement is precisely what ends
/// that. Retire the iterators and a real body reads a null `this$0`; keep
/// them and a real `SequencedMap` default writes through a VM cursor into a
/// model the real map no longer reads.
///
/// `seed_real_map_itr_fields` (`native-collections/src/lib.rs`) writes those
/// fields at the mint site, out of the JDK's own constructors read off
/// `javap -p -c` on all three images. The dilemma was an artefact of a carrier
/// that had a real class name and none of its state.
///
/// # Why this is not lane T's registrar after all
///
/// §10 item 3 of the lane page has said since wave 5 that these rows need a
/// `HashSet`-family iterator carrier, and that the carrier has to come with
/// `register_hashset_natives` -- whose `SET_CLASSES` spans `java/util/` and
/// `java/util/concurrent/`, making the registrar lane T's whole. That is
/// still true of the REGISTRAR and it turned out not to be true of the
/// BLOCKER. Nothing here retires a `java/util/HashSet` or
/// `java/util/LinkedHashSet` row, and `SET_CLASSES` is untouched: the fix is
/// at the site that MINTS the carrier, which serves the set family and the map
/// family alike and is reached from both. A blocker attributed to a registrar
/// because the registrar produced the symptom.
///
/// # Scope
///
/// 168 rows: every `owns_slot && kind == "bridge"` registration under
/// `java/util/HashMap` and `java/util/LinkedHashMap` and their view, iterator
/// and node classes that is bucket A or B **on all three supported images**.
///
/// Nothing is subtracted for waves 3 and 7, and that is not an oversight: a
/// retirement re-tags the registration `SyntheticStub`, so a row an earlier
/// wave took is no longer `kind == "bridge"` and the census does not offer it.
/// The first cut of this wave subtracted 14 rows anyway, having matched them
/// against the whole of `retired_shadow.rs` including the HOLD TESTS' own
/// arrays -- lists of triples those tests exist to assert are NOT retired.
/// Three of the 14 were live and `cargo test` said so within the minute.
///
/// Eleven rows stay out because
/// jdk-17.0.20.1+1 declares none of them -- the whole JDK 21 `SequencedMap`
/// surface (`putFirst`, `putLast`, `pollFirstEntry`, `pollLastEntry`,
/// `reversed`, `sequencedKeySet`, `sequencedValues`, `sequencedEntrySet`, and
/// `reversed` on the three view carriers).
pub(in crate::retired_shadow) static RETIRED_SHADOW_L1_LHM_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/util/HashMap",
        "compute",
        "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
    ),
    (
        "java/util/HashMap",
        "computeIfPresent",
        "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
    ),
    ("java/util/HashMap", "entrySet", "()Ljava/util/Set;"),
    ("java/util/HashMap", "equals", "(Ljava/lang/Object;)Z"),
    ("java/util/HashMap", "hashCode", "()I"),
    ("java/util/HashMap", "keySet", "()Ljava/util/Set;"),
    (
        "java/util/HashMap",
        "merge",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
    ),
    (
        "java/util/HashMap",
        "readObject",
        "(Ljava/io/ObjectInputStream;)V",
    ),
    (
        "java/util/HashMap",
        "replaceAll",
        "(Ljava/util/function/BiFunction;)V",
    ),
    ("java/util/HashMap", "values", "()Ljava/util/Collection;"),
    (
        "java/util/HashMap",
        "writeObject",
        "(Ljava/io/ObjectOutputStream;)V",
    ),
    ("java/util/HashMap$EntryIterator", "hasNext", "()Z"),
    (
        "java/util/HashMap$EntryIterator",
        "next",
        "()Ljava/lang/Object;",
    ),
    ("java/util/HashMap$EntryIterator", "remove", "()V"),
    ("java/util/HashMap$EntrySet", "add", "(Ljava/lang/Object;)Z"),
    (
        "java/util/HashMap$EntrySet",
        "addAll",
        "(Ljava/util/Collection;)Z",
    ),
    ("java/util/HashMap$EntrySet", "clear", "()V"),
    (
        "java/util/HashMap$EntrySet",
        "contains",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/HashMap$EntrySet",
        "containsAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/HashMap$EntrySet",
        "equals",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/HashMap$EntrySet",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
    ),
    ("java/util/HashMap$EntrySet", "hashCode", "()I"),
    ("java/util/HashMap$EntrySet", "isEmpty", "()Z"),
    (
        "java/util/HashMap$EntrySet",
        "iterator",
        "()Ljava/util/Iterator;",
    ),
    (
        "java/util/HashMap$EntrySet",
        "remove",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/HashMap$EntrySet",
        "removeAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/HashMap$EntrySet",
        "removeIf",
        "(Ljava/util/function/Predicate;)Z",
    ),
    (
        "java/util/HashMap$EntrySet",
        "retainAll",
        "(Ljava/util/Collection;)Z",
    ),
    ("java/util/HashMap$EntrySet", "size", "()I"),
    (
        "java/util/HashMap$EntrySet",
        "spliterator",
        "()Ljava/util/Spliterator;",
    ),
    (
        "java/util/HashMap$EntrySet",
        "stream",
        "()Ljava/util/stream/Stream;",
    ),
    (
        "java/util/HashMap$EntrySet",
        "toArray",
        "()[Ljava/lang/Object;",
    ),
    (
        "java/util/HashMap$EntrySet",
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
    ),
    (
        "java/util/HashMap$EntrySet",
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
    ),
    (
        "java/util/HashMap$EntrySet",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/util/HashMap$KeyIterator", "hasNext", "()Z"),
    (
        "java/util/HashMap$KeyIterator",
        "next",
        "()Ljava/lang/Object;",
    ),
    ("java/util/HashMap$KeyIterator", "remove", "()V"),
    ("java/util/HashMap$KeySet", "add", "(Ljava/lang/Object;)Z"),
    (
        "java/util/HashMap$KeySet",
        "addAll",
        "(Ljava/util/Collection;)Z",
    ),
    ("java/util/HashMap$KeySet", "clear", "()V"),
    (
        "java/util/HashMap$KeySet",
        "contains",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/HashMap$KeySet",
        "containsAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/HashMap$KeySet",
        "equals",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/HashMap$KeySet",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
    ),
    ("java/util/HashMap$KeySet", "hashCode", "()I"),
    ("java/util/HashMap$KeySet", "isEmpty", "()Z"),
    (
        "java/util/HashMap$KeySet",
        "iterator",
        "()Ljava/util/Iterator;",
    ),
    (
        "java/util/HashMap$KeySet",
        "remove",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/HashMap$KeySet",
        "removeAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/HashMap$KeySet",
        "removeIf",
        "(Ljava/util/function/Predicate;)Z",
    ),
    (
        "java/util/HashMap$KeySet",
        "retainAll",
        "(Ljava/util/Collection;)Z",
    ),
    ("java/util/HashMap$KeySet", "size", "()I"),
    (
        "java/util/HashMap$KeySet",
        "spliterator",
        "()Ljava/util/Spliterator;",
    ),
    (
        "java/util/HashMap$KeySet",
        "stream",
        "()Ljava/util/stream/Stream;",
    ),
    (
        "java/util/HashMap$KeySet",
        "toArray",
        "()[Ljava/lang/Object;",
    ),
    (
        "java/util/HashMap$KeySet",
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
    ),
    (
        "java/util/HashMap$KeySet",
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
    ),
    (
        "java/util/HashMap$KeySet",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/HashMap$Node",
        "setValue",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    ("java/util/HashMap$ValueIterator", "hasNext", "()Z"),
    (
        "java/util/HashMap$ValueIterator",
        "next",
        "()Ljava/lang/Object;",
    ),
    ("java/util/HashMap$ValueIterator", "remove", "()V"),
    ("java/util/HashMap$Values", "clear", "()V"),
    (
        "java/util/HashMap$Values",
        "contains",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/HashMap$Values",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
    ),
    ("java/util/HashMap$Values", "isEmpty", "()Z"),
    (
        "java/util/HashMap$Values",
        "iterator",
        "()Ljava/util/Iterator;",
    ),
    (
        "java/util/HashMap$Values",
        "remove",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/HashMap$Values",
        "removeIf",
        "(Ljava/util/function/Predicate;)Z",
    ),
    ("java/util/HashMap$Values", "size", "()I"),
    (
        "java/util/HashMap$Values",
        "spliterator",
        "()Ljava/util/Spliterator;",
    ),
    (
        "java/util/HashMap$Values",
        "stream",
        "()Ljava/util/stream/Stream;",
    ),
    (
        "java/util/HashMap$Values",
        "toArray",
        "()[Ljava/lang/Object;",
    ),
    (
        "java/util/HashMap$Values",
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
    ),
    (
        "java/util/HashMap$Values",
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
    ),
    (
        "java/util/HashMap$Values",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/util/LinkedHashMap", "<init>", "()V"),
    ("java/util/LinkedHashMap", "<init>", "(I)V"),
    ("java/util/LinkedHashMap", "<init>", "(IF)V"),
    ("java/util/LinkedHashMap", "<init>", "(IFZ)V"),
    ("java/util/LinkedHashMap", "<init>", "(Ljava/util/Map;)V"),
    ("java/util/LinkedHashMap", "clear", "()V"),
    (
        "java/util/LinkedHashMap",
        "computeIfAbsent",
        "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;",
    ),
    (
        "java/util/LinkedHashMap",
        "containsKey",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/LinkedHashMap",
        "containsValue",
        "(Ljava/lang/Object;)Z",
    ),
    ("java/util/LinkedHashMap", "entrySet", "()Ljava/util/Set;"),
    (
        "java/util/LinkedHashMap",
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
    ),
    (
        "java/util/LinkedHashMap",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/LinkedHashMap",
        "getOrDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    ("java/util/LinkedHashMap", "isEmpty", "()Z"),
    ("java/util/LinkedHashMap", "keySet", "()Ljava/util/Set;"),
    (
        "java/util/LinkedHashMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    ("java/util/LinkedHashMap", "putAll", "(Ljava/util/Map;)V"),
    (
        "java/util/LinkedHashMap",
        "putIfAbsent",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/LinkedHashMap",
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/LinkedHashMap",
        "remove",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
    ),
    (
        "java/util/LinkedHashMap",
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/LinkedHashMap",
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z",
    ),
    ("java/util/LinkedHashMap", "size", "()I"),
    (
        "java/util/LinkedHashMap",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/LinkedHashMap",
        "values",
        "()Ljava/util/Collection;",
    ),
    (
        "java/util/LinkedHashMap$Entry",
        "setValue",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/LinkedHashMap$LinkedEntryIterator",
        "hasNext",
        "()Z",
    ),
    (
        "java/util/LinkedHashMap$LinkedEntryIterator",
        "next",
        "()Ljava/lang/Object;",
    ),
    (
        "java/util/LinkedHashMap$LinkedEntryIterator",
        "remove",
        "()V",
    ),
    (
        "java/util/LinkedHashMap$LinkedEntrySet",
        "add",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/LinkedHashMap$LinkedEntrySet",
        "addAll",
        "(Ljava/util/Collection;)Z",
    ),
    ("java/util/LinkedHashMap$LinkedEntrySet", "clear", "()V"),
    (
        "java/util/LinkedHashMap$LinkedEntrySet",
        "contains",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/LinkedHashMap$LinkedEntrySet",
        "containsAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/LinkedHashMap$LinkedEntrySet",
        "equals",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/LinkedHashMap$LinkedEntrySet",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
    ),
    ("java/util/LinkedHashMap$LinkedEntrySet", "hashCode", "()I"),
    ("java/util/LinkedHashMap$LinkedEntrySet", "isEmpty", "()Z"),
    (
        "java/util/LinkedHashMap$LinkedEntrySet",
        "iterator",
        "()Ljava/util/Iterator;",
    ),
    (
        "java/util/LinkedHashMap$LinkedEntrySet",
        "remove",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/LinkedHashMap$LinkedEntrySet",
        "removeAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/LinkedHashMap$LinkedEntrySet",
        "removeIf",
        "(Ljava/util/function/Predicate;)Z",
    ),
    (
        "java/util/LinkedHashMap$LinkedEntrySet",
        "retainAll",
        "(Ljava/util/Collection;)Z",
    ),
    ("java/util/LinkedHashMap$LinkedEntrySet", "size", "()I"),
    (
        "java/util/LinkedHashMap$LinkedEntrySet",
        "spliterator",
        "()Ljava/util/Spliterator;",
    ),
    (
        "java/util/LinkedHashMap$LinkedEntrySet",
        "stream",
        "()Ljava/util/stream/Stream;",
    ),
    (
        "java/util/LinkedHashMap$LinkedEntrySet",
        "toArray",
        "()[Ljava/lang/Object;",
    ),
    (
        "java/util/LinkedHashMap$LinkedEntrySet",
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
    ),
    (
        "java/util/LinkedHashMap$LinkedEntrySet",
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
    ),
    (
        "java/util/LinkedHashMap$LinkedEntrySet",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/LinkedHashMap$LinkedKeyIterator",
        "hasNext",
        "()Z",
    ),
    (
        "java/util/LinkedHashMap$LinkedKeyIterator",
        "next",
        "()Ljava/lang/Object;",
    ),
    ("java/util/LinkedHashMap$LinkedKeyIterator", "remove", "()V"),
    (
        "java/util/LinkedHashMap$LinkedKeySet",
        "add",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/LinkedHashMap$LinkedKeySet",
        "addAll",
        "(Ljava/util/Collection;)Z",
    ),
    ("java/util/LinkedHashMap$LinkedKeySet", "clear", "()V"),
    (
        "java/util/LinkedHashMap$LinkedKeySet",
        "contains",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/LinkedHashMap$LinkedKeySet",
        "containsAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/LinkedHashMap$LinkedKeySet",
        "equals",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/LinkedHashMap$LinkedKeySet",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
    ),
    ("java/util/LinkedHashMap$LinkedKeySet", "hashCode", "()I"),
    ("java/util/LinkedHashMap$LinkedKeySet", "isEmpty", "()Z"),
    (
        "java/util/LinkedHashMap$LinkedKeySet",
        "iterator",
        "()Ljava/util/Iterator;",
    ),
    (
        "java/util/LinkedHashMap$LinkedKeySet",
        "remove",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/LinkedHashMap$LinkedKeySet",
        "removeAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/LinkedHashMap$LinkedKeySet",
        "removeIf",
        "(Ljava/util/function/Predicate;)Z",
    ),
    (
        "java/util/LinkedHashMap$LinkedKeySet",
        "retainAll",
        "(Ljava/util/Collection;)Z",
    ),
    ("java/util/LinkedHashMap$LinkedKeySet", "size", "()I"),
    (
        "java/util/LinkedHashMap$LinkedKeySet",
        "spliterator",
        "()Ljava/util/Spliterator;",
    ),
    (
        "java/util/LinkedHashMap$LinkedKeySet",
        "stream",
        "()Ljava/util/stream/Stream;",
    ),
    (
        "java/util/LinkedHashMap$LinkedKeySet",
        "toArray",
        "()[Ljava/lang/Object;",
    ),
    (
        "java/util/LinkedHashMap$LinkedKeySet",
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
    ),
    (
        "java/util/LinkedHashMap$LinkedKeySet",
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
    ),
    (
        "java/util/LinkedHashMap$LinkedKeySet",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/LinkedHashMap$LinkedValueIterator",
        "hasNext",
        "()Z",
    ),
    (
        "java/util/LinkedHashMap$LinkedValueIterator",
        "next",
        "()Ljava/lang/Object;",
    ),
    (
        "java/util/LinkedHashMap$LinkedValueIterator",
        "remove",
        "()V",
    ),
    ("java/util/LinkedHashMap$LinkedValues", "clear", "()V"),
    (
        "java/util/LinkedHashMap$LinkedValues",
        "contains",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/LinkedHashMap$LinkedValues",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
    ),
    ("java/util/LinkedHashMap$LinkedValues", "isEmpty", "()Z"),
    (
        "java/util/LinkedHashMap$LinkedValues",
        "iterator",
        "()Ljava/util/Iterator;",
    ),
    (
        "java/util/LinkedHashMap$LinkedValues",
        "remove",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/LinkedHashMap$LinkedValues",
        "removeIf",
        "(Ljava/util/function/Predicate;)Z",
    ),
    ("java/util/LinkedHashMap$LinkedValues", "size", "()I"),
    (
        "java/util/LinkedHashMap$LinkedValues",
        "spliterator",
        "()Ljava/util/Spliterator;",
    ),
    (
        "java/util/LinkedHashMap$LinkedValues",
        "stream",
        "()Ljava/util/stream/Stream;",
    ),
    (
        "java/util/LinkedHashMap$LinkedValues",
        "toArray",
        "()[Ljava/lang/Object;",
    ),
    (
        "java/util/LinkedHashMap$LinkedValues",
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
    ),
    (
        "java/util/LinkedHashMap$LinkedValues",
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
    ),
    (
        "java/util/LinkedHashMap$LinkedValues",
        "toString",
        "()Ljava/lang/String;",
    ),
];

/// L1 wave 9 -- `java/util/Date`, 19 triples.
///
/// # `sun/util/calendar/ZoneInfo`'s nine rows were WITHDRAWN 2026-09-17
///
/// This table originally retired 28 rows: the 19 `java/util/Date` rows below,
/// plus all nine `Bridge`-category `sun/util/calendar/ZoneInfo` triples
/// (`getDSTSavings`, `useDaylightTime`, `observesDaylightTime`,
/// `inDaylightTime`, the six-arg `getOffset`, the one-arg `getOffset`,
/// `getOffsets`, `getOffsetsByWall`, `getRawOffset`). MEASURED on a fresh
/// `--real-jdk` build off `origin/dev`: with those nine retired, `register()`
/// refuses ALL of them under `--real-jdk` (a `SyntheticStub` is refused
/// without being inserted, so the class does not appear in
/// `--dump-native-registry` at all -- `grep -c 'sun/util/calendar'
/// reg.json` reads 0), and real JDK bytecode runs every DST accessor against
/// `alloc_synth_timezone`'s fabricated `ZoneInfo`, whose `transitions` array
/// is null and always has been. That is the EXACT defect
/// `G17-1-the-dst-family-and-the-fixture-that-compared-two-empty-strings-20260817.md`
/// and `G28-1-the-dst-rule-layer-rebuilt-20260817.md` fixed with the five
/// natives in `native-builtins/src/tzdb.rs::register_zoneinfo_dst_natives`,
/// reintroduced whole: `RSimpleDateFormatZone` throws
/// `getTimeZone("America/New_York").getOffset(JUL) must be -14400000 (DST)`
/// and `RSimpleTimeZoneRaw` reads `checks=393 fails=93`, both 100% repro,
/// both alone, on a low-loaded host -- not the `RMapGcStress` flake shape.
///
/// This table's own doc, below, already named the risk and reasoned past it:
/// `LocaleDateTzShadowSweep` armed ONE class at a time read
/// `sun/util/calendar/ZoneInfo` at **+4 WORSE**, and wave 9 retired it anyway
/// on a DIFFERENT, broader probe's **+0** reading (`L1LocaleProviderWorkload`,
/// 147 rows over multiple prefixes at once). §7's own words for that shape:
/// "a dial declines at the dispatch door while a retirement deletes the
/// registration" -- and "Convert a dial result into a table entry only via a
/// registry dump **from a run of the very probe whose improvement you are
/// citing**" (`docs/feature-designs/native-dispatch-architecture.md` §"And the dial
/// is not the retirement"). `L1LocaleProviderWorkload`'s 411 engagements on
/// `sun/util/calendar/` were never the DST family's own contract edges --
/// `RSimpleTimeZoneRaw` is -- and the per-class **+4 WORSE** reading was the
/// one that turned out to be right.
///
/// # Two readings of the same family, and neither of them a verdict
///
/// `LocaleDateTzShadowSweep` armed one class at a time read
/// `sun/util/calendar/ZoneInfo` at **+4 WORSE**, and that is the reading §1 of
/// the lane page held this family on. Wave 6 then built
/// `apps/probes/L1LocaleProviderWorkload` -- 147 rows driving every public API
/// whose real-JDK implementation goes through these classes -- and read the
/// same prefixes at **+0**, with the engagement counter saying the probe had
/// actually asked:
///
/// ```text
///   java/util/Date          +0   reached=14
///   sun/util/calendar/      +0   reached=411
/// ```
///
/// Both are DIAL readings, and a dial declines at the dispatch door while a
/// retirement deletes the registration. That is why §12 of the lane page has
/// carried these as "candidates with no trial binary yet" for two waves: the
/// only instrument that can answer is a second binary, and this table is it.
/// **The `ZoneInfo` half of that binary is now measured, above, and the
/// answer is NO.**
///
/// # `java/util/TimeZone` is NOT here, and was censused with the rest
///
/// Ten of its rows are bucket A or B on all three images and it would have
/// been the natural third class. It stays out because it is the one member of
/// this family with a measurement AGAINST it that a dial artefact does not
/// explain: `the_l1_families_measured_load_bearing_are_still_held` records
/// `java/util/TimeZone.getID()Ljava/lang/String;` as measured load-bearing by
/// L1 wave 1, and `LocaleDateTzShadowSweep` read the class at **+2 WORSE**.
/// Wave 6's `+0` at reached=196 is evidence the other way, and two readings
/// that disagree are a reason to spend a binary on it alone rather than to
/// fold it into a table with rows that do not need one.
///
/// # Scope
///
/// 19 rows: every `owns_slot && kind == "bridge"` registration under
/// `java/util/Date` that is bucket A or B on all three supported images --
/// jdk-17.0.20.1+1, jdk-21.0.12+8 and jdk-25.0.4+7, censused separately.
/// `sun/util/calendar/ZoneInfoFile` contributes nothing: wave 5 took its two
/// rows (`RETIRED_SHADOW_L1_ZI_TRIPLES`) and no other row of that class is a
/// `Bridge` this crate owns.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L1_DATE_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/util/Date", "<init>", "(III)V"),
    ("java/util/Date", "<init>", "(IIIII)V"),
    ("java/util/Date", "<init>", "(IIIIII)V"),
    ("java/util/Date", "<init>", "(Ljava/lang/String;)V"),
    ("java/util/Date", "getDate", "()I"),
    ("java/util/Date", "getDay", "()I"),
    ("java/util/Date", "getHours", "()I"),
    ("java/util/Date", "getMinutes", "()I"),
    ("java/util/Date", "getMonth", "()I"),
    ("java/util/Date", "getSeconds", "()I"),
    ("java/util/Date", "getTimezoneOffset", "()I"),
    ("java/util/Date", "getYear", "()I"),
    ("java/util/Date", "setDate", "(I)V"),
    ("java/util/Date", "setHours", "(I)V"),
    ("java/util/Date", "setMinutes", "(I)V"),
    ("java/util/Date", "setMonth", "(I)V"),
    ("java/util/Date", "setSeconds", "(I)V"),
    ("java/util/Date", "setYear", "(I)V"),
    ("java/util/Date", "toGMTString", "()Ljava/lang/String;"),
    // The nine `sun/util/calendar/ZoneInfo` rows that were here (getDSTSavings,
    // both getOffset overloads, getOffsets, getOffsetsByWall, getRawOffset,
    // inDaylightTime, observesDaylightTime, useDaylightTime) were WITHDRAWN
    // 2026-09-17: see this table's own doc comment above for the measurement.
];

/// `java/io/FileDescriptor$1.close(FileDescriptor)` -- the JDK's own
/// `JavaIOFileDescriptorAccess.close`, retired 2026-09-23.
///
/// The registration it retires was a NO-OP (`jiofd_noop`, "CratonVM owns fd
/// lifetime through the fd_table + explicit channel close natives"). That
/// premise stopped being true for every channel real bytecode builds: under
/// `--real-jdk` `sun.nio.fs.UnixChannelFactory` opens the descriptor through
/// `UnixNativeDispatcher.open0` (an `fd_table` entry) and the REAL
/// `FileChannelImpl.implCloseChannel` releases it through exactly this hook --
/// `closer.clean()` -> `FileChannelImpl$Closer.run` -> `fdAccess.close(fd)`. The
/// no-op answered, nothing closed, and every `FileChannel.open(..).close()` and
/// `Files.newInputStream(..).close()` leaked its descriptor. MEASURED on
/// Temurin 25.0.4 / linux-x64 (`FdLeakProbe`, 300 iterations each): HotSpot
/// leaks 0 in every arm, CratonVM leaked 300 for `FileChannel.open`, 300 for
/// `open`+`map`, 300 for `Files.newInputStream`, and 0 for `RandomAccessFile`
/// (which closes through `FileDescriptor.closeAll`, never this hook). H2's
/// `TestFullText` (Lucene opening an index file per segment read) ran out of
/// descriptors in under ten seconds: `FileSystemException: Too many open
/// files`, then `NoClassDefFoundError` from a shutdown hook that could no
/// longer open a class file.
///
/// The real body is `Objects.requireNonNull(fdo); fdo.close();`, and
/// `FileDescriptor.close()` is `unregisterCleanup(); close0();` --
/// `close0` is this VM's `native_fd_close0`, which releases the `fd_table`
/// entry and latches `fd = -1`/`handle = -1`, so a second close of the same
/// descriptor (the `parent` stream closing after the channel, or a
/// `Closer` racing an explicit close) is a no-op exactly as on HotSpot.
///
/// **It completes a pair wave 17 half-retired.** `registerCleanup(FileDescriptor)`
/// has run real bytecode since `RETIRED_SHADOW_L4_BUFFERS2_TRIPLES`
/// (2026-09-19), so every real stream already armed a GC-time
/// `FileCleanable` -- while this no-op `close` never released the
/// descriptor and never cleared that cleanable. The real `close` does both
/// in HotSpot's order (`FileDescriptor.close()` is `unregisterCleanup();
/// close0();`), so an explicitly closed descriptor can no longer be closed a
/// second time BY NUMBER when its cleanable fires after the number was reused.
/// The two-argument `registerCleanup(FileDescriptor, PhantomCleanable)` and
/// the `unregisterCleanup` hook stay native: nothing measured them, and this
/// table is one row on purpose. The GC-time close of an UNCLOSED channel
/// still happens: `FileChannelImpl` registers its own `Closer` with the
/// common cleaner, not through this hook, and that `Closer` now reaches a real
/// close.
///
/// Found a second time, the same day, by the archive-provider wave above,
/// which makes every classpath jar a real `ZipFileSystem` over such a channel:
/// an in-process `javac` per Spring `TestCompiler` compile then ran out of
/// descriptors at the host's 1024 limit (`cannot access com.example ... Too
/// many open files`). MEASURED on Azure linux, JDK 25.0.4+7, `--real-jdk`: 300
/// open/close cycles of `Files.newByteChannel` or `FileChannel.open` held 300
/// descriptors (HotSpot and `--compatible`: 0)
/// (`h2-inprocess-javac-jrt-modules-listing-gap-FIXED-20260923.md`).
///
/// `nonpassed-classbyclass-census-RESOLVED-20260923.md`
pub(in crate::retired_shadow) static RETIRED_SHADOW_FD_ACCESS_CLOSE_TRIPLES: &[(&str, &str, &str)] = &[(
    "java/io/FileDescriptor$1",
    "close",
    "(Ljava/io/FileDescriptor;)V",
)];

/// `JavaLangAccess.{get,set,remove}CarrierThreadLocal` and
/// `.isCarrierThreadLocalPresent` on the JLA carrier (`java/lang/System$1` on
/// 25, `$2` on images that numbered it so), retired 2026-09-23.
///
/// The natives kept every `CarrierThreadLocal` value in ONE process-wide map
/// keyed by the ThreadLocal's identity hash -- no thread in the key -- so a
/// carrier-thread-local was a process global, and every thread shared one
/// value. The JDK's own body is `((ThreadLocal) local).getCarrierThreadLocal()`
/// (and the matching set/remove/isPresent), i.e. `get(Thread.
/// currentCarrierThread())` over that thread's real `threadLocals` map.
///
/// The visible casualty was `sun.nio.ch.Util.bufferCache`, a
/// `TerminatingThreadLocal`: every thread in the process shared one
/// `Util$BufferCache`, whose unsynchronised ring then handed out a null slot
/// (`NullPointerException: ... because "buf" is null`) or the same buffer to
/// two threads at once. MEASURED on Temurin 25.0.4 / linux-x64
/// (`UtilShareProbe`, 8 threads x 3000 `getTemporaryDirectBuffer`/
/// `releaseTemporaryDirectBuffer`): HotSpot 0 buffers held by two threads at
/// once; this VM over these natives 7,883, plus 8,014 NPEs; with these rows
/// retired, 0 and 0. `sun.nio.fs.NativeBuffers` and `sun.nio.ch.IOVecWrapper`
/// are `TerminatingThreadLocal`s too, and were shared the same way.
///
/// The `jdk/internal/access/JavaLangAccess` interface registrations of the
/// same four methods are outside every admitted prefix and stay registered;
/// under `--real-jdk` their bodies delegate to the same real `ThreadLocal`
/// methods (`shared_secrets_bridge.rs`), so neither door keeps the shared map.
///
/// `nonpassed-classbyclass-census-RESOLVED-20260923.md`
pub(in crate::retired_shadow) static RETIRED_SHADOW_JLA_CARRIER_TL_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/lang/System$1",
        "getCarrierThreadLocal",
        "(Ljdk/internal/misc/CarrierThreadLocal;)Ljava/lang/Object;",
    ),
    (
        "java/lang/System$1",
        "isCarrierThreadLocalPresent",
        "(Ljdk/internal/misc/CarrierThreadLocal;)Z",
    ),
    (
        "java/lang/System$1",
        "removeCarrierThreadLocal",
        "(Ljdk/internal/misc/CarrierThreadLocal;)V",
    ),
    (
        "java/lang/System$1",
        "setCarrierThreadLocal",
        "(Ljdk/internal/misc/CarrierThreadLocal;Ljava/lang/Object;)V",
    ),
    (
        "java/lang/System$2",
        "getCarrierThreadLocal",
        "(Ljdk/internal/misc/CarrierThreadLocal;)Ljava/lang/Object;",
    ),
    (
        "java/lang/System$2",
        "isCarrierThreadLocalPresent",
        "(Ljdk/internal/misc/CarrierThreadLocal;)Z",
    ),
    (
        "java/lang/System$2",
        "removeCarrierThreadLocal",
        "(Ljdk/internal/misc/CarrierThreadLocal;)V",
    ),
    (
        "java/lang/System$2",
        "setCarrierThreadLocal",
        "(Ljdk/internal/misc/CarrierThreadLocal;Ljava/lang/Object;)V",
    ),
];

/// `sun/nio/ch/Util`'s temporary-direct-buffer pool, retired 2026-09-23.
///
/// These four natives replace `Util$BufferCache` with a per-native-thread pool
/// because the real cache was observed shared between threads. It was -- by
/// the carrier-thread-local map `RETIRED_SHADOW_JLA_CARRIER_TL_TRIPLES`
/// retires, not by anything in `Util`. With that table in force the real
/// bytecode is correct: `UtilShareProbe` 0 shared buffers and 0 NPEs,
/// `BufRaceProbe` (8 threads x 3000 positional reads/writes through one
/// `FileChannel` each) 0 failures, where the same bytecode over the shared map
/// failed 8,347 of 24,000. The two tables are one decision and must move
/// together: this one without the carrier table reinstates the census's
/// `"buf" is null` crash on every multi-threaded NIO user.
///
/// Two of these rows (`getTemporaryDirectBuffer`,
/// `offerFirstTemporaryDirectBuffer`) were in `RETIRED_SHADOW_L4_BUFFERS2_TRIPLES`
/// until 2026-09-22 and were withdrawn when the crash surfaced; see the note
/// after that table.
///
/// `nonpassed-classbyclass-census-RESOLVED-20260923.md`
pub(in crate::retired_shadow) static RETIRED_SHADOW_UTIL_TEMP_BUFFER_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "sun/nio/ch/Util",
        "getTemporaryDirectBuffer",
        "(I)Ljava/nio/ByteBuffer;",
    ),
    (
        "sun/nio/ch/Util",
        "offerFirstTemporaryDirectBuffer",
        "(Ljava/nio/ByteBuffer;)V",
    ),
    (
        "sun/nio/ch/Util",
        "offerLastTemporaryDirectBuffer",
        "(Ljava/nio/ByteBuffer;)V",
    ),
    (
        "sun/nio/ch/Util",
        "releaseTemporaryDirectBuffer",
        "(Ljava/nio/ByteBuffer;)V",
    ),
];

/// `java/lang/StringBuilder` + `java/lang/AbstractStringBuilder`, 2026-09-21 --
/// 118 of the 124 `Bridge` registrations `--dump-native-registry` reports for
/// the two classes, closing `H14-2`/`H14-3`'s last unretired "free" family
/// (`H14-3` N2, priced at zero corpus cost the same day `H22-2` found it
/// FATAL) and the open item the `StringBuilder` JIT-intrinsic lane
/// (`docs/feature-designs/jit-ir-optimizations.md`)
/// explicitly left for "a future lane": landing the rows into this file.
///
/// # The list is machine-generated, not hand-picked
///
/// Every triple below has `owns_slot: true` and a `real_declaring_method` that
/// is `loaded`, `declared` on THAT exact class, `has_code` and NOT
/// `acc_native` -- i.e. real JDK 25 bytecode this class itself owns, not one
/// it merely inherits. The filter excludes exactly six of the 124 rows, and
/// each exclusion matches a `lang_string.rs` comment written before this table
/// existed:
///
/// * `AbstractStringBuilder.toString()` -- `public abstract`, no `Code`
///   attribute on any supported image (`register_string_builder_natives`'s own
///   "DO NOT RETIRE" comment, `H14-1` §4/`H25-2`'s two-row "no implementation"
///   bucket).
/// * `StringBuilder.append(AbstractStringBuilder)`, `.appendNull()`,
///   `.getCoder()`, `.getValue()`, `.repeat(CI)` -- registered on
///   `StringBuilder` by the shared `register_string_builder_natives(registry,
///   class)` helper, but NOT declared by `StringBuilder` itself (only by
///   `AbstractStringBuilder`, whose own copies of the same four methods ARE in
///   this table). Retiring the `StringBuilder`-keyed registration would be
///   retiring a triple with no receiver of its own; dispatch already resolves
///   these through the real `AbstractStringBuilder` inheritance chain once the
///   `AbstractStringBuilder` rows below stop shadowing it. (Round 13 wave 13
///   found that last sentence false for the virtual-invoke cache, which looks
///   the native up on the receiver's own class, and retired the five in
///   [`RETIRED_SHADOW_SB_INHERITED_FIVE_TRIPLES`].)
///
/// # `H22-2`'s FATAL result does not reproduce today -- MEASURED, not argued
///
/// `H22-2` (2026-08-21) armed exactly these two classes and got `rc=1`,
/// `ArrayStoreException: can not copy byte[] into char[]`, on the theory that
/// CratonVM's `char[]`/`count` builder cannot run JDK 25's compact-string
/// `AbstractStringBuilder` bytecode. `lang_string.rs` still carries several
/// comments repeating that warning verbatim for `codePointAt`/`codePointBefore`/
/// `codePointCount`/`appendCodePoint`, `getValue`/`getCoder`/`getChars`, the
/// four scalar `insert` overloads and the two `repeat` overloads named there --
/// every one of which IS in this table. Before trusting either the six-week-old
/// crash or the five-week-old comments over the other, this table was preceded
/// by a fresh differential probe (`SbLayoutRisk.java`, this session, on a clean
/// build of this exact commit) exercising precisely those flagged methods --
/// `codePointAt`, `getChars`, `insert(int,boolean)`, `insert(int,double)`,
/// `repeat(char,int)`, `repeat(CharSequence,int)` -- run three ways:
///
/// ```text
///   HotSpot 25.0.4+7                                byte-identical
///   CratonVM, unarmed                                byte-identical
///   CratonVM, --real-jdk + CRATONVM_ENFORCE_NATIVE_SHADOW=
///     java/lang/StringBuilder,java/lang/AbstractStringBuilder   byte-identical
///       [DIAL_DOOR_CENSUS] armed=true reached=102 yielded=102 leaked=0
/// ```
///
/// Zero leaked natives, zero divergence, on the arm that is strictly the same
/// shape as this table (per-triple re-tag to `SyntheticStub`, refused under
/// `--real-jdk`). The crash `H22-2` found does not reproduce against the
/// current object layout; the `lang_string.rs` warnings are stale documentation
/// of a defect that was fixed elsewhere in the five weeks since, and nobody
/// removed the comments. Two later lanes had already measured the same
/// direction more broadly and less precisely: lane 2
/// (`docs/feature-designs/native-dispatch-architecture.md`, dial-armed,
/// 40/0 corpus) and the JIT-intrinsic lane (2026-09-17, full `SUITE=all`
/// 136/0 and `SUITE=core` 95/0 with the same two classes armed) -- both
/// correctly flagged as "a clean arm means the corpus did not ask" rather than
/// proof, which is why this table's own probe targets the exact methods their
/// suites do not happen to exercise.
///
/// docs/feature-designs/native-dispatch-architecture.md
pub(in crate::retired_shadow) static RETIRED_SHADOW_STRINGBUILDER_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/AbstractStringBuilder", "<init>", "()V"),
    ("java/lang/AbstractStringBuilder", "<init>", "(I)V"),
    (
        "java/lang/AbstractStringBuilder",
        "<init>",
        "(Ljava/lang/CharSequence;)V",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "append",
        "(C)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "append",
        "(C)Ljava/lang/Appendable;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "append",
        "(D)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "append",
        "(F)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "append",
        "(I)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "append",
        "(J)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "append",
        "(Ljava/lang/AbstractStringBuilder;)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "append",
        "(Ljava/lang/CharSequence;)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "append",
        "(Ljava/lang/CharSequence;)Ljava/lang/Appendable;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "append",
        "(Ljava/lang/CharSequence;II)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "append",
        "(Ljava/lang/CharSequence;II)Ljava/lang/Appendable;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "append",
        "(Ljava/lang/Object;)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "append",
        "(Ljava/lang/String;)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "append",
        "(Ljava/lang/StringBuffer;)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "append",
        "(Z)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "append",
        "([C)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "append",
        "([CII)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "appendCodePoint",
        "(I)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "appendNull",
        "()Ljava/lang/AbstractStringBuilder;",
    ),
    ("java/lang/AbstractStringBuilder", "capacity", "()I"),
    ("java/lang/AbstractStringBuilder", "charAt", "(I)C"),
    ("java/lang/AbstractStringBuilder", "codePointAt", "(I)I"),
    ("java/lang/AbstractStringBuilder", "codePointBefore", "(I)I"),
    ("java/lang/AbstractStringBuilder", "codePointCount", "(II)I"),
    (
        "java/lang/AbstractStringBuilder",
        "delete",
        "(II)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "deleteCharAt",
        "(I)Ljava/lang/AbstractStringBuilder;",
    ),
    ("java/lang/AbstractStringBuilder", "ensureCapacity", "(I)V"),
    ("java/lang/AbstractStringBuilder", "getChars", "(II[CI)V"),
    ("java/lang/AbstractStringBuilder", "getCoder", "()B"),
    ("java/lang/AbstractStringBuilder", "getValue", "()[B"),
    (
        "java/lang/AbstractStringBuilder",
        "indexOf",
        "(Ljava/lang/String;)I",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "indexOf",
        "(Ljava/lang/String;I)I",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "insert",
        "(IC)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "insert",
        "(ID)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "insert",
        "(IF)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "insert",
        "(II)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "insert",
        "(IJ)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "insert",
        "(ILjava/lang/CharSequence;)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "insert",
        "(ILjava/lang/CharSequence;II)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "insert",
        "(ILjava/lang/Object;)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "insert",
        "(ILjava/lang/String;)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "insert",
        "(IZ)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "insert",
        "(I[C)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "insert",
        "(I[CII)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "lastIndexOf",
        "(Ljava/lang/String;)I",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "lastIndexOf",
        "(Ljava/lang/String;I)I",
    ),
    ("java/lang/AbstractStringBuilder", "length", "()I"),
    (
        "java/lang/AbstractStringBuilder",
        "repeat",
        "(CI)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "repeat",
        "(II)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "repeat",
        "(Ljava/lang/CharSequence;I)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "replace",
        "(IILjava/lang/String;)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "reverse",
        "()Ljava/lang/AbstractStringBuilder;",
    ),
    ("java/lang/AbstractStringBuilder", "setCharAt", "(IC)V"),
    ("java/lang/AbstractStringBuilder", "setLength", "(I)V"),
    (
        "java/lang/AbstractStringBuilder",
        "substring",
        "(I)Ljava/lang/String;",
    ),
    (
        "java/lang/AbstractStringBuilder",
        "substring",
        "(II)Ljava/lang/String;",
    ),
    ("java/lang/AbstractStringBuilder", "trimToSize", "()V"),
    ("java/lang/StringBuilder", "<init>", "()V"),
    ("java/lang/StringBuilder", "<init>", "(I)V"),
    (
        "java/lang/StringBuilder",
        "<init>",
        "(Ljava/lang/CharSequence;)V",
    ),
    ("java/lang/StringBuilder", "<init>", "(Ljava/lang/String;)V"),
    (
        "java/lang/StringBuilder",
        "append",
        "(C)Ljava/lang/Appendable;",
    ),
    (
        "java/lang/StringBuilder",
        "append",
        "(C)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "append",
        "(D)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "append",
        "(F)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "append",
        "(I)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "append",
        "(J)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "append",
        "(Ljava/lang/CharSequence;)Ljava/lang/Appendable;",
    ),
    (
        "java/lang/StringBuilder",
        "append",
        "(Ljava/lang/CharSequence;)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "append",
        "(Ljava/lang/CharSequence;II)Ljava/lang/Appendable;",
    ),
    (
        "java/lang/StringBuilder",
        "append",
        "(Ljava/lang/CharSequence;II)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "append",
        "(Ljava/lang/Object;)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "append",
        "(Ljava/lang/String;)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "append",
        "(Ljava/lang/StringBuffer;)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "append",
        "(Z)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "append",
        "([C)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "append",
        "([CII)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "appendCodePoint",
        "(I)Ljava/lang/StringBuilder;",
    ),
    ("java/lang/StringBuilder", "capacity", "()I"),
    ("java/lang/StringBuilder", "charAt", "(I)C"),
    ("java/lang/StringBuilder", "codePointAt", "(I)I"),
    ("java/lang/StringBuilder", "codePointBefore", "(I)I"),
    ("java/lang/StringBuilder", "codePointCount", "(II)I"),
    (
        "java/lang/StringBuilder",
        "delete",
        "(II)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "deleteCharAt",
        "(I)Ljava/lang/StringBuilder;",
    ),
    ("java/lang/StringBuilder", "ensureCapacity", "(I)V"),
    ("java/lang/StringBuilder", "getChars", "(II[CI)V"),
    (
        "java/lang/StringBuilder",
        "indexOf",
        "(Ljava/lang/String;)I",
    ),
    (
        "java/lang/StringBuilder",
        "indexOf",
        "(Ljava/lang/String;I)I",
    ),
    (
        "java/lang/StringBuilder",
        "insert",
        "(IC)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "insert",
        "(ID)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "insert",
        "(IF)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "insert",
        "(II)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "insert",
        "(IJ)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "insert",
        "(ILjava/lang/CharSequence;)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "insert",
        "(ILjava/lang/CharSequence;II)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "insert",
        "(ILjava/lang/Object;)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "insert",
        "(ILjava/lang/String;)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "insert",
        "(IZ)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "insert",
        "(I[C)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "insert",
        "(I[CII)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "lastIndexOf",
        "(Ljava/lang/String;)I",
    ),
    (
        "java/lang/StringBuilder",
        "lastIndexOf",
        "(Ljava/lang/String;I)I",
    ),
    ("java/lang/StringBuilder", "length", "()I"),
    (
        "java/lang/StringBuilder",
        "repeat",
        "(II)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "repeat",
        "(Ljava/lang/CharSequence;I)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "replace",
        "(IILjava/lang/String;)Ljava/lang/StringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "reverse",
        "()Ljava/lang/StringBuilder;",
    ),
    ("java/lang/StringBuilder", "setCharAt", "(IC)V"),
    ("java/lang/StringBuilder", "setLength", "(I)V"),
    (
        "java/lang/StringBuilder",
        "substring",
        "(I)Ljava/lang/String;",
    ),
    (
        "java/lang/StringBuilder",
        "substring",
        "(II)Ljava/lang/String;",
    ),
    (
        "java/lang/StringBuilder",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/StringBuilder", "trimToSize", "()V"),
];

/// The archive-provider wave, 2026-09-23: the five rows that kept every
/// `jar:` and `jrt:` filesystem this VM's own synthetic one.
///
/// ```text
///   java/nio/file/spi/FileSystemProvider.installedProviders()
///   java/nio/file/spi/FileSystemProvider.newFileSystem(Path, Map)
///   java/nio/file/FileSystems.newFileSystem(Path, ClassLoader)
///   java/nio/file/FileSystems.newFileSystem(Path, Map)
///   java/nio/file/FileSystems.newFileSystem(Path, Map, ClassLoader)
/// ```
///
/// Three earlier tables held these out by name, each on the same premise:
/// "this VM ships no `META-INF/services` entries for a `jar:`/`jrt:` provider,
/// so the real `ServiceLoader` discovery would answer a one-element list", and
/// the three `newFileSystem` rows "lean on `installedProviders`". MEASURED
/// 2026-09-23 on Azure linux, JDK 25.0.4+7, `--real-jdk`, the premise is false
/// and the real providers work:
///
/// ```text
///   ServiceLoader.load(FileSystemProvider.class, systemLoader)
///       -> jdk.nio.zipfs.ZipFileSystemProvider, jdk.internal.jrtfs.JrtFileSystemProvider
///   real ZipFileSystemProvider.newFileSystem(h2.jar, {multi-release=25})
///       -> jdk.nio.zipfs.ZipFileSystem: /org/h2/tools 32 entries, 68 dirs,
///          SimpleResultSet.class 25831 bytes (HotSpot: identical)
///   jdk.internal.jimage.ImageReader.open(<java.home>/lib/modules)
///       -> /modules/java.base/java/lang/Object.class 2412 bytes (HotSpot: identical)
/// ```
///
/// The one thing standing between the real `jrt:` provider and a running
/// filesystem was not in this family at all: `SystemImage.findHome` reads
/// `SystemImage.class.getProtectionDomain().getCodeSource()`, which is null
/// for a class of the runtime image and was this VM's synthesised
/// `file:/runtime-defined/...` URL. `lang_class::native_class_get_protection_domain`
/// now runs the real body for a bootstrap class under `--real-jdk`, in the
/// same change.
///
/// With these retired, `FileSystemProvider.installedProviders()` is the real
/// `ServiceLoader` list with the default provider first (the real body reads
/// it from `FileSystems.getDefault().provider()`, the singleton, so the
/// identity HotSpot answers is kept), a `jar:` filesystem is a real
/// `ZipFileSystem` of `ZipPath`s whose separator is `/` on every platform,
/// and `jrt:/` is a real `JrtFileSystem`. That is what closed the in-process
/// `javac` gap -- H2's `CREATE ALIAS ... AS $$ ... $$`, whose classpath jar
/// this VM's synthetic filesystem listed as empty on Windows
/// (`docs/feature-designs/native-dispatch-architecture.md`
/// `h2-inprocess-javac-jrt-modules-listing-gap-FIXED-20260923.md`). A provider
/// a jar contributes through `META-INF/services` (Spring Boot's `nested:`)
/// is now in the list too, as on HotSpot.
///
/// `FileSystemProvider.newFileSystem(Path, Map)` is the fifth, and it is the
/// row that was really doing the mounting. The real `FileSystems.newFileSystem
/// (Path, ...)` offers the path to each installed provider in turn, and the
/// FIRST is the default `file` provider -- which inherits the abstract
/// default (`throw new UnsupportedOperationException()`) on HotSpot, so the
/// loop moves on to `jdk.nio.zipfs`. This VM's native on that triple mounted
/// its synthetic archive filesystem instead, for ANY provider, so with the
/// other four retired the default provider still answered first: MEASURED,
/// `javac` saw a real `ZipFileSystemProvider` in the list and a synthetic
/// filesystem from `FileSystems.newFileSystem`, and its classpath listing of
/// `org.h2.tools` came back EMPTY on linux -- the Windows symptom of the
/// javac page, reproduced by the mix. Its mirrored copies on the concrete
/// provider classes are registered with the kind this row ends up with
/// (`concrete_receiver::mirror_class_registrations`), so they retire with it.
///
/// The provider-level natives on the ABSTRACT `FileSystemProvider`
/// (`newFileSystem(URI, Map)`, `getPath(URI)`, ...) stay: no real provider
/// reaches them any more, because each real provider overrides them with
/// `Code` of its own, and the default mode still mints the synthetic providers
/// they serve.
pub(in crate::retired_shadow) static RETIRED_SHADOW_ARCHIVE_PROVIDER_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/nio/file/FileSystems",
        "newFileSystem",
        "(Ljava/nio/file/Path;Ljava/lang/ClassLoader;)Ljava/nio/file/FileSystem;",
    ),
    (
        "java/nio/file/FileSystems",
        "newFileSystem",
        "(Ljava/nio/file/Path;Ljava/util/Map;)Ljava/nio/file/FileSystem;",
    ),
    (
        "java/nio/file/FileSystems",
        "newFileSystem",
        "(Ljava/nio/file/Path;Ljava/util/Map;Ljava/lang/ClassLoader;)Ljava/nio/file/FileSystem;",
    ),
    (
        "java/nio/file/spi/FileSystemProvider",
        "installedProviders",
        "()Ljava/util/List;",
    ),
    (
        "java/nio/file/spi/FileSystemProvider",
        "newFileSystem",
        "(Ljava/nio/file/Path;Ljava/util/Map;)Ljava/nio/file/FileSystem;",
    ),
];

/// `java/util/jar/JarFile`, the whole class, 2026-09-24: the family's
/// producer, which wave 4 held (`+34` on `L1TailSweep`, `reached=4`).
///
/// # Why wave 4's red was a half-retirement, not a verdict on the class
///
/// Wave 4 armed the dial, and the dial yields at DISPATCH. A subclass's
/// `super(file)` never reaches the dial: `invoke.rs`'s stackless invokespecial
/// arm calls `native_methods.find("java/util/jar/JarFile", "<init>", ...)`
/// directly for the four `File` constructor shapes. So every armed run still
/// built its `JarFile`s NATIVELY -- the compact layout, `path` in slot 0 and no
/// `res` -- and then ran real `JarFile`/`ZipFile` accessors against them.
/// Measured on the pre-change binary with `apps`-style probe
/// `JarShadowProbe` (every construction shape, then every accessor, then
/// `close`), dial armed on `java/util/jar/JarFile`:
///
/// ```text
///   new JarFile(File)   res=false  size() -> NPE: Cannot read field "zsrc"
///                                  because "this.res" is null
///   class Sub extends JarFile { super(file) }
///                       res=false  every accessor served by the NATIVE
/// ```
///
/// That is the `+34`: real accessors over a native object. Retirement drops
/// the registration itself, so the invokespecial arm's `find` misses and falls
/// through to the real constructor too -- the object and every method on it
/// come from one side.
///
/// # What it repairs
///
/// Unarmed, `--real-jdk` answered for EVERY `JarFile` with the compact native
/// object while `java/util/zip/ZipFile` itself was already real (L1 wave 2).
/// The same probe, HotSpot 25.0.4+7 against the pre-change binary:
///
/// ```text
///                       HotSpot          --real-jdk (native JarFile)
///   res                 set              null
///   getEntry type       JarFileEntry     JarEntry
///   size() after close  IllegalStateException   answers
///   getEntry after close IllegalStateException  answers
/// ```
///
/// and Spring Boot's `NestedJarFile.close()` -> `super.close()` reached the
/// real `ZipFile.close()` over that null `res`
/// (`zipfile-cleanableresource-npe-on-nestedjarfile-close-FIXED-20260924.md`).
///
/// Both registrars' rows retire together: `native-io`'s
/// `register_jar_natives` and `native-builtins`' `register_p59_jar` register
/// the same triples (the latter last, so it owns the slot in compatible mode),
/// and a retirement that took one would leave the other as the surviving
/// owner `no_retired_triple_survives_the_strict_boot` exists to catch.
/// `close`/`getName`/`size`/`getComment` are inherited `ZipFile` bodies; the
/// other fourteen are declared by `JarFile` itself. All eighteen carry `Code`.
///
/// `--compatible` is unchanged: the re-tag makes these `SyntheticStub`, which
/// that mode registers and dispatches exactly as it did the `Bridge`.
///
/// # The nineteenth row: `ZipFile$1.getManifestName`
///
/// L1 wave 2 excluded it as "a shared-secret accessor shim no bytecode can
/// name", and under a native `JarFile` that was true. A real `JarFile` names it
/// twice -- `getManEntry()` with `false`, `maybeInstantiateVerifier()` with
/// `true` (JDK 25 `javap -c`) -- and a
/// `--dump-native-registry` census of the 54 `spring-boot-loader` test classes
/// counted 203 calls. The native answered `"META-INF/MANIFEST.MF"` for every
/// jar and both argument values. The real body answers the manifest's ACTUAL
/// entry name, or `null` when there is none, and with
/// `onlyIfSignatureRelatedFiles` it answers `null` for an unsigned jar. That
/// last case is how `JarFile` decides not to build a `JarVerifier`, so the
/// native was making every unsigned jar pay for verification setup.
pub(in crate::retired_shadow) static RETIRED_SHADOW_JARFILE_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/util/jar/JarFile", "<init>", "(Ljava/io/File;)V"),
    ("java/util/jar/JarFile", "<init>", "(Ljava/io/File;Z)V"),
    ("java/util/jar/JarFile", "<init>", "(Ljava/io/File;ZI)V"),
    (
        "java/util/jar/JarFile",
        "<init>",
        "(Ljava/io/File;ZILjava/lang/Runtime$Version;)V",
    ),
    ("java/util/jar/JarFile", "<init>", "(Ljava/lang/String;)V"),
    ("java/util/jar/JarFile", "<init>", "(Ljava/lang/String;Z)V"),
    ("java/util/jar/JarFile", "close", "()V"),
    (
        "java/util/jar/JarFile",
        "entries",
        "()Ljava/util/Enumeration;",
    ),
    (
        "java/util/jar/JarFile",
        "getComment",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/jar/JarFile",
        "getEntry",
        "(Ljava/lang/String;)Ljava/util/zip/ZipEntry;",
    ),
    (
        "java/util/jar/JarFile",
        "getInputStream",
        "(Ljava/util/zip/ZipEntry;)Ljava/io/InputStream;",
    ),
    (
        "java/util/jar/JarFile",
        "getJarEntry",
        "(Ljava/lang/String;)Ljava/util/jar/JarEntry;",
    ),
    (
        "java/util/jar/JarFile",
        "getManifest",
        "()Ljava/util/jar/Manifest;",
    ),
    (
        "java/util/jar/JarFile",
        "getManifestFromReference",
        "()Ljava/util/jar/Manifest;",
    ),
    ("java/util/jar/JarFile", "getName", "()Ljava/lang/String;"),
    ("java/util/jar/JarFile", "isMultiRelease", "()Z"),
    ("java/util/jar/JarFile", "size", "()I"),
    (
        "java/util/jar/JarFile",
        "stream",
        "()Ljava/util/stream/Stream;",
    ),
    (
        "java/util/zip/ZipFile$1",
        "getManifestName",
        "(Ljava/util/jar/JarFile;Z)Ljava/lang/String;",
    ),
];

/// 2026-09-24 -- `java/net/URLClassLoader` and `jdk/internal/loader/URLClassPath`,
/// retired whole: 26 rows, every registration either class had under
/// `--real-jdk`.
///
/// # What they were
///
/// A loader that never ran. The seven `URLClassLoader.<init>` natives skipped
/// the real constructors and left an UNCONSTRUCTED `URLClassPath` in `ucp`,
/// whose own natives answered empty. `findClass`/`findResource(s)`/
/// `getResourceAsStream`/`getURLs` then served the loader from the VM's flat
/// class path, reading each URL back as a filesystem path. A URL's `handler`
/// never took part.
///
/// # What that cost
///
/// Spring Boot's fat-JAR launcher is a `URLClassLoader` over
/// `jar:nested:/app.jar/!BOOT-INF/lib/x.jar!/` URLs served by Spring's own
/// handler. No filesystem path spells those, so `LaunchedClassLoader` could
/// not load one class out of `BOOT-INF/lib`. The VM got by with a second
/// shadow, flattening every fat JAR onto its own class path, and the
/// application then ran on the APPLICATION loader: wrong `getClassLoader()`,
/// `file:` code sources, handler-less resource URLs, `getResource("dir/")`
/// null. See
/// `fatjar-classes-load-from-the-vms-flattened-class-path-not-launchedclassloader-FIXED-20260924.md`.
///
/// # Measured
///
/// With these retired, the flattening refused under `--real-jdk`, and the
/// class-loading shortcut `ucl_try_define_local_class` refused there too,
/// `LoaderProbe` and `FatProbe` run as the `Start-Class` of a fat JAR matched
/// HotSpot 25.0.4+7 line for line. That covers the loader, parent, 36 URLs, resources
/// through Spring's handler, `getJarFile()` on nested entries, directory
/// resources and code sources. Neither did before.
///
/// `--compatible` is unchanged: the re-tag makes these `SyntheticStub`, which
/// that mode registers and dispatches exactly as it did the `Bridge`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_URLCLASSLOADER_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/net/URLClassLoader",
        "<init>",
        "(Ljava/lang/String;[Ljava/net/URL;Ljava/lang/ClassLoader;)V",
    ),
    (
        "java/net/URLClassLoader",
        "<init>",
        "(Ljava/lang/String;[Ljava/net/URL;Ljava/lang/ClassLoader;Ljava/net/URLStreamHandlerFactory;)V",
    ),
    (
        "java/net/URLClassLoader",
        "<init>",
        "(Ljava/lang/String;[Ljava/net/URL;Ljava/lang/ClassLoader;Ljava/security/AccessControlContext;)V",
    ),
    ("java/net/URLClassLoader", "<init>", "([Ljava/net/URL;)V"),
    (
        "java/net/URLClassLoader",
        "<init>",
        "([Ljava/net/URL;Ljava/lang/ClassLoader;)V",
    ),
    (
        "java/net/URLClassLoader",
        "<init>",
        "([Ljava/net/URL;Ljava/lang/ClassLoader;Ljava/net/URLStreamHandlerFactory;)V",
    ),
    (
        "java/net/URLClassLoader",
        "<init>",
        "([Ljava/net/URL;Ljava/security/AccessControlContext;)V",
    ),
    ("java/net/URLClassLoader", "addURL", "(Ljava/net/URL;)V"),
    ("java/net/URLClassLoader", "close", "()V"),
    (
        "java/net/URLClassLoader",
        "findClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
    ),
    (
        "java/net/URLClassLoader",
        "findResource",
        "(Ljava/lang/String;)Ljava/net/URL;",
    ),
    (
        "java/net/URLClassLoader",
        "findResources",
        "(Ljava/lang/String;)Ljava/util/Enumeration;",
    ),
    (
        "java/net/URLClassLoader",
        "getResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;",
    ),
    ("java/net/URLClassLoader", "getURLs", "()[Ljava/net/URL;"),
    ("jdk/internal/loader/URLClassPath", "<clinit>", "()V"),
    ("jdk/internal/loader/URLClassPath", "<init>", "([Ljava/net/URL;)V"),
    (
        "jdk/internal/loader/URLClassPath",
        "<init>",
        "([Ljava/net/URL;Ljava/net/URLStreamHandlerFactory;)V",
    ),
    ("jdk/internal/loader/URLClassPath", "addURL", "(Ljava/net/URL;)V"),
    (
        "jdk/internal/loader/URLClassPath",
        "closeLoaders",
        "()Ljava/util/List;",
    ),
    ("jdk/internal/loader/URLClassPath", "closeLoaders", "()V"),
    (
        "jdk/internal/loader/URLClassPath",
        "findResource",
        "(Ljava/lang/String;)Ljava/net/URL;",
    ),
    (
        "jdk/internal/loader/URLClassPath",
        "findResource",
        "(Ljava/lang/String;Z)Ljava/net/URL;",
    ),
    (
        "jdk/internal/loader/URLClassPath",
        "findResources",
        "(Ljava/lang/String;)Ljava/util/Enumeration;",
    ),
    (
        "jdk/internal/loader/URLClassPath",
        "getLoader",
        "(I)Ljdk/internal/loader/URLClassPath$Loader;",
    ),
    ("jdk/internal/loader/URLClassPath", "getURLs", "()[Ljava/net/URL;"),
    ("jdk/internal/loader/URLClassPath", "getURLs", "(Z)[Ljava/net/URL;"),
];

/// 2026-09-24 -- `java/net/URL`'s `(String)` constructor and seven accessors,
/// the half of lane 6's `URL` follow-up ("give the carrier a `handler`, or
/// stop minting it") that the `URLClassLoader` wave made possible.
///
/// Each JDK body is one field read, or, for the constructor, the parse these
/// natives yielded to anyway. They existed for URLs this VM MINTED, whose
/// fields were written by slot for a synthetic layout. Under `--real-jdk` those
/// are now built by the JDK's constructor, or by name where no JDK handler
/// exists (`jdk_only_constructed_url`, `build_synthetic_url`), so the real
/// fields are the answer.
///
/// Measured before the retirement, a Spring Boot fat JAR run through Spring's
/// own `LaunchedClassLoader` made 1.46 million calls into these eight, 30% of
/// that run's 4.8 million native calls: `JarFileUrlKey`, `URLStreamHandler` and
/// every resource lookup read the URL through them. `getDefaultPort` stays: its
/// JDK body goes through `handler`, which a by-name URL does not have.
/// See `fatjar-classes-load-from-the-vms-flattened-class-path-not-launchedclassloader-FIXED-20260924.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_URL_ACCESSOR_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/net/URL", "<init>", "(Ljava/lang/String;)V"),
    ("java/net/URL", "getFile", "()Ljava/lang/String;"),
    ("java/net/URL", "getHost", "()Ljava/lang/String;"),
    ("java/net/URL", "getPath", "()Ljava/lang/String;"),
    ("java/net/URL", "getPort", "()I"),
    ("java/net/URL", "getProtocol", "()Ljava/lang/String;"),
    ("java/net/URL", "getQuery", "()Ljava/lang/String;"),
    ("java/net/URL", "getRef", "()Ljava/lang/String;"),
];

/// 2026-09-26 -- `LinkedHashMap`'s three `SequencedMap` view accessors, the
/// door wave 9's retirement of `keySet` / `values` / `entrySet` left open.
///
/// On JDK 21 and 25 `LinkedHashMap.keySet()` is `return sequencedKeySet();`
/// (and likewise `values` / `entrySet`), so retiring the three plain accessors
/// only moved the call one frame down: every `keySet()` under `--real-jdk`
/// still reached `native_lhm_key_set`. That native rebuilds its carrier from
/// the whole `after` chain after any mutation (`lhm_collect_keys`, O(n)), so a
/// `keySet()` taken after a put costs a walk of the map and an allocation per
/// entry. `R11W19LhmTail` (8 x 100 000 puts, then one `keySet()` walk per
/// call) measured the phase at ~2.9 s before this table and ~0.6 s after,
/// which is what the puts alone cost (HotSpot: 40 ms and 38 ms).
///
/// The real bodies need nothing the VM does not already provide under
/// `--real-jdk`: `new LinkedKeySet(false)` (or `LinkedValues`,
/// `LinkedEntrySet`) cached in the map's own `keySet` / `values` /
/// `entrySet` field, over the real `head` / `after` chain that wave 9's
/// retirement of `put` and the iterator carriers made authoritative.
///
/// Wave 9 held these out of its own table for a census reason, not a verdict:
/// jdk-17.0.20.1+1 declares none of them, and that table is "bucket A or B on
/// all three images". A separate table keeps that claim true. On a 17 image
/// the rows have no method to shadow and nothing calls them, so dropping them
/// under `--real-jdk` there changes nothing.
///
/// # `reversed()` has to go with them
///
/// Measured on the first trial binary, which retired the three accessors
/// alone: `apps/probes/LinkedSequencedShadowSweep` row 92 ("reversed is a live
/// view") went from HotSpot's `{e=5, c=3, a=1, b=2, d=4}` to the stale
/// `{c=3, a=1, b=2, d=4}`. `LinkedHashMap.reversed()` is a native SNAPSHOT
/// (`build_reversed_map_snapshot`), and what kept it live was
/// `resync_reversed_map` inside the three accessor natives. Retire the readers
/// and the snapshot stops being refreshed. The same is true of every other
/// reader of a reversed map that reaches a view.
///
/// So `reversed()` is retired too, and the JDK's `ReverseOrderLinkedHashMapView`
/// runs over the real chain. Its views are `base.sequencedKeySet().reversed()`
/// and siblings, which reach the three view carriers' own `reversed()` -- also
/// native, and also wrong on its own: `native_view_reversed` answers an
/// `ArrayList`, which is not a `SequencedSet`, so
/// `m.sequencedKeySet().reversed().toString()` threw
/// `IncompatibleClassChangeError` in both modes (sweep row 80). The real body
/// is `new LinkedKeySet(!reversed)`. Seven rows, one path.
///
/// `putFirst` / `putLast` / `pollFirstEntry` / `pollLastEntry` stay native.
/// They write the map through `lhm_set`, which mirrors into the real fields,
/// and the sweep's rows for them match HotSpot with or without this table. See
/// `r11w17-hashmap-linkedhashmap-put-34x-hotspot-FIXED-20260926.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_LHM_SEQUENCED_VIEW_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/util/LinkedHashMap",
        "reversed",
        "()Ljava/util/SequencedMap;",
    ),
    (
        "java/util/LinkedHashMap",
        "sequencedEntrySet",
        "()Ljava/util/SequencedSet;",
    ),
    (
        "java/util/LinkedHashMap",
        "sequencedKeySet",
        "()Ljava/util/SequencedSet;",
    ),
    (
        "java/util/LinkedHashMap",
        "sequencedValues",
        "()Ljava/util/SequencedCollection;",
    ),
    (
        "java/util/LinkedHashMap$LinkedEntrySet",
        "reversed",
        "()Ljava/util/SequencedSet;",
    ),
    (
        "java/util/LinkedHashMap$LinkedKeySet",
        "reversed",
        "()Ljava/util/SequencedSet;",
    ),
    (
        "java/util/LinkedHashMap$LinkedValues",
        "reversed",
        "()Ljava/util/SequencedCollection;",
    ),
];

/// Round 13 wave 7 (lane shadow), 2026-09-28 -- the `HashSet` family, the
/// last `java/util` hash collection still standing in front of its own
/// bytecode under `--real-jdk`. 48 rows here, 7 `SequencedSet` rows in
/// [`RETIRED_SHADOW_LHS_SEQUENCED_TRIPLES`].
///
/// # What the hold was, and why it is gone
///
/// `the_held_collection_families_are_not_retired` held `HashSet.iterator` and
/// `LinkedHashSet.add` with no reason of their own; the reason was the
/// family's neighbours. Wave 7 (`LinkedHashMap`) was refused on "the
/// `HashSet`-family iterator carrier", and wave 9 found that carrier was a
/// MINT-SITE defect (`seed_real_map_itr_fields`), fixed for the set family and
/// the map family alike without touching `SET_CLASSES`. Read on this tree:
///
/// * The store is real. `native_hs_init*` put a real `java/util/HashMap` (or
///   `LinkedHashMap` for an insertion-ordered set) in the set's own `map`
///   field (`alloc_hs_backing`, `hs_set_backing_map`), and `native_hs_add`
///   writes the REAL `HashSet.PRESENT` (`hs_present_marker_at`), so
///   `map.remove(o) == PRESENT` in real `HashSet.remove` holds for an element
///   a native added.
/// * Both backing families are already retired (`RETIRED_SHADOW_L1_HM_TRIPLES`,
///   `RETIRED_SHADOW_L1_LHM_TRIPLES`), so under `--real-jdk` the map inside a
///   set is served by real bytecode today. What stays native is the set's own
///   surface, which is the mixed state `r13w6-hashtree-...` photographed: a
///   colliding `HashSet` never treeifies (`native_map_put_evict_pinned`
///   appends plain `HashMap$Node`s), while the same keys in a bare `HashMap`
///   do. The blast-radius baseline already took the class-level dial on this
///   prefix and it was verdict-neutral against its control
///   (`scripts/baselines/jdk-only-blast-radius-25-linux.txt`,
///   `java/util/HashSet 128 140`, no vector net of control, 2026-09-21).
/// * Every non-retired mint site builds through the class's own `<init>` and
///   `add` (`build_real_hash_set`, and the `try_alloc_concurrent_synthetic`
///   sites in `phases_late/collections.rs`, `charset_buffers.rs`,
///   `text_intl.rs`, `reflect_invoke.rs`, `stack_walker.rs`), which after this
///   table are real bytecode. The three that write a raw legacy layout
///   (`Collections.singleton` twice and `newSetFromMap`'s fallback) sit behind
///   natives that are themselves retired.
///
/// # The rows
///
/// Every `Bridge` registration on `java/util/HashSet` and
/// `java/util/LinkedHashSet` in the 25/linux kind map (63 registrations over
/// these 55 triples; four triples on each class are registered twice or three
/// times) whose target has a `Code` attribute, declared or inherited, on 17, 21
/// and 25. **One is left out on each class**: `comparator()`, registered by
/// `register_collections_natives` for an AssertJ `SortedSet` edge. No image
/// declares it on either class, so there is no bytecode to yield to and
/// retiring it would turn a null into an `AbstractMethodError`.
/// `the_held_collection_families_are_not_retired` holds it.
///
/// The second and third `LinkedHashSet.remove` / `retainAll` registrations
/// are `native-builtins/src/properties_sidetable.rs`'s, kept "for ORDINARY
/// `LinkedHashSet`s" because the backing map once held an `Int(1)` sentinel
/// that real `remove`'s `== PRESENT` test failed. The writers store the real
/// `PRESENT` now (`hs_present_marker_at`), except
/// `build_real_layout_string_hashset`, which is filed with a patch
/// (`r13w7-shadow-string-hashset-builder-present-and-utf16-hash-patch-FIXED-20260928.md`).
///
/// `java/util/concurrent/CopyOnWriteArraySet`, the third `SET_CLASSES` entry,
/// needs no row: `cow_set_route` already takes a real receiver to its own
/// bytecode, and the natives are kept for the fabricated stub.
///
/// # What is NOT changed, and what must be measured before this lands
///
/// * `--compatible` is unchanged: a `SyntheticStub` registers and dispatches
///   normally there.
/// * The interface rows `register_interface_natives` puts on `java/util/Set`
///   (`iterator`, `size`, `isEmpty`, `forEach`) stay `Bridge`, exactly as the
///   `Map` rows stayed when `HashMap` retired. They read the real backing map.
/// * Written from reading, not from a trial binary. The acceptance run is the
///   one every family above took: the probe tree and the strict corpus with
///   `CRATONVM_UNRETIRE_NATIVE_SHADOW=java/util/HashSet,java/util/LinkedHashSet`
///   as the paired control (that variable is also the kill switch), plus
///   `R13HashcompatEqualsOrder` under `--real-jdk`, whose `HashSet colliding`
///   line should then match HotSpot. `the native-registry census` moves by the number of
///   registrations re-tagged (+63 predicted per configuration, +0 rows; measure
///   it paired) and the 25/linux kind map by the same rows, `bridge` ->
///   `synthetic-stub`. Record:
///   `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L1_HS_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/util/HashSet", "<init>", "()V"),
    ("java/util/HashSet", "<init>", "(I)V"),
    ("java/util/HashSet", "<init>", "(IF)V"),
    ("java/util/HashSet", "<init>", "(Ljava/util/Collection;)V"),
    ("java/util/HashSet", "add", "(Ljava/lang/Object;)Z"),
    ("java/util/HashSet", "addAll", "(Ljava/util/Collection;)Z"),
    ("java/util/HashSet", "clear", "()V"),
    ("java/util/HashSet", "contains", "(Ljava/lang/Object;)Z"),
    (
        "java/util/HashSet",
        "containsAll",
        "(Ljava/util/Collection;)Z",
    ),
    ("java/util/HashSet", "equals", "(Ljava/lang/Object;)Z"),
    (
        "java/util/HashSet",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
    ),
    ("java/util/HashSet", "hashCode", "()I"),
    ("java/util/HashSet", "isEmpty", "()Z"),
    ("java/util/HashSet", "iterator", "()Ljava/util/Iterator;"),
    ("java/util/HashSet", "remove", "(Ljava/lang/Object;)Z"),
    (
        "java/util/HashSet",
        "removeAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/HashSet",
        "retainAll",
        "(Ljava/util/Collection;)Z",
    ),
    ("java/util/HashSet", "size", "()I"),
    (
        "java/util/HashSet",
        "spliterator",
        "()Ljava/util/Spliterator;",
    ),
    ("java/util/HashSet", "stream", "()Ljava/util/stream/Stream;"),
    ("java/util/HashSet", "toArray", "()[Ljava/lang/Object;"),
    (
        "java/util/HashSet",
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
    ),
    (
        "java/util/HashSet",
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
    ),
    ("java/util/HashSet", "toString", "()Ljava/lang/String;"),
    ("java/util/LinkedHashSet", "<init>", "()V"),
    ("java/util/LinkedHashSet", "<init>", "(I)V"),
    ("java/util/LinkedHashSet", "<init>", "(IF)V"),
    (
        "java/util/LinkedHashSet",
        "<init>",
        "(Ljava/util/Collection;)V",
    ),
    ("java/util/LinkedHashSet", "add", "(Ljava/lang/Object;)Z"),
    (
        "java/util/LinkedHashSet",
        "addAll",
        "(Ljava/util/Collection;)Z",
    ),
    ("java/util/LinkedHashSet", "clear", "()V"),
    (
        "java/util/LinkedHashSet",
        "contains",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/LinkedHashSet",
        "containsAll",
        "(Ljava/util/Collection;)Z",
    ),
    ("java/util/LinkedHashSet", "equals", "(Ljava/lang/Object;)Z"),
    (
        "java/util/LinkedHashSet",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
    ),
    ("java/util/LinkedHashSet", "hashCode", "()I"),
    ("java/util/LinkedHashSet", "isEmpty", "()Z"),
    (
        "java/util/LinkedHashSet",
        "iterator",
        "()Ljava/util/Iterator;",
    ),
    ("java/util/LinkedHashSet", "remove", "(Ljava/lang/Object;)Z"),
    (
        "java/util/LinkedHashSet",
        "removeAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/LinkedHashSet",
        "retainAll",
        "(Ljava/util/Collection;)Z",
    ),
    ("java/util/LinkedHashSet", "size", "()I"),
    (
        "java/util/LinkedHashSet",
        "spliterator",
        "()Ljava/util/Spliterator;",
    ),
    (
        "java/util/LinkedHashSet",
        "stream",
        "()Ljava/util/stream/Stream;",
    ),
    (
        "java/util/LinkedHashSet",
        "toArray",
        "()[Ljava/lang/Object;",
    ),
    (
        "java/util/LinkedHashSet",
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
    ),
    (
        "java/util/LinkedHashSet",
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
    ),
    (
        "java/util/LinkedHashSet",
        "toString",
        "()Ljava/lang/String;",
    ),
];

/// The JDK 21 `SequencedSet` half of [`RETIRED_SHADOW_L1_HS_TRIPLES`], seven
/// `LinkedHashSet` rows jdk-17 does not declare -- kept apart for the census
/// reason [`RETIRED_SHADOW_LHM_SEQUENCED_VIEW_TRIPLES`] gives, and retired for
/// the reason it gives too: on 21/25 the real bodies go through the map's
/// retired `sequencedKeySet()` and its still-native, real-field-mirroring
/// `putFirst`/`putLast`; on 17 nothing can call them.
pub(in crate::retired_shadow) static RETIRED_SHADOW_LHS_SEQUENCED_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/util/LinkedHashSet",
        "addFirst",
        "(Ljava/lang/Object;)V",
    ),
    (
        "java/util/LinkedHashSet",
        "addLast",
        "(Ljava/lang/Object;)V",
    ),
    (
        "java/util/LinkedHashSet",
        "getFirst",
        "()Ljava/lang/Object;",
    ),
    ("java/util/LinkedHashSet", "getLast", "()Ljava/lang/Object;"),
    (
        "java/util/LinkedHashSet",
        "removeFirst",
        "()Ljava/lang/Object;",
    ),
    (
        "java/util/LinkedHashSet",
        "removeLast",
        "()Ljava/lang/Object;",
    ),
    (
        "java/util/LinkedHashSet",
        "reversed",
        "()Ljava/util/SequencedSet;",
    ),
];

/// Round 13 wave 8 (lane shadow2), 2026-09-28 -- the `java.beans` change-support
/// family: `PropertyChangeEvent` (6), `PropertyChangeSupport` (12) and
/// `VetoableChangeSupport` (7). 25 rows, every `Bridge` registration on the
/// three classes in the 25/linux kind map, each registered once, by
/// `register_p72_beans` (`native-builtins/src/phases_late/beans_jndi.rs`).
///
/// # Why this family
///
/// `register_p72_beans` is registered in real-JDK mode for ONE reason, the
/// `Introspector.getBeanInfo` / `PropertyDescriptor` natives Spring needs
/// (SPB.10, `reflect_annotations.rs`, and `vm_exec.rs`'s forced-native arms).
/// The change-support natives came along with that call. They were written
/// for the synthetic JDK's own two- and four-slot layouts (`class_manager.rs`
/// `instance_fields(2)` / `instance_fields(4)`) and they diverge from the JDK
/// in ways a caller can see:
///
/// * `firePropertyChange(name, null, null)` does not fire, and two EQUAL but
///   distinct values do (`pcs_values_equal` compares references). The JDK
///   fires when either value is null and skips when `oldValue.equals(newValue)`.
/// * Named listeners get every property (`removePropertyChangeListener(name,
///   l)` removes from the one list); `getPropertyChangeListeners(name)`
///   answers an empty array; `getPropertyChangeListeners()` has no
///   `PropertyChangeListenerProxy` wrappers and keeps a `null` added by
///   `addPropertyChangeListener(null)`.
/// * A listener's `RuntimeException` is swallowed and the next listener still
///   runs; the JDK propagates it.
/// * `new PropertyChangeSupport(null)` does not throw `NullPointerException`
///   and `new PropertyChangeEvent(null, ..)` does not throw `EventObject`'s
///   `IllegalArgumentException("null source")`.
/// * `PropertyChangeEvent.toString()` answers `PropertyChangeEvent[p]`, not the
///   JDK's `java.beans.PropertyChangeEvent[propertyName=p; oldValue=..; ..]`.
/// * `fireVetoableChange` re-fires the reverted event to EVERY listener; the
///   JDK re-fires it only to the listeners before the one that vetoed.
/// * The natives write the synthetic slots into REAL-layout objects: the real
///   `PropertyChangeSupport` declares `map` then `source`, and the native
///   `<init>` stores the source bean where real bytecode reads `map` and its
///   own `ArrayList` where real bytecode reads `source`. Every member left
///   UNREGISTERED -- `fireIndexedPropertyChange`, `getVetoableChangeListeners`,
///   `fireVetoableChange(PropertyChangeEvent)`, `writeObject` -- is real
///   bytecode reading those two slots, so an indexed event carries the list as
///   its source and `getVetoableChangeListeners()` dispatches `getListeners` on
///   the bean.
///
/// # The real bytecode has what it needs under `--real-jdk`
///
/// Read on the JDK 25 sources: `ChangeListenerMap` is a `HashMap` of listener
/// arrays (`RETIRED_SHADOW_L1_HM_TRIPLES`: real), `getListeners` builds an
/// `ArrayList` (real) and calls `toArray(T[])`; the rest is `System.arraycopy`,
/// `Integer.valueOf` / `Boolean.valueOf`, `Object.equals`, `EventObject`,
/// `EventListenerProxy` / `PropertyChangeListenerProxy` (no natives registered
/// on any of them), and `StringBuilder` (`RETIRED_SHADOW_STRINGBUILDER_TRIPLES`:
/// real) plus `Class.getName` for `toString`. `<clinit>` builds three
/// `ObjectStreamField`s, as `java.util.Random`'s does on every boot. Nothing
/// here is needed before bytecode can run, and no VM-internal invariant rides
/// on the natives: the only mint sites of the three classes are these natives
/// themselves (their `try_alloc_concurrent_synthetic` event allocations), and
/// the one VM reader, the `PropertyVetoException` report in `phases_late.rs`,
/// goes through `invoke_virtual`.
///
/// `PropertyChangeEvent.getSource` is INHERITED (declared by
/// `java/util/EventObject`), retired on the same terms as the `HashSet`
/// table's inherited rows: the retired row yields to `EventObject.getSource`.
///
/// # Not retired, on purpose
///
/// The rest of `register_p72_beans` -- `Introspector.*`, `PropertyDescriptor`,
/// `FeatureDescriptor.getName`, `BeanInfo`, `Beans` -- stays `Bridge`:
/// `vm_exec.rs` forces the `Introspector` / `PropertyDescriptor` /
/// `FeatureDescriptor` natives for Spring's `BeanWrapperImpl`, and their
/// synthetic `PropertyDescriptor` carriers are read by slot.
/// `the_beans_change_support_family_is_retired_alone` holds that line.
///
/// # What is NOT changed, and what must be measured before this lands
///
/// * `--compatible` is unchanged: the re-tagged `SyntheticStub` registers and
///   dispatches there as the `Bridge` did (no `keep_real_*` or class-wide
///   drop arm names `java/beans/`; `real_protected_stub_class` does not list
///   it). The compatible-mode layout corruption above is therefore still
///   there; it is filed as
///   `r13w8-shadow2-beans-change-support-natives-corrupt-real-layout-in-compatible-FIXED-20260928.md`.
///   Round 13 wave 9 (lane shadow3) fixed it at the registration site:
///   `register_p72_beans` no longer registers the change-support half on a
///   real JDK in `--compatible` (`beans_change_support_left_to_bytecode`, kill
///   switch `CRATONVM_BEANS_REAL_CHANGE_SUPPORT=0`). `--real-jdk` still goes
///   through THIS table, unchanged, so the rows here stay live there.
/// * Written from reading, not from a trial binary. The acceptance run is the
///   paired one: the probe tree (`R13Shadow2PropertyChange`,
///   `R13Shadow2VetoableChange`) and the strict corpus under `--real-jdk`,
///   with `CRATONVM_UNRETIRE_NATIVE_SHADOW=java/beans/` as the control and
///   kill switch. `the native-registry census` should move by +25 stubs, +0 rows per
///   arm (each triple is registered once; measure it paired), and the 25/linux
///   kind map by the same 25 rows, `bridge` -> `synthetic-stub`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_BEANS_CHANGE_SUPPORT_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/beans/PropertyChangeEvent",
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
    ),
    (
        "java/beans/PropertyChangeEvent",
        "getNewValue",
        "()Ljava/lang/Object;",
    ),
    (
        "java/beans/PropertyChangeEvent",
        "getOldValue",
        "()Ljava/lang/Object;",
    ),
    (
        "java/beans/PropertyChangeEvent",
        "getPropertyName",
        "()Ljava/lang/String;",
    ),
    (
        "java/beans/PropertyChangeEvent",
        "getSource",
        "()Ljava/lang/Object;",
    ),
    (
        "java/beans/PropertyChangeEvent",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "java/beans/PropertyChangeSupport",
        "<init>",
        "(Ljava/lang/Object;)V",
    ),
    (
        "java/beans/PropertyChangeSupport",
        "addPropertyChangeListener",
        "(Ljava/beans/PropertyChangeListener;)V",
    ),
    (
        "java/beans/PropertyChangeSupport",
        "addPropertyChangeListener",
        "(Ljava/lang/String;Ljava/beans/PropertyChangeListener;)V",
    ),
    (
        "java/beans/PropertyChangeSupport",
        "firePropertyChange",
        "(Ljava/beans/PropertyChangeEvent;)V",
    ),
    (
        "java/beans/PropertyChangeSupport",
        "firePropertyChange",
        "(Ljava/lang/String;II)V",
    ),
    (
        "java/beans/PropertyChangeSupport",
        "firePropertyChange",
        "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
    ),
    (
        "java/beans/PropertyChangeSupport",
        "firePropertyChange",
        "(Ljava/lang/String;ZZ)V",
    ),
    (
        "java/beans/PropertyChangeSupport",
        "getPropertyChangeListeners",
        "()[Ljava/beans/PropertyChangeListener;",
    ),
    (
        "java/beans/PropertyChangeSupport",
        "getPropertyChangeListeners",
        "(Ljava/lang/String;)[Ljava/beans/PropertyChangeListener;",
    ),
    (
        "java/beans/PropertyChangeSupport",
        "hasListeners",
        "(Ljava/lang/String;)Z",
    ),
    (
        "java/beans/PropertyChangeSupport",
        "removePropertyChangeListener",
        "(Ljava/beans/PropertyChangeListener;)V",
    ),
    (
        "java/beans/PropertyChangeSupport",
        "removePropertyChangeListener",
        "(Ljava/lang/String;Ljava/beans/PropertyChangeListener;)V",
    ),
    (
        "java/beans/VetoableChangeSupport",
        "<init>",
        "(Ljava/lang/Object;)V",
    ),
    (
        "java/beans/VetoableChangeSupport",
        "addVetoableChangeListener",
        "(Ljava/beans/VetoableChangeListener;)V",
    ),
    (
        "java/beans/VetoableChangeSupport",
        "fireVetoableChange",
        "(Ljava/lang/String;II)V",
    ),
    (
        "java/beans/VetoableChangeSupport",
        "fireVetoableChange",
        "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
    ),
    (
        "java/beans/VetoableChangeSupport",
        "fireVetoableChange",
        "(Ljava/lang/String;ZZ)V",
    ),
    (
        "java/beans/VetoableChangeSupport",
        "hasListeners",
        "(Ljava/lang/String;)Z",
    ),
    (
        "java/beans/VetoableChangeSupport",
        "removeVetoableChangeListener",
        "(Ljava/beans/VetoableChangeListener;)V",
    ),
];

/// Round 13 wave 12 (lane shadow4), 2026-09-28 -- the throwable family's
/// trace pair: `printStackTrace(Ljava/io/PrintWriter;)V` on all 62
/// `THROWABLE_FAMILY_CLASSES` and `setStackTrace([Ljava/lang/StackTraceElement;)V`
/// on 61 of them (`InvocationTargetException`'s branch of the registrar
/// registers no `setStackTrace`). 123 rows, each registered once, by
/// `register_throwable_subclass_natives` (`native-builtins/src/lang_misc.rs`)
/// under its own `set_category(Bridge)` scope; 2 are declared
/// (`java/lang/Throwable`'s own), 121 inherited. The class set is exactly the
/// one [`RETIRED_SHADOW_LT_TRIPLES`] retires `printStackTrace(PrintStream)`
/// on, which `the_throwable_trace_pair_is_retired_on_lane_t_s_class_set`
/// holds.
///
/// # They were in lane T's table, and why they came out
///
/// Lane T retired these 123 on 2026-09-10 and the 2026-09-11 merge pulled
/// them back (`lane-t-the-throwable-family-retired-...-20260910.md` §8) on
/// L1 wave 4's `java/text/ParseException` measurement:
/// `e.setStackTrace(new StackTraceElement[0]); e.printStackTrace(w)` printed
/// the whole captured trace with that ONE class's rows retired, and the
/// record's explanation was that "this VM's `Throwable` model does not read
/// back a `stackTrace` array that bytecode wrote". Read on this tree, what
/// does not read it back is the NATIVE printer, not the model:
///
/// * `native_throwable_print_stack_trace_to_stream` builds its lines from
///   `throwable_frame_text`, which reads the VM's capture store and never the
///   `stackTrace` field. An arm over `ParseException` alone left the
///   inherited rows on `Exception` / `Throwable` answering the call with that
///   native, over the array the real `setStackTrace` had just stored.
/// * The real `setStackTrace` stores a defensive copy into `stackTrace`; the
///   real `printStackTrace(PrintStreamOrWriter)` reads it through
///   `getOurStackTrace()`, and materialises `StackTraceElement.of(backtrace,
///   depth)` only while the field is `UNASSIGNED_STACK` / `null` -- which the
///   capture (`capture_throwable_trace_body`: `backtrace = this`, `depth`) and
///   `native_init_stack_trace_elements` already serve.
///
/// So the pair is coherent only when BOTH halves are bytecode on EVERY class
/// a lookup can reach, `java/lang/Throwable` included: the "all or none" of
/// `LogRecord`'s source pair (`the_log_record_source_pair_is_retired_as_a_set`).
/// That is this table, and it is why it is one table.
///
/// # What is wrong today under `--real-jdk`
///
/// The other two `printStackTrace` overloads, `getStackTrace` and every
/// constructor on these classes are real bytecode already (lane T), so the VM
/// answers one question two ways, by sink:
///
/// * `setStackTrace(x)` then `printStackTrace(PrintStream)` prints `x`, and
///   `printStackTrace(PrintWriter)` prints the captured frames instead. The
///   JUnit Platform's `ExceptionUtils.readStackTrace` and
///   `java.util.logging.SimpleFormatter` both render through a `PrintWriter`,
///   so a pruned or synthetic trace is lost exactly where it is shown.
/// * `printStackTrace((PrintWriter) null)` writes to fd 2 and returns; the JDK
///   throws `NullPointerException`.
/// * The native `setStackTrace` ignores the immutable-stack protocol: after the
///   `writableStackTrace == false` constructor (`stackTrace == null &&
///   backtrace == null`) the JDK's call is a no-op and `getStackTrace()` stays
///   empty; the native stores the array.
///
/// # The real bytecode has what it needs
///
/// `printStackTrace(PrintWriter)` is `printStackTrace(new
/// WrappedPrintWriter(s))`: the SAME private body the `PrintStream` overload
/// has run here since lane T (every `Thread.dispatchUncaughtException` report
/// goes through it). What is new is `Throwable$WrappedPrintWriter` (no natives)
/// and `PrintWriter`'s `lock` / `println(Object)`, and `java/io/PrintWriter` has
/// no live `Bridge` row ([`RETIRED_SHADOW_L4_PRINTWRITER_TRIPLES`] took the
/// seven it had) and no native mint site. `setStackTrace` is a `clone`, a null
/// scan, a monitor and a field store. No VM-internal invariant rides on either
/// native: `jni.rs`'s `ExceptionDescribe` invokes `printStackTrace()V`, which is
/// bytecode already, and nothing else in `vm/` or `native-*/` calls the two
/// bodies directly.
///
/// # What is NOT changed, and what must be measured before this lands
///
/// * `--compatible` is unchanged: the re-tagged `SyntheticStub` registers and
///   dispatches there as the `Bridge` did (no `real_protected_stub_class`
///   entry and no real-layout drop arm names a throwable class, which
///   `real_layout_bridge_keeps_are_not_retired_shadows` re-asks for every row
///   here). The compatible-mode divergence above is therefore still there,
///   for all three `printStackTrace` overloads; filed as
///   `r13w12-shadow4-throwable-native-print-ignores-set-stack-trace-FIXED-20260929.md`.
/// * Written from reading, not from a trial binary. The acceptance run is the
///   paired one: `C:\craton\jitr13-probes\src\R13Shadow4ThrowableTracePair.java`
///   and the strict corpus under `--real-jdk`, with
///   `CRATONVM_UNRETIRE_NATIVE_SHADOW` naming the 123 triples as the control
///   and kill switch (the census page's wave-12 section has the one-line
///   generator; a `Class` rule would also un-retire lane T's rows).
///   `the native-registry census` should move by +123 stubs, +0 rows per arm (each triple
///   is registered once; measure it paired), and the 25/linux kind map by the
///   same 123 rows, `bridge` -> `synthetic-stub`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_THROWABLE_TRACE_PAIR_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/io/EOFException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/io/EOFException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/io/FileNotFoundException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/io/FileNotFoundException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/io/IOException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/io/IOException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/io/NotSerializableException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/io/NotSerializableException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/io/UncheckedIOException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/io/UncheckedIOException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/io/UnsupportedEncodingException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/io/UnsupportedEncodingException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/AbstractMethodError",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/AbstractMethodError",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/ArithmeticException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/ArithmeticException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/ArrayIndexOutOfBoundsException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/ArrayIndexOutOfBoundsException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/AssertionError",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/AssertionError",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/ClassCastException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/ClassCastException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/ClassNotFoundException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/ClassNotFoundException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/CloneNotSupportedException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/CloneNotSupportedException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/Error",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/Error",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/Exception",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/Exception",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/ExceptionInInitializerError",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/ExceptionInInitializerError",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/IllegalAccessError",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/IllegalAccessError",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/IllegalAccessException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/IllegalAccessException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/IllegalArgumentException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/IllegalArgumentException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/IllegalStateException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/IllegalStateException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/IncompatibleClassChangeError",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/IncompatibleClassChangeError",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/IndexOutOfBoundsException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/IndexOutOfBoundsException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/InstantiationException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/InstantiationException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/InternalError",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/InternalError",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/InterruptedException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/InterruptedException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/LinkageError",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/LinkageError",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/MatchException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/MatchException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/NegativeArraySizeException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/NegativeArraySizeException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/NoClassDefFoundError",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/NoClassDefFoundError",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/NoSuchFieldError",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/NoSuchFieldError",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/NoSuchFieldException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/NoSuchFieldException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/NoSuchMethodError",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/NoSuchMethodError",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/NoSuchMethodException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/NoSuchMethodException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/NullPointerException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/NullPointerException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/NumberFormatException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/NumberFormatException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/OutOfMemoryError",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/OutOfMemoryError",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/ReflectiveOperationException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/ReflectiveOperationException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/RuntimeException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/RuntimeException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/SecurityException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/SecurityException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/StackOverflowError",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/StackOverflowError",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/StringIndexOutOfBoundsException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/StringIndexOutOfBoundsException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/Throwable",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/Throwable",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/TypeNotPresentException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/TypeNotPresentException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/UnsatisfiedLinkError",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/UnsatisfiedLinkError",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/UnsupportedOperationException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/UnsupportedOperationException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/VerifyError",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/VerifyError",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/reflect/InaccessibleObjectException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/lang/reflect/InaccessibleObjectException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/lang/reflect/InvocationTargetException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/net/MalformedURLException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/net/MalformedURLException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/net/UnknownHostException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/net/UnknownHostException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/text/ParseException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/text/ParseException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/util/ConcurrentModificationException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/util/ConcurrentModificationException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/util/FormatterClosedException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/util/FormatterClosedException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/util/InputMismatchException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/util/InputMismatchException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/util/MissingResourceException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/util/MissingResourceException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/util/NoSuchElementException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/util/NoSuchElementException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/util/concurrent/BrokenBarrierException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/util/concurrent/BrokenBarrierException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/util/concurrent/CancellationException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/util/concurrent/CancellationException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/util/concurrent/CompletionException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/util/concurrent/CompletionException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/util/concurrent/ExecutionException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/util/concurrent/ExecutionException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/util/concurrent/RejectedExecutionException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/util/concurrent/RejectedExecutionException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
    (
        "java/util/concurrent/TimeoutException",
        "printStackTrace",
        "(Ljava/io/PrintWriter;)V",
    ),
    (
        "java/util/concurrent/TimeoutException",
        "setStackTrace",
        "([Ljava/lang/StackTraceElement;)V",
    ),
];

/// Round 13 wave 13 (lane shadow5), 2026-09-28 -- the five `StringBuilder`
/// rows [`RETIRED_SHADOW_STRINGBUILDER_TRIPLES`] left out by rule:
/// `append(AbstractStringBuilder)`, `appendNull`, `getCoder`, `getValue` and
/// `repeat(CI)`, registered on `java/lang/StringBuilder` by the shared
/// `register_string_builder_natives(registry, class)` helper
/// (`native-builtins/src/lang_string.rs`, called once for this class from
/// `lib.rs` under an ambient `Bridge`, each triple registered once).
///
/// # Why the rule that held them was the wrong way round
///
/// That table's doc comment excluded them because `StringBuilder` does not
/// DECLARE them (only `AbstractStringBuilder` does, and those copies are
/// retired), "so dispatch already resolves these through the real
/// `AbstractStringBuilder` chain". A registry key is not a declaration,
/// though: a door that keys the lookup on the receiver's RUNTIME class (the
/// virtual-invoke cache populate; `invoke_or_native`'s exact-class probe when
/// the call site names `StringBuilder`) asks for
/// `(java/lang/StringBuilder, getValue, ()[B)` and finds this live `Bridge`
/// before any hierarchy walk starts. So under `--real-jdk` a builder whose every other
/// method is real bytecode over the real `value`/`coder`/`count` layout still
/// had these five answered by the dual-layout natives (`sb_view`), which read
/// the compact fields correctly but are a second implementation of state the
/// real class owns -- `repeat(CI)` is even bound to the `repeat(II)` codepoint
/// body. No HotSpot difference is known; this is AGENTS.md's "real class bytes
/// are authoritative" made true for the receiver-keyed doors, and the census
/// page's "an inherited-row `Bridge` is still reachable by virtual dispatch on
/// a `StringBuilder` receiver", ranked first by lane shadow4.
///
/// # The screen
///
/// * Real bytecode to yield to: all five are declared with `Code` on
///   `java/lang/AbstractStringBuilder` in 21 and 25 (`appendNull` and
///   `repeat(CI)` private, `getCoder`/`getValue`/`append(ASB)` package-private),
///   and their `AbstractStringBuilder` rows have been retired since
///   2026-09-21 with a byte-identical `SbLayoutRisk` probe.
/// * Unconstructed carrier: none new -- `StringBuilder.<init>` is retired, so
///   every builder a `--real-jdk` run sees went through the real constructor.
/// * VM invariant on the natives: none found. The only VM-side name match is
///   `native_override.rs`'s Mockito-redefinition force list, which forces a
///   REGISTERED native and has nothing to force once strict mode refuses it,
///   exactly as for the retired `AbstractStringBuilder.getValue`/`getCoder`.
/// * `--compatible` is unchanged: a `SyntheticStub` registers and dispatches
///   there as the `Bridge` did (`StringBuilder` is on no
///   `real_protected_stub_class` list and no real-layout drop arm).
///
/// Control and kill switch (five rules, one row each):
/// `CRATONVM_UNRETIRE_NATIVE_SHADOW=java/lang/StringBuilder.append(Ljava/lang/AbstractStringBuilder;)Ljava/lang/AbstractStringBuilder;,java/lang/StringBuilder.appendNull,java/lang/StringBuilder.getCoder,java/lang/StringBuilder.getValue,java/lang/StringBuilder.repeat(CI)Ljava/lang/AbstractStringBuilder;`.
/// Probe: `C:\craton\jitr13-probes\src\R13Shadow5StringBuilderInherited.java`.
/// Written from reading, not from a trial binary.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SB_INHERITED_FIVE_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/lang/StringBuilder",
        "append",
        "(Ljava/lang/AbstractStringBuilder;)Ljava/lang/AbstractStringBuilder;",
    ),
    (
        "java/lang/StringBuilder",
        "appendNull",
        "()Ljava/lang/AbstractStringBuilder;",
    ),
    ("java/lang/StringBuilder", "getCoder", "()B"),
    ("java/lang/StringBuilder", "getValue", "()[B"),
    (
        "java/lang/StringBuilder",
        "repeat",
        "(CI)Ljava/lang/AbstractStringBuilder;",
    ),
];

/// Round 13 wave 13 (lane shadow5), 2026-09-28 -- `getMessage` and
/// `toString` on `java/lang/VirtualMachineError`, the half-retirement the
/// SH4-1 ancestor gate (`native-builtins/tests/r13_shadow5_retired_ancestor_gate.rs`)
/// found on its first reading.
///
/// `register_exception_extras_natives` (`native-builtins/src/lib.rs`)
/// registers these two natives on 63 throwable classes. Lane T retired them
/// on 62 -- its class set, which `VirtualMachineError` is not in -- including
/// all three of its subclasses, `InternalError`, `OutOfMemoryError` and
/// `StackOverflowError`. None of the four declares either method, so the
/// superclass walk from a subclass receiver (`invoke_or_native`'s, and the
/// virtual-invoke cache populate's) passes the retired subclass row and stops
/// at the first ancestor with a registered native: THIS one
/// (`InvokeOrNativeParent`). The subclass rows have
/// therefore been retired in the table and not in effect, which is lane
/// shadow4's `ParseException` species (SH4-1) on a second family.
///
/// With these two retired the walk continues to `Error` (retired, no
/// declaration) and `Throwable` (declares both with `Code`, retired), so the
/// real bodies run. The HotSpot difference today: the native `toString`
/// reads `detailMessage` directly instead of calling `getLocalizedMessage()`,
/// so a `VirtualMachineError` subclass that overrides `getLocalizedMessage`
/// (and not `toString`) prints the raw message where HotSpot prints the
/// localized one. Same natives, same screen and same `--compatible`
/// argument as lane T's 62 classes; each triple registered once, ambient
/// `Bridge`.
///
/// Control and kill switch:
/// `CRATONVM_UNRETIRE_NATIVE_SHADOW=java/lang/VirtualMachineError.getMessage,java/lang/VirtualMachineError.toString`
/// (two rules, one row each; a `Class` rule would also un-retire lane 2's
/// four constructors). Probe: `R13Shadow5VmErrorMessagePair.java`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_VME_MESSAGE_PAIR_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/lang/VirtualMachineError",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/VirtualMachineError",
        "toString",
        "()Ljava/lang/String;",
    ),
];

/// Round 14 wave 2 (lane shadow), 2026-09-29 -- `java/io/OutputStream.write([B)V`,
/// the last live `Bridge` on the root of every byte output stream, item 2 of
/// `r13w13-shadow5-ancestor-bridges-over-retired-rows-FIXED-20260929.md`.
///
/// The screen, read on this tree:
///
/// * Pure-Java bytecode: JDK 17/21/25 declare it with `Code`, and the body is
///   `write(b, 0, b.length)` -- a virtual call.
/// * The native (`native_output_stream_write_all`, `native-builtins/src/lib.rs`,
///   registered once, ambient `Bridge`, no `real_jdk()` branch) does exactly
///   that: NPE on a null array, then `invoke_virtual(this, "write", "([BII)V")`.
///   So there is no state it keeps and no answer it gives that the bytecode
///   does not. What differs is the frame: an exception thrown by a
///   `write(byte[], int, int)` override reached through it carries no
///   `java.io.OutputStream.write(OutputStream.java)` frame, where HotSpot's
///   trace has one.
/// * Unconstructed carrier: none that matters. The only
///   `try_alloc_concurrent_synthetic(.., "java/io/OutputStream", 0)` sites are
///   inside `#[cfg(test)]` modules, and `OutputStream` has no instance fields.
///   The real body dispatches `write([BII)` virtually, exactly as the native
///   did, so a fabricated subclass reaches the same method either way.
/// * VM invariant on the native: none. Nothing in `vm/` or `native-*/` calls
///   it by name; VM code that writes to a stream calls `invoke_virtual`.
/// * `--compatible`: a `SyntheticStub` registers and dispatches there as the
///   `Bridge` did (`java/io/OutputStream` is on no `real_protected_stub_class`
///   list and no real-layout drop arm).
///
/// With it retired, `ByteArrayOutputStream.write([B)V` (retired since lane 4,
/// BAOS does not declare the method) no longer depends on the round-14
/// ancestor-bridge mask either: the ancestor row itself is gone under
/// `--real-jdk`.
///
/// Control and kill switch (one rule, one row):
/// `CRATONVM_UNRETIRE_NATIVE_SHADOW=java/io/OutputStream.write([B)V`.
/// Probe: `C:\craton\jitr14-probes\src\R14ShadowOutputStreamWriteAll.java`.
/// Written from reading, not from a trial binary.
pub(in crate::retired_shadow) static RETIRED_SHADOW_OUTPUT_STREAM_WRITE_ALL_TRIPLES: &[(&str, &str, &str)] =
    &[("java/io/OutputStream", "write", "([B)V")];

/// Round 14 wave 5 (lane compat5), 2026-09-29 -- the `java.sql` date/time
/// family: `java/sql/Date`, `java/sql/Time`, `java/sql/Timestamp`, 11 rows,
/// every `Bridge` `register_sql_datetime_natives` (`native-builtins/src/jdbc.rs`,
/// under `register_jdbc_driver_natives`' explicit `Bridge` scope, no
/// `real_jdk()` branch, each triple registered once) puts on them. A census
/// family of `r13w7-shadow-jdk-only-bridges-over-bytecode-census-CLOSED-20261006.md`.
///
/// # The screen, read on this tree
///
/// * Pure-Java bytecode to yield to: `Date(long)`, `Time(long)`,
///   `Timestamp(long)`, `Timestamp(int,int,int,int,int,int,int)`, the three
///   `toString()`s, `Timestamp.getTime()` and `Timestamp.toLocalDateTime()`
///   are declared with `Code` on 17/21/25; `Date.getTime()` / `Time.getTime()`
///   are INHERITED rows over `java.util.Date.getTime()` (declared with `Code`,
///   no native registered on it). The real bodies need only `java.util.Date`'s
///   constructors and deprecated getters -- the getters, setters and
///   field constructors retired since L1 wave 9
///   (`RETIRED_SHADOW_L1_DATE_TRIPLES`), the rest never registered -- and
///   `LocalDateTime.of`.
/// * State: the real layout. The natives already wrote `fastTime` (slot 0) and
///   `Timestamp.nanos` (the last slot) exactly where the real constructors
///   write them; the VM keeps no side table for these classes.
/// * Unconstructed carrier: none. No `try_alloc_concurrent_synthetic(..,
///   "java/sql/..")` site exists outside `#[cfg(test)]`; every instance goes
///   through a real or retired-to-real constructor. (The retired
///   `toLocalDateTime` native was itself a mint site: it built `LocalDate`,
///   `LocalTime` and `LocalDateTime` without `<init>`, by slot index.)
/// * VM invariant on the natives: none. Nothing in `vm/` or `native-*/`
///   names these triples; the Liquibase column reader (`native-builtins`
///   `lib.rs`) reads the fields directly and is unaffected.
///
/// # The HotSpot difference today (`--real-jdk`)
///
/// The natives read `fastTime` and never the `cdate` the real
/// `java.util.Date` keeps after a deprecated setter. `Date.setHours` & co.
/// run real bytecode here since L1 wave 9, and they leave `fastTime` stale
/// until `getTimeImpl()` normalizes: `Timestamp ts = new Timestamp(0L);
/// ts.setHours(5); ts.getTime()` answered `0` (HotSpot: 5 h local), and
/// `toString()` printed the unset time. The same holds for `Time` (its
/// `setHours` is legal) and for `java.sql.Date.setYear` / `setMonth` /
/// `setDate`. The natives also formatted with this VM's own calendar
/// (`millis_to_default_date_parts`) where the real bodies use
/// `java.util.Date`'s getters, so the two could not disagree only by luck.
///
/// `--compatible` is unchanged: a `SyntheticStub` registers and dispatches
/// there as the `Bridge` did (no `java/sql/` class is on any
/// `real_protected_stub_class` list or real-layout drop arm).
///
/// Control and kill switch (a class rule each, three rules, 11 rows):
/// `CRATONVM_UNRETIRE_NATIVE_SHADOW=java/sql/Date,java/sql/Time,java/sql/Timestamp`.
/// Probe: `C:\craton\jitr14-probes\src\R14Compat5SqlDatetime.java`. Written
/// from reading, not from a trial binary.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SQL_DATETIME_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/sql/Date", "<init>", "(J)V"),
    ("java/sql/Date", "getTime", "()J"),
    ("java/sql/Date", "toString", "()Ljava/lang/String;"),
    ("java/sql/Time", "<init>", "(J)V"),
    ("java/sql/Time", "getTime", "()J"),
    ("java/sql/Time", "toString", "()Ljava/lang/String;"),
    ("java/sql/Timestamp", "<init>", "(IIIIIII)V"),
    ("java/sql/Timestamp", "<init>", "(J)V"),
    ("java/sql/Timestamp", "getTime", "()J"),
    (
        "java/sql/Timestamp",
        "toLocalDateTime",
        "()Ljava/time/LocalDateTime;",
    ),
    ("java/sql/Timestamp", "toString", "()Ljava/lang/String;"),
];

/// Round 14 wave 6 (lane compat6), 2026-09-29 -- `java.lang.reflect.Array`'s
/// two `newInstance` overloads, the two `register` calls in
/// `register_reflect_array_natives` (`native-builtins/src/reflect_annotations.rs`,
/// under that registrar's explicit `Bridge` scope; the `register_synthetic_overrides`
/// copy is `synthetic-jdk` only). Page:
/// `r14w5-trace4-array-new-instance-bridge-over-bytecode-FIXED-20260929.md`; a family
/// of `r13w7-shadow-jdk-only-bridges-over-bytecode-census-CLOSED-20261006.md`, which
/// listed the class among the "mostly `ACC_NATIVE`" targets -- true of every
/// other row on it, not of these two.
///
/// # The screen, read on this tree
///
/// * Pure-Java bytecode to yield to: on 17/21/25 `newInstance(Class, int)` is
///   `return newArray(componentType, length);` (`Array.java:76` on 25) and
///   `newInstance(Class, int...)` is `return multiNewArray(componentType,
///   dimensions);`. Only `newArray` / `multiNewArray` (and `getLength`, `get`,
///   `set` and the typed accessors) are `ACC_NATIVE`; `newArray` and
///   `multiNewArray` stay registered as explicit `Bridge`s (`native-builtins`
///   `lib.rs`, `lang_system::native_array_new_array` /
///   `native_array_multi_new_array`), so the real bodies reach a native that
///   is live in every mode.
/// * Same answer, same checks: the retired `native_array_new_instance` made
///   `newArray`'s checks in `newArray`'s order (null component NPE, negative
///   length `NegativeArraySizeException`, `void` / 255-dimension component
///   IAE with no message) and resolved the component from the mirror's own
///   `ClassId` first, as `newArray` does; `native_array_new_instance_multi`
///   was already a tail call into `native_array_multi_new_array`. Nothing but
///   the extra frame changes.
/// * State: none (static methods, no carrier). Unconstructed carrier: none.
/// * VM invariant on the natives: none. Nothing in `vm/`, `jit/` or
///   `native-*/` looks the two triples up by name. The caller-sensitive
///   component resolution (`NativeContextImpl::array_resolution_referencing_class`)
///   skips `java/lang/reflect/Array` frames by class name, so the real
///   `newInstance` frame now under `newArray` is skipped too; and `newArray`
///   asks the mirror's `ClassId` before any by-name lookup anyway.
///
/// # The HotSpot difference today (`--real-jdk`)
///
/// A throw from the served native had the CALLER on top; HotSpot prints
/// `Array.newArray(Native Method)`, `Array.newInstance(Array.java:76)`. Wave
/// 5's stand-in rows (`vm/src/runtime/stackwalker.rs`, switch
/// `CRATONVM_THROWABLE_STANDIN_ARRAY_FRAMES`) rebuild that for the one-int
/// overload in every mode; with this table the real frame exists under
/// `--real-jdk`, so the multi-dimensional overload gets its
/// `Array.newInstance` frame there too (the stand-in has no row for it).
///
/// `--compatible` is unchanged: a `SyntheticStub` registers and dispatches
/// there as the `Bridge` did (`java/lang/reflect/Array` is on no
/// `real_protected_stub_class` list and no real-layout drop arm, so the
/// yield arbitration answers "native" and the JIT site cache admits it).
///
/// Control and kill switch:
/// `CRATONVM_UNRETIRE_NATIVE_SHADOW=java/lang/reflect/Array.newInstance`.
/// Probe: `C:\craton\jitr14-probes\src\R14Compat6ArrayNewInstance.java`.
/// Written from reading, not from a trial binary.
pub(in crate::retired_shadow) static RETIRED_SHADOW_REFLECT_ARRAY_NEW_INSTANCE_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/lang/reflect/Array",
        "newInstance",
        "(Ljava/lang/Class;I)Ljava/lang/Object;",
    ),
    (
        "java/lang/reflect/Array",
        "newInstance",
        "(Ljava/lang/Class;[I)Ljava/lang/Object;",
    ),
];

/// SBR wave 1, lane LOCALE, 2026-09-28: `java/util/Locale` (18),
/// `java/util/TimeZone` (5) and `ResourceBundle$Control.getCandidateLocales`
/// (1). 24 triples, 24 `Bridge` registrations in the 25/linux kind map; every
/// row declared with `Code` on jdk 17, 21 and 25 (`javap -p`). Record:
/// `docs/feature-designs/native-dispatch-architecture.md`.
///
/// # Why it is safe
///
/// `Locale`'s state is `baseLocale` / `localeExtensions` plus two lazy caches
/// whose zero is their declared initial value. Every native mint site that
/// survives this table (`locale_alloc` behind the HELD `ResourceBundle`
/// natives) goes through `locale_populate_full`, which writes a real
/// `BaseLocale` with interned subtags into `baseLocale` -- what the real
/// accessors, `equals`/`hashCode` and `Control.getCandidateLocales`' cache
/// read. With `<init>`/`getDefault`/`forLanguageTag` retired, every other
/// `Locale` is the JDK's own. `TimeZone`'s neighbours are already real:
/// `ZoneInfoFile.getZoneInfo*` is retired (`RETIRED_SHADOW_L1_ZI_TRIPLES`), so
/// real `getTimeZone` hands back a real `ZoneInfo`; the `ACC_NATIVE`
/// `getSystemTimeZoneID` and the `getOffset*` intrinsics are untouched.
///
/// One READER owned both ends of a convention and is fixed in the same
/// commit: `locale_bootstrap::default_case_mapping_language` read only the slot
/// the native `getDefault`/`setDefault` pair maintained, behind a cache only
/// the native `setDefault` invalidated. It now falls back to the real
/// `Locale.defaultLocale` static, keyed by the default's address, so a real
/// `Locale.setDefault(tr)` reaches `"TITLE".toLowerCase()`.
///
/// # Measured (linux/x86_64, JDK 25.0.4+7, control dev `038a7f265`)
///
/// ```text
///   tools/probes/SbrW1Locale (328 rows, drift 0 in every arm)   vs HotSpot
///     control --real-jdk                                          28
///     this table --real-jdk                                        9  (19 rows toward HotSpot)
///     this table + UNRETIRE (18+5+1 rows)          28, transcript = control
///     this table --compatible                      28, transcript = control --compatible
///   full suite --real-jdk    151/152, failing set RJdkLogging = control
///   strict corpus            diverged set = control's (minus a timestamp-only one)
///   witnesses LocaleDateTzShadowSweep, UulrLocaleDisplaySweep,
///     UtilCoverage4Sweep + 6 tools/probes Locale*: 0 lines control vs this
/// ```
///
/// The 9 left are not this table's: the host default `en_US` vs `en`
/// (`user.country` publishing, 5), `%S` ignoring `getDefault(FORMAT)` (1) and
/// the HELD `ResourceBundle.getBundle` (3).
///
/// # What is NOT here
///
/// * The 22 `Locale` static-FIELD registrations (`ROOT`, `US`, ...): not
///   methods, no `Code`, unreachable (`getstatic` never consults the registry).
/// * `java/util/ResourceBundle` (17): its natives mint instances of the
///   ABSTRACT class with a hidden map, and `sun/util/resources/Bundles.of` (1)
///   returns the same carrier; real `getObject` would meet an abstract
///   `handleGetObject`. L1 wave 2 also measured `TimeZone.getDisplayName` going
///   to UTC when it was retired.
/// * `sun/util/locale/provider/LocaleResources` (6): the floor the retired
///   `java/text` families stand on (`RETIRED_SHADOW_L1_BI_TRIPLES`); dialled with
///   `Locale` it threw `MissingResourceException` from
///   `DecimalFormatSymbols.getInstance(de_DE)`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_LOCALE_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/util/Locale", "<init>", "(Ljava/lang/String;)V"),
    (
        "java/util/Locale",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/String;)V",
    ),
    (
        "java/util/Locale",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
    ),
    (
        "java/util/Locale",
        "forLanguageTag",
        "(Ljava/lang/String;)Ljava/util/Locale;",
    ),
    (
        "java/util/Locale",
        "getAvailableLocales",
        "()[Ljava/util/Locale;",
    ),
    ("java/util/Locale", "getCountry", "()Ljava/lang/String;"),
    ("java/util/Locale", "getDefault", "()Ljava/util/Locale;"),
    (
        "java/util/Locale",
        "getDefault",
        "(Ljava/util/Locale$Category;)Ljava/util/Locale;",
    ),
    ("java/util/Locale", "getExtension", "(C)Ljava/lang/String;"),
    (
        "java/util/Locale",
        "getISO3Language",
        "()Ljava/lang/String;",
    ),
    ("java/util/Locale", "getLanguage", "()Ljava/lang/String;"),
    ("java/util/Locale", "getScript", "()Ljava/lang/String;"),
    ("java/util/Locale", "getVariant", "()Ljava/lang/String;"),
    (
        "java/util/Locale",
        "setDefault",
        "(Ljava/util/Locale$Category;Ljava/util/Locale;)V",
    ),
    ("java/util/Locale", "setDefault", "(Ljava/util/Locale;)V"),
    (
        "java/util/Locale",
        "stripExtensions",
        "()Ljava/util/Locale;",
    ),
    ("java/util/Locale", "toLanguageTag", "()Ljava/lang/String;"),
    ("java/util/Locale", "toString", "()Ljava/lang/String;"),
    (
        "java/util/ResourceBundle$Control",
        "getCandidateLocales",
        "(Ljava/lang/String;Ljava/util/Locale;)Ljava/util/List;",
    ),
    ("java/util/TimeZone", "getDefault", "()Ljava/util/TimeZone;"),
    (
        "java/util/TimeZone",
        "getDefaultRef",
        "()Ljava/util/TimeZone;",
    ),
    (
        "java/util/TimeZone",
        "getDisplayName",
        "(ZILjava/util/Locale;)Ljava/lang/String;",
    ),
    ("java/util/TimeZone", "getID", "()Ljava/lang/String;"),
    (
        "java/util/TimeZone",
        "getTimeZone",
        "(Ljava/lang/String;)Ljava/util/TimeZone;",
    ),
];

/// SBR wave 1, lane JCA (2026-09-28): the JCA engine/value classes whose
/// `Bridge` natives won over real bytecode in the `--real-jdk` census, SIX of
/// the fourteen classes the lane was given. 30 triples, 30 registrations.
///
/// # What is retired, and why it is safe
///
/// * `javax/crypto/spec/SecretKeySpec` (4). The native `<init>` wrote key@0 and
///   algorithm@1 by raw slot, which IS the real declaration order (`key`,
///   `algorithm`; `javap -p` on 17/21/25), and every native reader of a key
///   (`cipher.rs` `extract_key_bytes`, the keystore and PBE mints) reads slot 0.
///   So real `<init>`/getters and the native readers agree on one layout.
/// * `java/security/KeyPair` (2 accessors). Real `KeyPair` declares
///   `privateKey` BEFORE `publicKey`; `key_factory.rs` `alloc_synthetic_keypair`
///   wrote them the other way round and its own getters knew it -- the
///   "reader that owned both ends of a convention" defect. Fixed in the same
///   commit: the mint now writes the real layout and the two native getters
///   decide by what the slot holds, not by a routing flag.
/// * `java/security/cert/CertificateFactory` (4). The native `getInstance`
///   already returned the REAL provider-built factory whenever it could
///   (`try_build_real_certificate_factory`); the real `getInstance` builds the
///   same object through `GetInstance`.
/// * `javax/crypto/SecretKeyFactory` (5): the real engine drives a provider
///   SPI obtained through `GetInstance`; PBKDF2 answers are byte-identical.
/// * `java/security/Security` (9) and `sun/security/jca/GetInstance` (6). The
///   natives answered `getProviders()`/`getProvider(name)` with providers
///   `make_provider` ALLOCATED (constructor never run) and resolved services
///   through the per-VM side table, whose natively-served rows carry the
///   `sun.security.provider.Native` / `com.sun.crypto.provider.Native` marker
///   in place of a class name. Retired, the JDK's own `ProviderList` builds the
///   real `Sun`/`SunJCE`/`SunEC`/... through their constructors, and a
///   `Provider.Service` names its real implementation class.
///
/// # Measured (JDK 25.0.4, linux, `cvm-sbr-base` = dev `038a7f265`)
///
/// ```text
///   dial pre-screen, full suite (152 vectors), --real-jdk:
///     control                                     151/152   RJdkLogging
///     all 14 classes armed                        140/152   +11 (RCrypto RChaCha20Cipher
///                                                   RSslLiveSession RSslEndpointIdentification
///                                                   RBigIntMontgomery RJdkSecurity RJdkX509Intercept
///                                                   RSerial RJdkReflect RJitMultiArrayClass RJdkIntrinsics2)
///     Cipher+SecretKeyFactory+Mac                 151/152   = control
///     Security+GetInstance                        151/152   = control
///     SecretKeySpec+KeyPair*+KeyFactory+CertFac   150/152   +RJdkSecurity (KeyFactory, below)
///   built, full suite:
///     cvm-sbr-jca-1 (these six + Cipher)          151/152   = control, vector for vector
///     cvm-sbr-jca-2 (these six)                   151/152   = control (4 vectors that
///                                                   lost their HotSpot oracle to a full
///                                                   disk re-ran 4/4 green alone)
///   census native-won rows on the six, summed over vectors: 54 -> 0
///   SbrW1Jca probe, lines differing from HotSpot 25 (142 lines; drift = 0 in every arm):
///     base --real-jdk                              10
///     jca-1 --real-jdk                              5   (6 fixed, 0 regressed)
///     jca-2 --real-jdk                              6   (5 fixed, 0 regressed)
///     jca-2 + CRATONVM_UNRETIRE_NATIVE_SHADOW=<the six>   = base, 0 lines
///     jca-2 --compatible vs base --compatible       0 lines
///   strict corpus: no divergent-section family beyond the control's
///   SbrW1JcaPerf, jca-2 vs base interleaved: SHA-256, HMAC, AES-GCM all within noise
/// ```
///
/// Five of the six fixed lines are the provider-marker family and belong to
/// `Security`+`GetInstance`: `Provider.Service.getClassName()` for SHA-256 was
/// `sun.security.provider.Native` and `newInstance` threw
/// `ClassNotFoundException` for it; SunJCE's `Cipher.AES` named
/// `com.sun.crypto.provider.Native`; `Provider.keySet()`/`get` held no
/// `MessageDigest.*` row. The sixth, `Cipher.update(byte[],int,int)` on AES/CBC
/// returning an empty first block, is `Cipher`'s, and went with it (below).
///
/// # What is held, each with its measured reason
///
/// * `javax/crypto/Cipher` (35). Correct everywhere measured, suite-neutral,
///   and it FIXES the CBC `update` line above -- held for throughput alone.
///   Retired, `AES/GCM/NoPadding` runs SunJCE's `GaloisCounterMode` bytecode:
///   interleaved A/B, 4 rounds, 1 MiB x 8 per round, best-of-5 ms/MiB,
///   base 45.9-46.7 vs retired 186-203 (4.0-4.4x slower; HotSpot 13). SHA-256
///   and HMAC unchanged in the same runs (not retired). Needs a JIT that makes
///   `AESCrypt`/`GHASH` bytecode competitive, or a reviewed `Intrinsic` at the
///   SPI level (`com/sun/crypto/provider/*`) instead of at the engine.
/// * `java/security/MessageDigest` (17) + the three `java/security/MessageDigestSpi`
///   rows (`lib.rs` `register_security_natives`). Armed alone:
///   `ClassNotFoundException: sun.security.provider.Native` from every
///   `getInstance` -- the real engine resolves through the side table's marker
///   rows. Armed with `Security`+`GetInstance` the digests come out right, but
///   `digest(byte[],int,int)` then lands on the native
///   `MessageDigestSpi.engineDigest([BII)I`, which answers "MessageDigest state
///   missing post-GC or never initialized" for a real `SHA2` delegate, and
///   HMAC/PBKDF2 inherit it. The digest's state is the accumulator side table,
///   not the SPI's fields.
/// * `javax/crypto/Mac` (19). Correct HMACs armed, but `Mac.init(null)`
///   answers `Secret key expected` where HotSpot answers `No installed provider
///   supports this key: (null)`: the services come from the native side table
///   with NO attributes (`getAttribute("SupportedKeyFormats")` is null), so the
///   real `Service.supportsParameter(null)` says yes and the SPI's own message
///   surfaces. Nothing gained to set against it.
/// * `java/security/KeyPairGenerator` (12) and `java/security/KeyFactory` (10).
///   Real bytecode reaches `Provider.Service.newInstance` on a PLAIN
///   `Provider$Service` the side table built for a SunEC row, whose class name
///   is SunEC's dotted label (`sun.security.ec.ed.EdDSAKeyPairGenerator.Ed25519`,
///   `sun.security.ec.XDHKeyFactory.X25519`) that only SunEC's own
///   `ProviderService.newInstance` override understands: `ClassNotFoundException`
///   on Ed25519/X25519 key generation and on `RJdkSecurity`'s
///   advertised-versus-served KeyFactory sweep.
/// * `java/security/Signature` (22). Armed, the native `<clinit>` (a no-op) still
///   runs -- the dial does not reach class initialisation -- and the real
///   `getInstance` NPEs on the null `signatureInfo`. Not measured as a real
///   retirement in this wave.
/// * `java/security/Provider` (22), `Provider$Service` (5), `$EngineDescription`,
///   `$ServiceKey`. Armed, 29 probe lines regress and the answers DRIFT between
///   the first and later runs (1,668 of 5,200): the provider objects other
///   natives hand out were never constructed, so real `Provider` code meets an
///   uninitialised `Properties`. The side table IS the provider state. This is
///   the root blocker for everything held above.
///
/// `--compatible` is unchanged: a `SyntheticStub` registers and dispatches
/// normally there. Record:
/// `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_JCA_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/security/KeyPair", "getPrivate", "()Ljava/security/PrivateKey;"),
    ("java/security/KeyPair", "getPublic", "()Ljava/security/PublicKey;"),
    ("java/security/Security", "addProvider", "(Ljava/security/Provider;)I"),
    (
        "java/security/Security",
        "getAlgorithms",
        "(Ljava/lang/String;)Ljava/util/Set;",
    ),
    (
        "java/security/Security",
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
    ),
    (
        "java/security/Security",
        "getProvider",
        "(Ljava/lang/String;)Ljava/security/Provider;",
    ),
    (
        "java/security/Security",
        "getProviders",
        "()[Ljava/security/Provider;",
    ),
    (
        "java/security/Security",
        "getProviders",
        "(Ljava/lang/String;)[Ljava/security/Provider;",
    ),
    (
        "java/security/Security",
        "insertProviderAt",
        "(Ljava/security/Provider;I)I",
    ),
    ("java/security/Security", "removeProvider", "(Ljava/lang/String;)V"),
    (
        "java/security/Security",
        "setProperty",
        "(Ljava/lang/String;Ljava/lang/String;)V",
    ),
    (
        "java/security/cert/CertificateFactory",
        "generateCertificate",
        "(Ljava/io/InputStream;)Ljava/security/cert/Certificate;",
    ),
    (
        "java/security/cert/CertificateFactory",
        "generateCertificates",
        "(Ljava/io/InputStream;)Ljava/util/Collection;",
    ),
    (
        "java/security/cert/CertificateFactory",
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/cert/CertificateFactory;",
    ),
    (
        "java/security/cert/CertificateFactory",
        "getInstance",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/cert/CertificateFactory;",
    ),
    (
        "javax/crypto/SecretKeyFactory",
        "generateSecret",
        "(Ljava/security/spec/KeySpec;)Ljavax/crypto/SecretKey;",
    ),
    (
        "javax/crypto/SecretKeyFactory",
        "getAlgorithm",
        "()Ljava/lang/String;",
    ),
    (
        "javax/crypto/SecretKeyFactory",
        "getInstance",
        "(Ljava/lang/String;)Ljavax/crypto/SecretKeyFactory;",
    ),
    (
        "javax/crypto/SecretKeyFactory",
        "getInstance",
        "(Ljava/lang/String;Ljava/lang/String;)Ljavax/crypto/SecretKeyFactory;",
    ),
    (
        "javax/crypto/SecretKeyFactory",
        "getProvider",
        "()Ljava/security/Provider;",
    ),
    (
        "javax/crypto/spec/SecretKeySpec",
        "<init>",
        "([BLjava/lang/String;)V",
    ),
    (
        "javax/crypto/spec/SecretKeySpec",
        "getAlgorithm",
        "()Ljava/lang/String;",
    ),
    ("javax/crypto/spec/SecretKeySpec", "getEncoded", "()[B"),
    (
        "javax/crypto/spec/SecretKeySpec",
        "getFormat",
        "()Ljava/lang/String;",
    ),
    (
        "sun/security/jca/GetInstance",
        "getInstance",
        "(Ljava/lang/String;Ljava/lang/Class;Ljava/lang/String;)Lsun/security/jca/GetInstance$Instance;",
    ),
    (
        "sun/security/jca/GetInstance",
        "getInstance",
        "(Ljava/lang/String;Ljava/lang/Class;Ljava/lang/String;Ljava/lang/String;)Lsun/security/jca/GetInstance$Instance;",
    ),
    (
        "sun/security/jca/GetInstance",
        "getInstance",
        "(Ljava/lang/String;Ljava/lang/Class;Ljava/lang/String;Ljava/security/Provider;)Lsun/security/jca/GetInstance$Instance;",
    ),
    (
        "sun/security/jca/GetInstance",
        "getService",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/Provider$Service;",
    ),
    (
        "sun/security/jca/GetInstance",
        "getService",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)Ljava/security/Provider$Service;",
    ),
    // Declared (with `Code`) on 25 only; 17 and 21 declare the `List` form, so
    // there no bytecode names this descriptor and dropping its native is inert.
    (
        "sun/security/jca/GetInstance",
        "getServices",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/util/Iterator;",
    ),
];

/// SBR wave 1, lane `uri`, 2026-09-28 -- `java/net/URI`, retired whole: all
/// 30 triples, every `Bridge` registration the class has under `--real-jdk`
/// (one each in the 25/linux kind map). `java.net.URI` is a value class whose
/// real bytecode is a pure-Java RFC 2396 parser; there was never anything for
/// a native to supply but the parse, and the natives' parse disagreed with the
/// JDK's own.
///
/// # What made it retirable: the producers, not the rows
///
/// The rows were always correct to retire. What was not real was the STATE the
/// VM's own producers left in a URI, because the natives were its only reader:
///
/// * `uri_publish_named` (every allocate-then-publish producer: `Path.toUri`
///   ×2 registrars, `HttpRequestImpl`/`HttpResponseImpl.uri`, `URL.toURI`'s
///   fallback, `make_uri` behind `normalize`/`resolve`/`relativize`/`create`)
///   wrote the fields from `uri_split`, which is not the JDK grammar -- an
///   opaque URI got a non-null `path` (the JDK's `isOpaque()` IS
///   `path == null`), and `decodedPath` came from a lossy `&str` while `path`
///   was re-derived from UTF-16 units. **Measured, not argued**: the dial arm
///   `CRATONVM_ENFORCE_NATIVE_SHADOW=java/net/URI` on the base binary failed
///   `RJdkBridge1` (`surrog`: `URL.toURI().getPath()` with a lone surrogate
///   read U+FFFD through real `getPath()` -> `decodedPath`), a vector green
///   with the dial off.
/// * Fixed at the producer (`native-builtins/src/net_phase_e.rs`): under
///   `--real-jdk` every one of them now builds the URI through the JDK's own
///   `URI(String)` constructor (`jdk_only_constructed_uri`; for an already
///   allocated receiver `uri_adopt_jdk_parse` copies a constructed one's slots
///   in, so a refusal leaves it untouched for the old publish). `Compatible`
///   never takes that branch.
/// * The READER side, the "owned both ends of a convention" defect
///   `RETIRED_SHADOW_L4_BACKED_OUT_TRIPLES` names: real `resolve`,
///   `normalize` and `relativize` assemble a URI field by field and leave its
///   `string` cache null until `toString()`. `uri_raw_string` -- what
///   `Path.of(URI)`, `FileSystemProvider.getPath(URI)`, the HTTP builders and
///   the proxy selector read -- answered "" for such a URI, because only a
///   native producer had ever handed it one. It now rebuilds the text the
///   JDK's `defineString` way under `--real-jdk` (`uri_define_string`), and
///   `nio_file.rs`'s `p57_uri_full_text` asks it before scanning raw slots.
///
/// # `java/net/URL` is NOT here, and why
///
/// The census has 5 native-won `URL` rows (`getDefaultPort`, `openConnection`,
/// `toExternalForm`, `toString`, `toURI`), and 10 `Bridge` registrations
/// remain on the class. Every JDK body among them goes through `handler`
/// (`toExternalForm` -> `handler.toExternalForm(this)`; `toURI`'s
/// `isBuiltinStreamHandler(handler)` once `authority != null`), and this VM
/// still mints handler-less URLs at about ten sites outside
/// `jdk_only_constructed_url` (`try_alloc_concurrent_synthetic("java/net/URL", ..)`
/// in `lang_class.rs`, `lib.rs`, `servlet.rs`, `phases_late.rs` ×3,
/// `jar_manifest.rs`, `jboss_module_loader.rs` ×2, `URI.toURL`'s old
/// fallback). The same precondition `RETIRED_SHADOW_URL_ACCESSOR_TRIPLES`
/// held `getDefaultPort` on: give every minted URL a handler, then retire.
/// `URL.toURI`'s own producer IS fixed here (it constructs through the real
/// `URI(String)` under `--real-jdk`), which is what closed `RJdkBridge1`.
///
/// # Measured
///
/// linux/x86_64, JDK 25.0.4+7, against `cvm-sbr-base` (dev `038a7f265`):
///
/// ```text
///   SbrW1Uri.java (225 rows, vs HotSpot 25)
///     base --real-jdk 5 diffs   this --real-jdk 0 diffs, drift 0
///     this + CRATONVM_UNRETIRE_NATIVE_SHADOW=java/net/URI  == base, row for row
///     this --compatible                                     == base, row for row
///   suite.sh SUITE=all --real-jdk   151/1 both, vector-identical (RJdkLogging)
///   native-won java/net/URI rows    26 -> 0
///   strict corpus                   same six divergent probes as the control
///   UriMicro (200k parse+accessors) 1513-2264 ms/round -> 423-672 ms/round
/// ```
///
/// The five probe rows retirement fixes: `jar:nested:` `toURL()` succeeding
/// where HotSpot refuses, the wrong exception from a relative `toURL()`,
/// `resolve(null)`'s message, `%2f` vs `%2F` equality, and `resolve("x y")`
/// accepting an illegal character. Full record:
/// `docs/feature-designs/native-dispatch-architecture.md`.
///
/// `--compatible` is unchanged: the re-tag makes these `SyntheticStub`, which
/// that mode registers and dispatches exactly as it did the `Bridge`, and every
/// producer/reader change above is behind `is_jdk_only()`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_URI_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/net/URI", "<init>", "(Ljava/lang/String;)V"),
    ("java/net/URI", "compareTo", "(Ljava/lang/Object;)I"),
    ("java/net/URI", "compareTo", "(Ljava/net/URI;)I"),
    (
        "java/net/URI",
        "create",
        "(Ljava/lang/String;)Ljava/net/URI;",
    ),
    ("java/net/URI", "equals", "(Ljava/lang/Object;)Z"),
    ("java/net/URI", "getAuthority", "()Ljava/lang/String;"),
    ("java/net/URI", "getFragment", "()Ljava/lang/String;"),
    ("java/net/URI", "getHost", "()Ljava/lang/String;"),
    ("java/net/URI", "getPath", "()Ljava/lang/String;"),
    ("java/net/URI", "getPort", "()I"),
    ("java/net/URI", "getQuery", "()Ljava/lang/String;"),
    ("java/net/URI", "getRawAuthority", "()Ljava/lang/String;"),
    ("java/net/URI", "getRawFragment", "()Ljava/lang/String;"),
    ("java/net/URI", "getRawPath", "()Ljava/lang/String;"),
    ("java/net/URI", "getRawQuery", "()Ljava/lang/String;"),
    (
        "java/net/URI",
        "getRawSchemeSpecificPart",
        "()Ljava/lang/String;",
    ),
    ("java/net/URI", "getRawUserInfo", "()Ljava/lang/String;"),
    ("java/net/URI", "getScheme", "()Ljava/lang/String;"),
    (
        "java/net/URI",
        "getSchemeSpecificPart",
        "()Ljava/lang/String;",
    ),
    ("java/net/URI", "getUserInfo", "()Ljava/lang/String;"),
    ("java/net/URI", "hashCode", "()I"),
    ("java/net/URI", "isAbsolute", "()Z"),
    ("java/net/URI", "isOpaque", "()Z"),
    ("java/net/URI", "normalize", "()Ljava/net/URI;"),
    ("java/net/URI", "parseServerAuthority", "()Ljava/net/URI;"),
    (
        "java/net/URI",
        "relativize",
        "(Ljava/net/URI;)Ljava/net/URI;",
    ),
    (
        "java/net/URI",
        "resolve",
        "(Ljava/lang/String;)Ljava/net/URI;",
    ),
    ("java/net/URI", "resolve", "(Ljava/net/URI;)Ljava/net/URI;"),
    ("java/net/URI", "toString", "()Ljava/lang/String;"),
    ("java/net/URI", "toURL", "()Ljava/net/URL;"),
];

/// SBR wave 1, lane JMX, 2026-09-28 -- `javax/management/ObjectName`, all 23
/// triples `register_object_name` (`native-builtins/src/jmx.rs`) puts on it,
/// 24 registrations (`getCanonicalName` is registered twice).
///
/// # Why the whole class, and why only now
///
/// H6-1 (`docs/feature-designs/native-dispatch-architecture.md`
/// §2.3) established that this family is ALL-OR-NOTHING: the natives keep one
/// source-order string in `_canonicalName` and leave `_kp_array`, `_ca_array`,
/// `_propertyList`, `_domain_length` and the pattern flags unwritten, while
/// real `ObjectName$Property` is three offsets INTO the key-sorted
/// `_canonicalName`. No subset of the natives can hand an instance to real
/// bytecode, and no hand-fill of the real fields is sound. The only coherent
/// move is the one this table makes: every triple at once, so under
/// `--real-jdk` every `ObjectName` is built by the real constructor and read by
/// the real accessors. The checklist:
///
/// * **Bytecode present:** all 23 are declared with `Code` on 17, 21 and 25
///   (`javap -p -s --module java.management`), none `ACC_NATIVE`. The class has
///   no VM leaf at all.
/// * **Producers go through the real parser:** the two native producers that
///   survive the retirement both reach `ObjectName.getInstance(String)` through
///   `invoke` -- `buffer_pool_get_object_name` (`shared_secrets_bridge.rs`)
///   already did; `native_platform_managed_object_name` (the eight MXBean
///   interface `getObjectName` rows, which bind abstract methods and stay
///   native) called `object_name_new` and now also calls `getInstance`, which
///   is the H6-1 §2.5 blocker. Under `--compatible` that call dispatches to the
///   still-registered native `getInstance`, i.e. to `object_name_new` on the
///   same text, so the object is unchanged there. The MBeanServer registry's
///   reconstruction arm (`mbs_resolve_oname`) already used `getInstance`.
/// * **Readers do not decode the native convention:** every MBeanServer-side
///   reader (`object_name_key`, `mbs_query_set`, `mbs_domains`) asks the NAME
///   through `invoke_virtual` (`getCanonicalName`/`toString`, `apply`,
///   `getDomain`), never through `_canonicalName` directly, and the key it
///   derives is re-canonicalised by `canonical_object_name_text`, which is
///   idempotent on the real sorted form. `object_name_text` -- the one field
///   reader -- is only called from the natives this table retires (and from
///   `object_name_new`, whose last caller is the `--compatible` `getInstance`
///   native).
/// * **No second registrar:** `register_object_name` is the only registrar of
///   the class and runs under an explicit `Bridge`; nothing re-registers a
///   triple under `Intrinsic`.
///
/// # What it fixes
///
/// Under `--real-jdk` on the control binary the natives answered wrongly
/// against HotSpot 25 (`SbrW1Jmx.java`): `isPropertyValuePattern`
/// false for `d:k=*` (the native model never wrote `_property_value_pattern`),
/// the `apply` matrix wrong in 9 of its 14 rows, a `,*` in the middle of
/// the list (`d:z=1,y=2,*,x=3`) not recognised as a pattern at all, a
/// `Hashtable` constructor in insertion rather than `Hashtable` order,
/// `d:k=v,k=w` and `d:k=a:b` accepted instead of rejected, and every
/// `MalformedObjectNameException` message different. See the record for the
/// arms.
///
/// # Measured
///
/// Linux/x86_64, JDK 25.0.4+7, control `038a7f265` vs this table, fat LTO:
///
/// ```text
///   SbrW1Jmx, 45 lines vs HotSpot 25   control --real-jdk 13 differ, drift 0
///                                      this    --real-jdk  0 differ, drift 0
///     + UNRETIRE=javax/management/ObjectName   == control (23 table rows)
///     --compatible                             == control --compatible
///   SUITE=all --real-jdk               151/1 both, 152 vectors, 0 differ
///   strict corpus                      same diverged set, same real lines
///   census native-shadows-bytecode     26 rows -> 0 on this class
/// ```
///
/// The dial preview (`CRATONVM_ENFORCE_NATIVE_SHADOW=javax/management/ObjectName`)
/// cost `RJdkJmx`; the table does not. The dial leaves the MXBean
/// `getObjectName` producer fabricating and does not reach the reflective
/// `readObject` call. Record:
/// `docs/feature-designs/native-dispatch-architecture.md`.
///
/// # Left out
///
/// Nothing on `ObjectName`. `javax/management/ObjectInstance.getObjectName` /
/// `getClassName` (1 registration each, real fields read by name) never won a
/// dispatch in the control census, so it is not this wave's. The
/// `javax/management/MBeanServer` rows and the MXBean-interface
/// `getObjectName` rows bind abstract interface methods: nothing to yield to.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_JMX_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "javax/management/ObjectName",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "javax/management/ObjectName",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
    ),
    (
        "javax/management/ObjectName",
        "<init>",
        "(Ljava/lang/String;Ljava/util/Hashtable;)V",
    ),
    (
        "javax/management/ObjectName",
        "_getKeyPropertyList",
        "()Ljava/util/Map;",
    ),
    (
        "javax/management/ObjectName",
        "apply",
        "(Ljavax/management/ObjectName;)Z",
    ),
    (
        "javax/management/ObjectName",
        "equals",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "javax/management/ObjectName",
        "getCanonicalKeyPropertyListString",
        "()Ljava/lang/String;",
    ),
    (
        "javax/management/ObjectName",
        "getCanonicalName",
        "()Ljava/lang/String;",
    ),
    (
        "javax/management/ObjectName",
        "getDomain",
        "()Ljava/lang/String;",
    ),
    (
        "javax/management/ObjectName",
        "getInstance",
        "(Ljava/lang/String;)Ljavax/management/ObjectName;",
    ),
    (
        "javax/management/ObjectName",
        "getInstance",
        "(Ljavax/management/ObjectName;)Ljavax/management/ObjectName;",
    ),
    (
        "javax/management/ObjectName",
        "getKeyProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
    ),
    (
        "javax/management/ObjectName",
        "getKeyPropertyList",
        "()Ljava/util/Hashtable;",
    ),
    (
        "javax/management/ObjectName",
        "getKeyPropertyListString",
        "()Ljava/lang/String;",
    ),
    (
        "javax/management/ObjectName",
        "getSerializedNameString",
        "()Ljava/lang/String;",
    ),
    ("javax/management/ObjectName", "hashCode", "()I"),
    ("javax/management/ObjectName", "isDomainPattern", "()Z"),
    ("javax/management/ObjectName", "isPattern", "()Z"),
    (
        "javax/management/ObjectName",
        "isPropertyListPattern",
        "()Z",
    ),
    ("javax/management/ObjectName", "isPropertyPattern", "()Z"),
    (
        "javax/management/ObjectName",
        "quote",
        "(Ljava/lang/String;)Ljava/lang/String;",
    ),
    (
        "javax/management/ObjectName",
        "readObject",
        "(Ljava/io/ObjectInputStream;)V",
    ),
    (
        "javax/management/ObjectName",
        "toString",
        "()Ljava/lang/String;",
    ),
];

/// SBR wave 1, lane `bytes`, 2026-09-28: 15 rows over four small, stateless
/// or small-state helpers the dev census found native-won under `--real-jdk`
/// (`/data/sbr/nw-base-byclass.txt`: 3 each). Every row is a `Bridge` over a
/// method with `Code` on the JDK 21 and 25 images (declared, or for the five
/// `StringBuilder` rows inherited from `AbstractStringBuilder`); JDK 17 has
/// neither `StringBuilder.repeat` nor `Buffer$2`, so nothing there can call
/// those.
///
/// # What each group was, and why real bytecode is the better producer
///
/// * **`java/lang/StringBuilder` — the five rows
///   [`RETIRED_SHADOW_STRINGBUILDER_TRIPLES`] excluded.** Its doc comment left
///   `append(AbstractStringBuilder)`, `appendNull`, `getCoder`, `getValue` and
///   `repeat(CI)` out as "a triple with no receiver of its own", expecting
///   dispatch to reach `AbstractStringBuilder`'s retired copies. It does not:
///   a call on a `StringBuilder` receiver asks the receiver-keyed triple first,
///   so these five kept winning over the inherited bytecode (census: three of
///   them native-won in the dev suite). Same bodies, same real fields as the
///   118 rows already retired, so the reason for leaving them no longer holds.
/// * **`java/util/AbstractMap$SimpleEntry` — `getKey`, `getValue`, `hashCode`,
///   `equals`, `toString`.** Every producer of a `SimpleEntry` puts the key and
///   value in slots 0 and 1, which ARE the real `key`/`value` fields, so the
///   real bodies read what the natives read.
/// * **`java/nio/Bits$1.getName`** — two instructions returning `"direct"`;
///   `native_override.rs`' `is_direct_buffer_pool_counter_override` already
///   said the JDK's answer is right and forced only the three counters.
/// * **`java/nio/Buffer$2`: `getBufferBase`, both `newDirectByteBuffer`s,
///   `scaleShifts`.** Each real body is one call: `buffer.base()`,
///   `new DirectByteBuffer(addr, cap[, att, seg])`, `buffer.scaleShifts()`.
///   The natives re-derived the same answers (a by-name `hb` read, a class-name
///   token scan) and `scaleShifts(null)` answered 0 where the JDK NPEs.
///
/// # `jdk/internal/util/ByteArray` is NOT here — it became a reviewed `Intrinsic`
///
/// The lane's first cut retired all 20 `ByteArray` rows: the `Bridge` bodies
/// gave different answers from the JDK (no bounds check, no null check, raw NaN
/// bits from `setDouble`/`setFloat`). The real bodies were right but cost
/// 1.6-2.6x on `DataInputStream`/`DataOutputStream` (`SbrW1BytesBench`). By the
/// orchestrator's decision the natives were instead rewritten to the JDK's exact
/// semantics and registered `NativeKind::Intrinsic` (see the registration in
/// `native-builtins/src/lib.rs`). A table entry re-tags only an effective
/// `Bridge`, so listing them here would be inert; they are absent, and so is
/// the `jdk/internal/util/ByteArray` prefix.
///
/// # Left out, on purpose
///
/// * `AbstractMap$SimpleEntry.setValue`: the entry-set views in
///   `native-collections` (`alloc_live_entry` and its callers) mint a
///   `SimpleEntry` ONE SLOT WIDER than declared and keep the source map in
///   that undeclared slot 2; only the native `setValue` reads it and writes
///   through to the map. The real body would silently stop writing through —
///   the "reader that owned both ends of a convention" shape.
/// * `Bits$1.getCount` / `getMemoryUsed` / `getTotalCapacity`: the JDK's
///   counters are maintained by `Bits.reserveMemory`, which this VM never
///   reaches (its own allocator serves `allocateDirect`), so the real bodies
///   read zero forever. Measured: the enforcement dial on them turns
///   `RBufferPoolCount` red and the probe's `directCount>0` from `true` to
///   `false`.
/// * `Bits$1.getObjectName`, `Buffer$1.scaleShifts`,
///   `Buffer$2.acquireSession(Buffer)MemorySegment$Scope`: no image declares
///   them, so there is no bytecode to yield to.
/// * `Buffer$2.reserveMemory` / `unreserveMemory` / `hasSession` /
///   `isThreadConfined` / `pageSize`: the first two are the VM's own
///   direct-memory accounting (`RETIRED_SHADOW_L4_W19_TRIPLES`' note); the
///   session pair answers from the session shim the JOTP wave keeps; none of
///   the five was native-won in the census, so none was measured here.
///
/// # Measured
///
/// Azure linux, JDK 25.0.4+7, base `038a7f265` against this table plus the
/// `ByteArray` intrinsic (`6059eca13`):
///
/// ```text
///   regression suite SUITE=all --real-jdk   base 151/1   lane 151/1   per-vector identical (RJdkLogging)
///     native-won triples (census UNION)     730 -> 706
///   SbrW1Bytes vs HotSpot (63 lines)        base 20 differ   lane 0, --real-jdk and --nojit; drift 0
///   SbrW1BytesBench, base -> lane           DataInput 0.95-0.97x, DataOutput 0.80-0.85x (JIT),
///                                           0.94-0.97x --nojit; ObjectStream and UUID unchanged
/// ```
///
/// The record below has the per-arm detail.
///
/// docs/feature-designs/native-dispatch-architecture.md
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_BYTES_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/nio/Bits$1", "getName", "()Ljava/lang/String;"),
    (
        "java/nio/Buffer$2",
        "getBufferBase",
        "(Ljava/nio/Buffer;)Ljava/lang/Object;",
    ),
    (
        "java/nio/Buffer$2",
        "newDirectByteBuffer",
        "(JI)Ljava/nio/ByteBuffer;",
    ),
    (
        "java/nio/Buffer$2",
        "newDirectByteBuffer",
        "(JILjava/lang/Object;Ljava/lang/foreign/MemorySegment;)Ljava/nio/ByteBuffer;",
    ),
    ("java/nio/Buffer$2", "scaleShifts", "(Ljava/nio/Buffer;)I"),
    (
        "java/util/AbstractMap$SimpleEntry",
        "equals",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/AbstractMap$SimpleEntry",
        "getKey",
        "()Ljava/lang/Object;",
    ),
    (
        "java/util/AbstractMap$SimpleEntry",
        "getValue",
        "()Ljava/lang/Object;",
    ),
    ("java/util/AbstractMap$SimpleEntry", "hashCode", "()I"),
    (
        "java/util/AbstractMap$SimpleEntry",
        "toString",
        "()Ljava/lang/String;",
    ),
];

/// SBR wave 2, lane NIOCH (2026-09-28): the socket / channel / selector
/// implementation classes. **One row retires; the family is HELD**, and the
/// size is the finding.
///
/// # The one row
///
/// `sun/nio/ch/SocketDispatcher.close(Ljava/io/FileDescriptor;)V`, native-won
/// in 3 vectors of the int1 census (every real `java.net.Socket` /
/// `ServerSocket` close goes `NioSocketImpl.close` -> `nd.close(fd)`). One
/// `Bridge` registration (`native-io/src/net.rs`, `register_sun_nio_ch_net`,
/// ambient `Bridge`). Its body was `close_net_fd_descriptor(fd, true)`. The
/// bytecode it shadows:
///
/// * 21 / 25: not declared on `SocketDispatcher`; inherited
///   `UnixDispatcher.close(fd)` = `close0(fd)`, ACC_NATIVE, registered in the
///   same function to `net_close` -- which IS `close_net_fd_descriptor(fd,
///   true)`. Retiring the shadow therefore runs one bytecode frame and lands
///   on the identical close.
/// * 17: declared, `FileDispatcherImpl.close0(fd)`, bound to the FILE table's
///   `native_fd_dispatcher_close0` -- which would miss every `net_sockets()`
///   id (disjoint by construction, `next_net_fd`) and leak the OS socket while
///   reporting success. Fixed in the commit this table lands with: under
///   `--real-jdk` that native closes a descriptor naming a live socket id the
///   way `net_close` does (`net::close_if_net_descriptor`). `--compatible`
///   keeps the shadow, never reaches the branch, and is unchanged.
///
/// # Why the family is held (measured, `cvm-sbr-int1`, dial and probe)
///
/// Under this VM the natives ARE the object model, three ways over:
///
/// 1. **Minted without `<init>`, state in side tables.** `SocketChannel.open`,
///    `ServerSocketChannel.open`, the `SelectorProviderImpl` factories and
///    `Selector.open` allocate `sun.nio.ch.SocketChannelImpl` /
///    `ServerSocketChannelImpl` / `EPollSelectorImpl` (`alloc_channel_as_impl`)
///    and keep their state in the identity-keyed `chan_fields` table plus the
///    `tcp_registry()` of `TcpStream`/`TcpListener`s. The real fields (`fd`,
///    `fdVal`, `state`, `readLock`, `writeLock`, `acceptLock`, the selector's
///    `interruptLock`, ...) stay null/0. Dial on the impl classes: every probe
///    row that touches a channel NPEs on one of them.
/// 2. **The NIO fd space is not the OS fd space.** Yielding the factories too
///    (real `SocketChannelImpl.<init>`) gets `Net.socket0` ids from
///    `net_sockets()` starting at `0x4000_0000`. The real `EPollSelectorImpl`
///    hands those to `EPoll.ctl` -- which passes them to `epoll_ctl(2)`
///    verbatim -- and `EPoll.wait` is a stub that returns 0. A real selector
///    cannot see a real channel. Measured: the probe's first two channel rows
///    diverge and the third hangs.
/// 3. **UDP and the async group are not modelled below the dispatcher.**
///    `Net.socket0` ignores its `stream` flag (a datagram bind builds a
///    `TcpListener`); `DatagramChannelImpl.send0`/`receive0` and
///    `IOUtil.makePipe` are unregistered (`UnsatisfiedLinkError`, measured);
///    `java.net.DatagramSocket` / `MulticastSocket` constructors write fd-table
///    ids into raw slots instead of the real `delegate`; the
///    `SimpleAsynchronousFileChannelImpl` carrier appends a private slot map
///    (`afc_base`) with `fdObj`/`threads`/`closeLock` null, and
///    `FileLockImpl.release` exists to keep the real `release` off it.
///
/// Also held on their own: `NativeThread.current()J` (native-won in 8
/// vectors) is ACC_NATIVE on JDK 17 -- a `SyntheticStub` there is dropped and
/// becomes an `UnsatisfiedLinkError`; `NativeThread.signal(J)V` and
/// `NativeDispatcher.preClose` shadow bytecode whose callee `signal0` is not
/// registered at all.
///
/// Dial pre-screen on the whole family, `SUITE=all --real-jdk`: 152/0 ->
/// 143/9 (RChannelToString, RSocketChannelInterrupt, RSslEndpointIdentification,
/// RSocketFastIo, RMulticastSocketClose, RMulticastSocketLoopback, RJdkNio,
/// RJdkNet, RJdkAsyncChannel), identical with the factories yielded too.
/// `SocketDispatcher` alone: see the record.
/// `the_sbr_nioch_wave_is_one_dispatcher_row_and_holds_the_channel_family`
/// holds the rest. Record:
/// `docs/feature-designs/profile-guided-inlining.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_NIOCH_TRIPLES: &[(&str, &str, &str)] = &[(
    "sun/nio/ch/SocketDispatcher",
    "close",
    "(Ljava/io/FileDescriptor;)V",
)];

