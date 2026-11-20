#[allow(unused_imports)]
use super::*;

pub(in crate::class_manager) fn synthetic_stub_fields(name: &str) -> Vec<cratonvm_reader::field::ClassFileField> {
    use cratonvm_reader::field::ClassFileField;

    /// Helper to create N unnamed instance fields (for synthetic objects
    /// whose native code accesses fields by index, not by name).
    fn instance_fields(n: usize) -> Vec<ClassFileField> {
        (0..n)
            .map(|i| ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc(&format!("_f{i}")),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/Object;"),
                attributes: vec![],
            })
            .collect()
    }

    /// The static fields a synthetic JDK enum needs: one per constant, in
    /// declaration (= ordinal) order, plus the `$VALUES` array its `<clinit>`
    /// publishes and `EnumSet.allOf` / `Class.getEnumConstants` read.
    ///
    /// One builder rather than a hand-written list per enum, for the reason
    /// `publish_synthetic_enum_constants` gives on the other half of this pair:
    /// the enum stubs that predate it are the survivors of copies that drifted.
    fn enum_constant_fields(descriptor: &str, constants: &[&str]) -> Vec<ClassFileField> {
        let mk = |n: &str, d: &str| ClassFileField {
            access_flags: FieldAccessFlags::PUBLIC
                | FieldAccessFlags::STATIC
                | FieldAccessFlags::FINAL,
            name: cratonvm_types::intern_arc(n),
            descriptor: cratonvm_types::intern_arc(d),
            attributes: vec![],
        };
        let mut fields: Vec<ClassFileField> = constants.iter().map(|c| mk(c, descriptor)).collect();
        fields.push(mk("$VALUES", &format!("[{descriptor}")));
        fields
    }

    fn named_field(name: &str, descriptor: &str) -> ClassFileField {
        ClassFileField {
            access_flags: FieldAccessFlags::empty(),
            name: cratonvm_types::intern_arc(name),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        }
    }

    /// A slot this VM parks its OWN value in — an fd, a wrapped stream, a
    /// discriminator — for which the real JDK class declares no field.
    ///
    /// Spelled `_vmN` so the L4 shadow-layout diff reports it
    /// (`SlotVerdict::VmInternal`) whenever a real field turns out to live at
    /// that index. The two alternatives both lie: naming the slot after the real
    /// field makes the diff agree with the overlay, and leaving it anonymous
    /// (`_fN`) makes the diff say nothing at all — which is how the fd that
    /// `Files.newBufferedWriter` writes into `java.io.Writer.writeBuffer` read
    /// as a harmless `pad` for a day.
    ///
    /// A `_vmN` slot past the end of the real layout is NOT reported: that is
    /// the shape to aim for (see `FIELD_EXTRA_OFFSET_*` in
    /// `native-builtins/src/lang_class.rs`, which anchors CratonVM's extra
    /// reflection metadata past `class_num_total_fields`).
    fn vm_internal_field(index: usize) -> ClassFileField {
        ClassFileField {
            access_flags: FieldAccessFlags::empty(),
            name: cratonvm_types::intern_arc(&format!("_vm{index}")),
            descriptor: cratonvm_types::intern_arc("Ljava/lang/Object;"),
            attributes: vec![],
        }
    }

    /// Pad a hand-written field list out to `total` INSTANCE fields.
    ///
    /// The factory (`alloc_concurrent_synthetic(ctx, name, n)`) and bytecode
    /// `new` must agree on how many slots an object of a synthesized class has:
    /// `new` sizes it from THIS table, and a native written against the factory
    /// shape that writes past the end has its write SILENTLY DISCARDED by
    /// `set_field`. Five separate bugs of exactly that shape were found in two
    /// days (`HttpExchange`, `HttpServer`, `sun/net/httpserver/HttpServerImpl`,
    /// `DatagramSocket`, `DatagramPacket`/`Preferences`), which is why
    /// `t9c_synthetic_field_tables_cover_their_factories` now enforces the
    /// invariant for the whole table.
    ///
    /// Padding preserves the named slots and their indices exactly and appends
    /// anonymous ones, so existing `set_field_by_name` and raw-index access both
    /// keep working.
    fn pad_to(mut fields: Vec<ClassFileField>, total: usize) -> Vec<ClassFileField> {
        let existing = fields
            .iter()
            .filter(|f| !f.access_flags.contains(FieldAccessFlags::STATIC))
            .count();
        for i in existing..total {
            fields.push(ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc(&format!("_f{i}")),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/Object;"),
                attributes: vec![],
            });
        }
        fields
    }

    match name {
        // String: 2 instance fields (value char[], hash int) — enough for
        // synthetic mode.  When the real JDK String class is loaded from a
        // .class file, its actual field count (e.g. 4 in JDK 25) is used
        // instead; see `create_java_string` in vm_object.rs.
        "java/lang/String" => instance_fields(2),
        // System has static fields for streams
        "java/lang/System" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC | FieldAccessFlags::STATIC,
                name: cratonvm_types::intern_arc("in"),
                descriptor: cratonvm_types::intern_arc("Ljava/io/InputStream;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC | FieldAccessFlags::STATIC,
                name: cratonvm_types::intern_arc("out"),
                descriptor: cratonvm_types::intern_arc("Ljava/io/PrintStream;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC | FieldAccessFlags::STATIC,
                name: cratonvm_types::intern_arc("err"),
                descriptor: cratonvm_types::intern_arc("Ljava/io/PrintStream;"),
                attributes: vec![],
            },
        ],
        // StringBuilder/StringBuffer: 2 fields (backing char[], count)
        "java/lang/StringBuilder" | "java/lang/StringBuffer" => instance_fields(2),
        "java/util/Collections" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: cratonvm_types::intern_arc("EMPTY_LIST"),
                descriptor: cratonvm_types::intern_arc("Ljava/util/List;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: cratonvm_types::intern_arc("EMPTY_SET"),
                descriptor: cratonvm_types::intern_arc("Ljava/util/Set;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: cratonvm_types::intern_arc("EMPTY_MAP"),
                descriptor: cratonvm_types::intern_arc("Ljava/util/Map;"),
                attributes: vec![],
            },
        ],
        "java/lang/Enum" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("name"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/String;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("ordinal"),
                descriptor: cratonvm_types::intern_arc("I"),
                attributes: vec![],
            },
        ],
        // `InvocationTargetException` declares ONE field of its own on JDK 25,
        // `target`; everything else it has is `Throwable`'s. The arm used to
        // re-declare `Throwable`'s six (`backtrace` .. `suppressedExceptions`)
        // on a chain that already reaches the three-slot `Throwable` model
        // (through `java/lang/reflect/ReflectiveOperationException`), so a
        // fabricated one was 10 wide and the real one was floored to 13 (10
        // with the stub-extent cap) against a real 7, with no compact layout.
        // Every reader and writer of `target` resolves it BY NAME
        // (`lang_misc.rs` `native_invocation_target_exception_*`,
        // `capture_then_assign_subclass_fields`, `lang_class.rs`
        // `wrap_as_invocation_target_exception`), and the `Throwable` slots go
        // through `throwable_field_index` / `synthetic_throwable_slot` (0..2).
        // JIT round 16 wave 17 (lane fieldsite3, FS2-2): a fabricated one is
        // now `Throwable`'s 3 + `target` = 4, and the real one floors to 7.
        "java/lang/reflect/InvocationTargetException" => {
            vec![named_field("target", "Ljava/lang/Throwable;")]
        }
        // `java.lang.Throwable` — THREE fields, and only `Throwable` itself.
        //
        // The three are `synthetic_throwable_slot`'s own map, which is the one
        // definition of a synthetic throwable's layout:
        // `detailMessage` = 0, `cause` = 1, `suppressedExceptions` = 2. This
        // arm declared TWO, so slot 2 fell outside a bare synthetic
        // `Throwable` and `write_throwable_field`'s
        // `slot < object_num_fields(this)` guard dropped every
        // `addSuppressed` on one.
        //
        // Every SUBCLASS is deliberately absent, and that is the fix for the
        // compounding described on `throwable_like_inherits_its_chain`: the
        // count here is a class's OWN fields, appended after its parent's, and
        // an exception hierarchy is deep. Declaring three at each level made
        // `java/io/FileNotFoundException` claim twelve slots for a three-slot
        // model, and in real-JDK mode stacked them on top of a real
        // `Throwable`'s six.
        "java/lang/Throwable" => instance_fields(3),
        "java/lang/Boolean" => {
            let mut fields = vec![
                ClassFileField {
                    access_flags: FieldAccessFlags::PUBLIC
                        | FieldAccessFlags::STATIC
                        | FieldAccessFlags::FINAL,
                    name: cratonvm_types::intern_arc("TRUE"),
                    descriptor: cratonvm_types::intern_arc("Ljava/lang/Boolean;"),
                    attributes: vec![],
                },
                ClassFileField {
                    access_flags: FieldAccessFlags::PUBLIC
                        | FieldAccessFlags::STATIC
                        | FieldAccessFlags::FINAL,
                    name: cratonvm_types::intern_arc("FALSE"),
                    descriptor: cratonvm_types::intern_arc("Ljava/lang/Boolean;"),
                    attributes: vec![],
                },
                ClassFileField {
                    access_flags: FieldAccessFlags::PUBLIC
                        | FieldAccessFlags::STATIC
                        | FieldAccessFlags::FINAL,
                    name: cratonvm_types::intern_arc("TYPE"),
                    descriptor: cratonvm_types::intern_arc("Ljava/lang/Class;"),
                    attributes: vec![],
                },
            ];
            fields.extend(instance_fields(1));
            fields
        }
        // Wrapper types: 1 instance field (primitive value) + static TYPE field
        // (Integer.TYPE == int.class, etc.)
        "java/lang/Integer"
        | "java/lang/Long"
        | "java/lang/Float"
        | "java/lang/Double"
        | "java/lang/Character"
        | "java/lang/Byte"
        | "java/lang/Short"
        | "java/lang/Void" => {
            let mut fields = vec![ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: cratonvm_types::intern_arc("TYPE"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/Class;"),
                attributes: vec![],
            }];
            fields.extend(instance_fields(1));
            fields
        }
        "java/util/Collections$SynchronizedObject" => vec![ClassFileField {
            access_flags: FieldAccessFlags::PRIVATE,
            name: cratonvm_types::intern_arc("mutex"),
            descriptor: cratonvm_types::intern_arc("Ljava/lang/Object;"),
            attributes: vec![],
        }],
        "java/util/Collections$SynchronizedCollection" => vec![ClassFileField {
            access_flags: FieldAccessFlags::PRIVATE,
            name: cratonvm_types::intern_arc("c"),
            descriptor: cratonvm_types::intern_arc("Ljava/util/Collection;"),
            attributes: vec![],
        }],
        "java/util/Collections$SynchronizedSet" => vec![],
        "java/util/Collections$SynchronizedMap" => vec![ClassFileField {
            access_flags: FieldAccessFlags::PRIVATE,
            name: cratonvm_types::intern_arc("m"),
            descriptor: cratonvm_types::intern_arc("Ljava/util/Map;"),
            attributes: vec![],
        }],
        "java/util/Collections$SingletonList" | "java/util/Collections$SingletonSet" => {
            vec![ClassFileField {
                access_flags: FieldAccessFlags::PRIVATE,
                name: cratonvm_types::intern_arc("element"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/Object;"),
                attributes: vec![],
            }]
        }
        // `k`/`v` sit at 2 and 3, not 0 and 1: the real class extends
        // `AbstractMap`, which declares `keySet` and `values` ahead of them.
        // The model named them at 0/1 until 2026-08-05, so both sat on the
        // inherited view fields — reference over reference, invisible to any
        // value-tag check, reported by the L4 shadow-layout diff. The only
        // writer (`native_collections_singleton_map`) goes by name, so this is
        // a model-only correction.
        "java/util/Collections$SingletonMap" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("_f0"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/Object;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("_f1"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/Object;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PRIVATE,
                name: cratonvm_types::intern_arc("k"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/Object;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PRIVATE,
                name: cratonvm_types::intern_arc("v"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/Object;"),
                attributes: vec![],
            },
        ],
        "java/util/Collections$EmptyList"
        | "java/util/Collections$EmptySet"
        | "java/util/Collections$EmptyMap" => vec![],
        "java/util/Collections$EmptyIterator" => vec![ClassFileField {
            access_flags: FieldAccessFlags::PUBLIC
                | FieldAccessFlags::STATIC
                | FieldAccessFlags::FINAL,
            name: cratonvm_types::intern_arc("EMPTY_ITERATOR"),
            descriptor: cratonvm_types::intern_arc("Ljava/util/Collections$EmptyIterator;"),
            attributes: vec![],
        }],
        "java/util/Collections$EmptyListIterator" => vec![ClassFileField {
            access_flags: FieldAccessFlags::PUBLIC
                | FieldAccessFlags::STATIC
                | FieldAccessFlags::FINAL,
            name: cratonvm_types::intern_arc("EMPTY_ITERATOR"),
            descriptor: cratonvm_types::intern_arc("Ljava/util/Collections$EmptyListIterator;"),
            attributes: vec![],
        }],
        "java/util/Collections$EmptyEnumeration" => vec![],
        "java/util/ArrayList$Itr" | "java/util/ArrayList$ListItr" => instance_fields(5),
        // The real nested class declares `root`/`parent`/`offset`/`size` on top
        // of `AbstractList.modCount`. Fabricated at that width so
        // `asl_base_checked`'s "`class_num_total_fields + ASL_NUM_FIELDS`"
        // comparison means the same thing in synthetic-JDK mode as it does
        // against the real class file.
        "java/util/ArrayList$SubList" => instance_fields(5),
        // `Arrays.asList(T...)`'s fixed-size view. The real nested class has
        // exactly one field, `private final E[] a`, and derives `size()` from
        // `a.length` — which is what `native_arrays_as_list` and the
        // `Arrays$ArrayList` accessors in `native-collections` are written
        // against (`arrays_array_list_backing` looks the slot up BY NAME).
        // Without this arm the fabricated stub had zero slots, so the array
        // could not be stored at all: `size()` answered 0 for every list and
        // `Arrays.asList` was pinned as a synthetic-JDK gap.
        "java/util/Arrays$ArrayList" => vec![named_field("a", "[Ljava/lang/Object;")],
        // ---- sun.reflect.generics reified types ----
        //
        // The WP2.8 reifier allocates these by name and populates them with
        // `set_field_by_name`, and every accessor native in
        // `native-builtins/src/lang_reflect.rs` reads them back BY NAME. With
        // no arm here the fabricated stubs had zero declared slots, so
        // `class_num_total_fields` was 0, the `.max(2)`/`.max(3)` floors gave
        // the objects anonymous `_fN` slots instead, and every
        // `set_field_by_name` write was silently discarded: `getRawType()`
        // answered null and `getActualTypeArguments()` answered null on an
        // object whose raw type and arguments had just been computed
        // correctly.
        //
        // Declaration ORDER matters and is the JDK's own — see the
        // `pti_real` note in `lang_reflect.rs`, which records the real
        // `ParameterizedTypeImpl` layout as `actualTypeArguments[0],
        // rawType[1], ownerType[2]`. The `java/lang/reflect/ParameterizedType`
        // interface natives registered beside it read slots 0/1/2 POSITIONALLY
        // for the bare-interface synthetic (rawType at 0, arguments at 1,
        // owner at 2), which is a different object and a different order; do
        // not "unify" the two.
        "sun/reflect/generics/reflectiveObjects/ParameterizedTypeImpl" => vec![
            named_field("actualTypeArguments", "[Ljava/lang/reflect/Type;"),
            named_field("rawType", "Ljava/lang/Class;"),
            named_field("ownerType", "Ljava/lang/reflect/Type;"),
        ],
        "sun/reflect/generics/reflectiveObjects/WildcardTypeImpl" => vec![
            named_field("upperBounds", "[Ljava/lang/reflect/Type;"),
            named_field("lowerBounds", "[Ljava/lang/reflect/Type;"),
        ],
        "sun/reflect/generics/reflectiveObjects/TypeVariableImpl" => vec![
            named_field(
                "genericDeclaration",
                "Ljava/lang/reflect/GenericDeclaration;",
            ),
            named_field("name", "Ljava/lang/String;"),
            named_field("bounds", "[Ljava/lang/Object;"),
        ],
        "sun/reflect/generics/reflectiveObjects/GenericArrayTypeImpl" => {
            vec![named_field(
                "genericComponentType",
                "Ljava/lang/reflect/Type;",
            )]
        }
        // Collections: ArrayList/Vector/Stack/CopyOnWriteArrayList = 2 fields
        // (data, size) -- `AL_FIELD_DATA`, `AL_FIELD_SIZE`, `AL_NUM_FIELDS = 2`
        // in `native-collections`. This said 2 and reserved 4 until 2026-09-11;
        // see the HashMap arm below for why that mattered and what the extra
        // slots cost.
        "java/util/ArrayList"
        | "java/util/Vector"
        | "java/util/Stack"
        | "java/util/concurrent/CopyOnWriteArrayList" => instance_fields(2),
        // HashMap/HashSet/ConcurrentHashMap = 3 fields (buckets, size, capacity)
        // -- `MAP_FIELD_BUCKETS/SIZE/CAPACITY`, `HS_FIELD_MAP`,
        // `CHM_FIELD_SEGMENTS` and its mask, all in `native-collections`. No
        // native indexes a slot above 2 on any of these receivers.
        //
        // This said 3 and reserved 16 until 2026-09-11. The floor is applied in
        // REAL-JDK mode too, as `max(declared, floor)`, and
        // `build_compact_layout` fills the padding with 8-byte reference slots
        // -- so an empty `java.util.HashMap` occupied 36 slots and 304 bytes
        // against HotSpot's 48, with `table` already null and nothing allocated
        // on the side to explain it. A `HashSet` cost 576. Bringing the two
        // drifted floors down to what the natives actually index took HashMap to
        // 64 bytes and ArrayList from 96 to 32, and 7% off the Hibernate HQL
        // parse that found it.
        //
        // VALIDATED, because lowering a floor fails SILENTLY (an out-of-range
        // `set_field` is dropped, not raised): 300 Hibernate ORM classes
        // byte-for-byte identical, `regression-suite` 92/92, and
        // `probes/CollectionSlotFloor.java` -- written for this -- passing in
        // real-JDK mode and producing an IDENTICAL failure set to the unchanged
        // build in synthetic-JDK mode, where the stub really is the layout.
        // Raising a floor here is free; lowering one needs that evidence again.
        "java/util/HashMap"
        | "java/util/HashSet"
        | "java/util/EnumMap"
        | "java/util/Hashtable"
        | "java/util/concurrent/ConcurrentHashMap" => instance_fields(3),
        // `ConcurrentHashMap$KeySetView` — the JDK's own two fields, named, so
        // `resolve_field_index_by_class_id` finds the same slots in synthetic
        // mode that it finds against the real class (`CollectionView.map` and
        // `KeySetView.value`).
        "java/util/concurrent/ConcurrentHashMap$KeySetView" => vec![
            named_field("map", "Ljava/util/concurrent/ConcurrentHashMap;"),
            named_field("value", "Ljava/lang/Object;"),
        ],
        // `AbstractMap$SimpleEntry` / `$SimpleImmutableEntry` — the JDK's own
        // two fields, named, in the JDK's own order. This is the layout the
        // natives in `phases_late/collections.rs` already write (key at slot
        // 0, value at slot 1); until it was declared here the class was
        // fabricated with ZERO slots, so `new AbstractMap.SimpleEntry<>(k, v)`
        // allocated a 0-slot object and the native `<init>`'s two `set_field`
        // writes were discarded in silence — `getKey()`/`getValue()` then read
        // null and `toString()` printed `null=null` where HotSpot prints
        // `k=7`. Entries the MAP creates were unaffected and that is what hid
        // it: `alloc_synthetic` passes its own slot count to `alloc_object`,
        // so a map-minted entry is well-sized whatever the class declares.
        // Only the bytecode `new`, which sizes from `num_total_fields`, was
        // reading this.
        //
        // Two, not three: `alloc_live_entry` mints these with a third
        // write-through `sourceMap` slot, which stays native-only and
        // undeclared exactly like `Proxy$Instance`'s slots 1 and 2 — a class
        // may be allocated with MORE slots than it declares, and the JDK
        // declares two.
        "java/util/AbstractMap$SimpleEntry" | "java/util/AbstractMap$SimpleImmutableEntry" => {
            vec![
                named_field("key", "Ljava/lang/Object;"),
                named_field("value", "Ljava/lang/Object;"),
            ]
        }
        // `java.util.KeyValueHolder` — the JDK's own two `final` fields, in the
        // JDK's own order, which is the layout `Map.entry`'s native writes.
        // Declared for the same reason the two entries above are: a bytecode
        // `new` sizes its object from `num_total_fields`, so an undeclared
        // layout allocates a 0-slot object and every `set_field` is dropped by
        // the heap's bounds guard, in silence apart from a WARN.
        "java/util/KeyValueHolder" => vec![
            named_field("key", "Ljava/lang/Object;"),
            named_field("value", "Ljava/lang/Object;"),
        ],
        // LinkedList = 3 fields (head, tail, size)
        "java/util/LinkedList" => instance_fields(3),
        // LinkedHashMap's OWN two: `head` and `tail`.
        //
        // The count here is a class's own fields, appended AFTER the parent's
        // -- `stub_total` is `stub_parent_fields + stub_instance_count`. The
        // synthetic indices `native-collections` writes are absolute:
        // `LHM_FIELD_BUCKETS/SIZE/CAPACITY = 0/1/2` are HashMap's three, and
        // LinkedHashMap adds `LHM_FIELD_HEAD = 3` and `LHM_FIELD_TAIL = 4`.
        // That is an absolute EXTENT of 5, which is what the old comment
        // ("= 5 fields") said -- but it was coded as 5 OWN fields on top of
        // HashMap's 3, for an extent of 8. Three slots past anything indexed,
        // in both modes.
        //
        // Comment and code disagreeing in the same direction is the signature
        // this page already found twice (`HashMap` "= 3" coded as 16,
        // `ArrayList` "= 2" coded as 4); this is the third and last of them.
        //
        // In real-JDK mode the arithmetic is what makes it expensive rather
        // than merely untidy: the parent term is the REAL `HashMap`'s eight
        // fields, so the floor came to 13 against a real `LinkedHashMap`'s
        // twelve. Padded by ONE slot -- and a padded class is refused the
        // compact layout entirely, so all thirteen became 16-byte tagged
        // cells: 224 B measured, against HotSpot's 65.1 for the same twelve
        // fields. At 2 the floor is 10, the real class is not padded, and the
        // synthetic indices 3 and 4 are exactly where they always were.
        "java/util/LinkedHashMap" => instance_fields(2),
        // TreeMap = 3 fields (data, size, comparator).
        //
        // A real `java/util/TreeMap` declares nine, so this floor never pads
        // it; it sizes the STUB, which in synthetic-jdk mode is the layout.
        "java/util/TreeMap" => instance_fields(3),
        // `TreeSet` was in that arm, and a real one declares exactly ONE
        // instance field (`m`), so the three padded it -- over the
        // `build_compact_layout` cliff, into 16-byte tagged cells.
        //
        // It is not padded for slots anyone indexes. `TS_FIELD_DATA/SIZE/
        // COMPARATOR` name an address-keyed SIDE TABLE, not object storage:
        // `ts_get_slot` / `ts_set_slot` say so ("The object's own fields are
        // never touched") and contain no `set_field` at all. One slot is kept
        // rather than zero so the `HS_FIELD_MAP = 0` shape -- `m`, the backing
        // map, at absolute 0 -- still has somewhere to live in synthetic mode.
        "java/util/TreeSet" => instance_fields(1),
        // ArrayDeque = 3 fields (elements, head, tail) — EXACTLY what the real
        // `java.util.ArrayDeque` declares, in the same order, so the class is
        // never padded and keeps its compact layout.
        //
        // It was four. The fourth was `size`, and `ad_state` stopped reading it
        // on 2026-08-30: the count is DERIVED from `head`/`tail` the way the
        // JDK's own `size()` derives it (`sub(tail, head, elements.length)`),
        // because any real body that moves either index — `delete`,
        // `DeqIterator.remove`, and every mutator this VM does not register —
        // cannot know a fourth slot exists and desynced it. Nothing has
        // written slot 3 since; the only reader left was a `>` bound in
        // `collect_collection_elements`, which now asks for the three that
        // exist.
        //
        // Narrowing it here is what takes the class off the padded list: a
        // floor of four against a real three cost `ArrayDeque` the compact
        // layout for ALL of its slots (`build_compact_layout` refuses any
        // padded class) and 232 bytes empty against HotSpot's 112.
        "java/util/ArrayDeque" => instance_fields(3),
        // EnumSet = 2 fields (elements backing, enum type)
        "java/util/EnumSet" => instance_fields(2),
        // PriorityQueue = 3 fields (data, size, comparator)
        "java/util/PriorityQueue" => instance_fields(3),
        // StringJoiner = 5 fields
        "java/util/StringJoiner" => instance_fields(5),

        "java/util/regex/Pattern" => instance_fields(2),
        "java/util/regex/Matcher" => instance_fields(6),
        // Scanner = 5 slots, of which the first TWO are named and the rest
        // deliberately are not.
        //
        // `native-io`'s Scanner natives resolve every field by NAME on the
        // receiver, falling back to these indices only for a receiver that does
        // not declare the name — so what the model has to get right is the
        // fallback, and what the shadow-layout diff can check is any slot where
        // the model claims to correspond to the image's slot of the same index.
        //
        // Against `javap -p --module java.base java.util.Scanner` (Temurin
        // 25.0.3) only the first two line up: real slot 0 is `buf` and real
        // slot 1 is `position`, which is where our model puts them. From slot 2
        // on the orders diverge — the image has `matcher`, `delimPattern`,
        // `hasNextPattern` where the model means delimiter, radix, closed, and
        // the image does not reach `closed` and `radix` until slots 14 and 15.
        // Naming those three in OUR order was tried and measured: the diff
        // compares model slot i against image slot i, so it turned one
        // anonymous-model artifact into three index-wise disagreements that are
        // not defects — the natives resolve those three by name and never touch
        // the model index on a real layout. An anonymous slot asserts nothing,
        // which is the correct thing for a model that does not claim to match.
        //
        // Naming `position` is not cosmetic. Every `_fN` is
        // `Ljava/lang/Object;`, so on a FABRICATED Scanner `set_field` coerced
        // the `Int` position to null on the way in and `scan_pos` read back 0
        // forever — `next()` would return the first token and never advance.
        // Declaring it `I` keeps the value. That is the same defect, and the
        // same fix, as the `StringReader` position documented in
        // `native-io/src/lib.rs`.
        "java/util/Scanner" => pad_to(
            vec![
                named_field("buf", "Ljava/nio/CharBuffer;"),
                named_field("position", "I"),
            ],
            5,
        ),
        // `Optional` = 1 field (`value`) on JDK 25, and every native that serves
        // it -- `OPT_FIELD_VALUE = 0` / `OPT_NUM_FIELDS = 1` in
        // native-collections, the `jdk25_concurrency` and HTTP/classfile
        // factories, all of which ask for 1 -- touches slot 0 only. It used to
        // share the primitive Optionals' two slots, which floored the REAL
        // `Optional` to 2 against its 1 (wave 17, lane fieldsite3).
        "java/util/Optional" => instance_fields(1),
        // The primitive Optionals really have two (`isPresent`, `value`), and
        // `OPT_PRIM_*` index both.
        "java/util/OptionalInt" | "java/util/OptionalLong" | "java/util/OptionalDouble" => {
            instance_fields(2)
        }
        // Synthetic math fallbacks mirror the native-builtins layouts.
        "java/math/BigInteger" => instance_fields(2),
        "java/math/BigDecimal" => instance_fields(3),
        // T2.3.8 — Spliterator and its primitive specializations:
        //   field 0 = backing array (Object[], int[], long[], double[])
        //   field 1 = cursor Int (next element to emit)
        "java/util/Spliterator"
        | "java/util/Spliterator$OfInt"
        | "java/util/Spliterator$OfLong"
        | "java/util/Spliterator$OfDouble" => instance_fields(3),
        // ---- java.io field layouts ----
        "java/io/FileInputStream" | "java/io/FileOutputStream" => {
            pad_to(vec![named_field("fd", "Ljava/io/FileDescriptor;")], 4)
        }
        "java/io/FilterInputStream" => vec![named_field("in", "Ljava/io/InputStream;")],
        "java/io/FilterOutputStream" => vec![named_field("out", "Ljava/io/OutputStream;")],
        // ── the java.io Reader/Writer chain, and why slot 0 is spelled two
        //    different ways in the two builds ────────────────────────────────
        //
        // A real `InputStreamReader` declares ONE field, `sd`, and inherits
        // `lock` and `skipBuffer` from `java.io.Reader`; a real
        // `OutputStreamWriter` declares `se` under `writeBuffer` and `lock`;
        // `BufferedReader` and `BufferedWriter` put `in`/`out` at index 2 for
        // the same reason. Naming any of them at 0 puts it on `Reader.lock` or
        // `Writer.writeBuffer`, which is what these four models used to do.
        //
        // Slot 0 — and, for `InputStreamReader`, slot 1 — is ALSO where the
        // synthetic Reader/Writer natives park an fd or a wrapped stream. That
        // is kind 3: a VM-internal value with no real JDK field to live in.
        // Every one of those writers is `#[cfg(feature = "synthetic-jdk")]`,
        // the build in which these classes are ALWAYS fabricated stubs and slot
        // 0 belongs to nobody else. So the honest model differs by build, and
        // it is split here rather than papered over with one spelling that is
        // wrong in one of them:
        //
        // * under `synthetic-jdk` the slots stay `_vmN`, which is what keeps
        //   the overlay COUNTED — an anonymous `_fN` reads as an innocuous
        //   `pad`, and that is how `Files.newBufferedWriter`'s fd hid inside
        //   `Writer.writeBuffer` for a day;
        // * in the default build there is no writer AND no reader. The last
        //   reader was the `BufferedWriter` natives' fd fast path, and it moved
        //   behind the same gate `bw_delegate_out` already carried (see
        //   `native-builtins`'s `phases_late::bw_synthetic_fd`). So the model
        //   names the real fields, and the census is clean because nothing is
        //   parked there — not because the model stopped saying so.
        //
        // Ungating a registration without moving its slot cannot pass silently
        // as a result: the natives read slot 0 only under the feature, and this
        // table is what `shadow_layout`'s production-model test diffs against
        // the JDK's declaration order.
        #[cfg(not(feature = "synthetic-jdk"))]
        "java/io/InputStreamReader" => vec![
            named_field("lock", "Ljava/lang/Object;"),
            named_field("skipBuffer", "[C"),
            named_field("sd", "Lsun/nio/cs/StreamDecoder;"),
        ],
        #[cfg(feature = "synthetic-jdk")]
        "java/io/InputStreamReader" => vec![
            vm_internal_field(0),
            vm_internal_field(1),
            named_field("sd", "Lsun/nio/cs/StreamDecoder;"),
        ],
        #[cfg(not(feature = "synthetic-jdk"))]
        "java/io/BufferedReader" => vec![
            named_field("lock", "Ljava/lang/Object;"),
            named_field("skipBuffer", "[C"),
            named_field("in", "Ljava/io/Reader;"),
        ],
        #[cfg(feature = "synthetic-jdk")]
        "java/io/BufferedReader" => vec![
            vm_internal_field(0),
            named_field("skipBuffer", "[C"),
            named_field("in", "Ljava/io/Reader;"),
        ],
        #[cfg(not(feature = "synthetic-jdk"))]
        "java/io/OutputStreamWriter" => vec![
            named_field("writeBuffer", "[C"),
            named_field("lock", "Ljava/lang/Object;"),
            named_field("se", "Lsun/nio/cs/StreamEncoder;"),
        ],
        #[cfg(feature = "synthetic-jdk")]
        "java/io/OutputStreamWriter" => vec![
            vm_internal_field(0),
            named_field("lock", "Ljava/lang/Object;"),
            named_field("se", "Lsun/nio/cs/StreamEncoder;"),
        ],
        #[cfg(not(feature = "synthetic-jdk"))]
        "java/io/BufferedWriter" => vec![
            named_field("writeBuffer", "[C"),
            named_field("lock", "Ljava/lang/Object;"),
            named_field("out", "Ljava/io/Writer;"),
        ],
        #[cfg(feature = "synthetic-jdk")]
        "java/io/BufferedWriter" => vec![
            vm_internal_field(0),
            named_field("lock", "Ljava/lang/Object;"),
            named_field("out", "Ljava/io/Writer;"),
        ],
        "java/io/DataInputStream" | "java/io/DataOutputStream" => instance_fields(1),
        "java/io/FileDescriptor" => instance_fields(4),
        // ── PrintStream / PrintWriter ────────────────────────────────────────
        //
        // `_f0` is the fd tag (declared slot 0; ABSOLUTE slot 1 for
        // `PrintStream`, which inherits `FilterOutputStream.out` at absolute
        // 0 — the print natives read the fd through `out`/raw slot 0, so the
        // comment this replaces, "= 1 field (fd)", named the wrong slot).
        //
        // `trouble` is the JDK's own field, not a VM-internal one, so it is
        // spelled with its real name rather than `_vmN`: it is what
        // `checkError()` returns, and `PrintStream`/`PrintWriter`'s
        // `catch (IOException x) { trouble = true; }` bodies are the only
        // things that set it. Without a slot for it, a synthetic-mode
        // `checkError()` has nothing to read and the absorbed failure is
        // unobservable rather than merely unthrown.
        // W7-64-printstream-trouble-and-errormanager.md
        //
        // Its INDEX is not the real image's — the real `java.io.PrintStream`
        // declares `trouble` at absolute 4, behind `out`/`closed`/`closeLock`/
        // `autoFlush`. That divergence is real and `shadow_layout`'s
        // `diff_against_model` is right to report it under
        // `CRATONVM_DBG_OVERLAY`; it is harmless because nothing addresses
        // `trouble` positionally. Every reader and writer goes through
        // `native-api`'s `print_error_state`, which resolves it BY NAME —
        // landing on the real slot in Compatible mode and on this one in
        // synthetic mode, with no `#[cfg]` at the call sites.
        //
        // `closing` is `PrintStream`'s and only `PrintStream`'s: the real
        // class declares `private boolean closing` ("to avoid recursive
        // closing") and `java.io.PrintWriter` declares no such field — it uses
        // `out == null` as its closed marker instead. So the one arm the two
        // classes used to share is split here rather than growing a field one
        // of them does not have: `print_error_state::is_closing` would then
        // answer for a `PrintWriter` too, while `native_printwriter_close`
        // deliberately latches nothing. `closing` carries the JDK's own name
        // for the same reason `trouble` does, and is likewise resolved BY NAME
        // at every reader and writer, so its index here diverging from the
        // real image's is a true report for `diff_against_model` to make and
        // harmless in fact.
        // W7-70-printstream-close-noop.md
        "java/io/PrintStream" => {
            let mut fields = instance_fields(1);
            fields.push(named_field("trouble", "Z"));
            fields.push(named_field("closing", "Z"));
            fields
        }
        "java/io/PrintWriter" => {
            let mut fields = instance_fields(1);
            fields.push(named_field("trouble", "Z"));
            fields
        }
        // T1.10 — corrected StringReader/StringWriter shapes to match
        // the real native init code in `native-io/src/lib.rs`:
        //   StringReader = 3 fields (content, pos, length) per
        //   `SR_FIELD_CONTENT/POS/LENGTH` constants.
        //   StringWriter = 2 fields (buffer, count) per
        //   `SW_FIELD_BUF/COUNT` constants.
        // The previous values (2 / 1) were stale and caused
        // `gen_heap::set_field` bounds-check panics during
        // `native_sw_init` / `native_sr_init`.
        "java/io/StringReader" => instance_fields(3),
        "java/io/StringWriter" => instance_fields(2),
        // ByteArrayInputStream = 4 (buf, pos, mark, count) — real-JDK layout (Session 83).
        "java/io/ByteArrayInputStream" => instance_fields(4),
        // ByteArrayOutputStream = 2 (data, count)
        "java/io/ByteArrayOutputStream" => instance_fields(2),
        // ObjectInputStream/ObjectOutputStream = 6 synthetic fields
        "java/io/ObjectInputStream" | "java/io/ObjectOutputStream" => instance_fields(6),
        "java/util/jar/Manifest" => {
            let field = |n: &'static str, d: &'static str| ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc(n),
                descriptor: cratonvm_types::intern_arc(d),
                attributes: vec![],
            };
            vec![
                field("mainAttrs", "Ljava/util/jar/Attributes;"),
                field("entries", "Ljava/util/Map;"),
            ]
        }
        "java/util/jar/Attributes" => vec![ClassFileField {
            access_flags: FieldAccessFlags::empty(),
            name: cratonvm_types::intern_arc("map"),
            descriptor: cratonvm_types::intern_arc("Ljava/util/Map;"),
            attributes: vec![],
        }],
        "java/util/jar/Attributes$Name" => {
            let static_name = |n: &'static str| ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: cratonvm_types::intern_arc(n),
                descriptor: cratonvm_types::intern_arc("Ljava/util/jar/Attributes$Name;"),
                attributes: vec![],
            };
            vec![
                ClassFileField {
                    access_flags: FieldAccessFlags::empty(),
                    name: cratonvm_types::intern_arc("name"),
                    descriptor: cratonvm_types::intern_arc("Ljava/lang/String;"),
                    attributes: vec![],
                },
                static_name("MANIFEST_VERSION"),
                static_name("MAIN_CLASS"),
            ]
        }
        // File = 1 instance field (path string) plus standard separator statics.
        "java/io/File" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("path"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/String;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: cratonvm_types::intern_arc("separator"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/String;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: cratonvm_types::intern_arc("separatorChar"),
                descriptor: cratonvm_types::intern_arc("C"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: cratonvm_types::intern_arc("pathSeparator"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/String;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: cratonvm_types::intern_arc("pathSeparatorChar"),
                descriptor: cratonvm_types::intern_arc("C"),
                attributes: vec![],
            },
        ],
        // ---- java.nio field layouts ----
        // Buffer = 4 fields (position, limit, capacity, mark)
        "java/nio/Buffer" => instance_fields(4),
        // JIT round 16 wave 15 (lane nio), FS2-1, extended 2026-11-13 to all
        // typed buffers behind `CRATONVM_HEAP_BUFFER_STUB_NARROW` (default ON):
        // the two heap buffers take no table of their own (a fabricated one is
        // its parent's width, `Buffer` 4 + `ByteBuffer` 6 = 10, or `Buffer` 4 +
        // `CharBuffer` 5 = 9), and `CharBuffer`, `ShortBuffer`, `IntBuffer`,
        // `LongBuffer`, `FloatBuffer`, and `DoubleBuffer` take five rather than
        // six. Every literal buffer slot in the tree is <= 7 and every literal
        // allocation count <= 8 (the wave-13 screen, re-checked: `BB_FIELD_*`,
        // `CB_FIELD_*`, `BUF_FIELD_*`, servlet `BB_*`, `BB_SEGMENT_SLOT` 5,
        // `BB_HEAP_OFFSET_FALLBACK_SLOT` 6, `CB_SYNTHETIC_FIELD_COUNT` 5,
        // `s2_view_buf_fn!` 6, `native-io` `BB_NUM_FIELDS` 8), so 9 and 10
        // still cover every fabricated index. What it buys is the REAL classes'
        // floor: `HeapByteBuffer` 18 -> 12, `HeapCharBuffer` 18 -> 11, and
        // `CharBuffer`/`ShortBuffer`/`IntBuffer`/`LongBuffer`/`FloatBuffer`/
        // `DoubleBuffer` 12 -> 11 on their own, and with
        // `CRATONVM_SYNTHETIC_FLOOR_STUB_EXTENT=1` their real extents (11 and
        // 9), i.e. the compact layout and JIT field rows for the typed buffers
        // and all their `Heap*` / `Direct*` / `ByteBufferAs*` subclasses.
        "java/nio/HeapByteBuffer" | "java/nio/HeapCharBuffer"
            if true =>
        {
            Vec::new()
        }
        "java/nio/CharBuffer"
        | "java/nio/ShortBuffer"
        | "java/nio/IntBuffer"
        | "java/nio/LongBuffer"
        | "java/nio/FloatBuffer"
        | "java/nio/DoubleBuffer"
            if true =>
        {
            instance_fields(5)
        }
        // ByteBuffer/CharBuffer = 5 fields (array, position, limit, capacity, mark)
        "java/nio/ByteBuffer"
        | "java/nio/CharBuffer"
        | "java/nio/HeapByteBuffer"
        | "java/nio/HeapCharBuffer"
        | "java/nio/ShortBuffer"
        | "java/nio/IntBuffer"
        | "java/nio/LongBuffer"
        | "java/nio/FloatBuffer"
        | "java/nio/DoubleBuffer" => instance_fields(6),
        // Charset = 2 fields (name, aliases)
        "java/util/Locale" => {
            let static_locale = |n: &'static str| ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: cratonvm_types::intern_arc(n),
                descriptor: cratonvm_types::intern_arc("Ljava/util/Locale;"),
                attributes: vec![],
            };
            let fields = vec![
                static_locale("ROOT"),
                static_locale("ENGLISH"),
                static_locale("US"),
                static_locale("CANADA"),
            ];
            // Four instance slots, the real JDK 25 `Locale`'s count
            // (`baseLocale`, `localeExtensions`, `hashCodeValue`, `languageTag`).
            // It was 32 ("conservative"), which floored every REAL `Locale` to
            // 32 slots against its 4: no native indexes a `Locale` slot past 0
            // (`locale_populate` writes `baseLocale` by name, the rest is the
            // `synthetic_locale_data` side table), and the one factory that
            // asked for 32 (`locale_bootstrap::get_or_create_default`) now asks
            // for 4. Wave 17, lane fieldsite3.
            pad_to(fields, 4)
        }
        "java/util/Currency" => vec![
            named_field("currencyCode", "Ljava/lang/String;"),
            named_field("defaultFractionDigits", "I"),
            named_field("numericCode", "I"),
        ],
        "java/nio/charset/Charset" => instance_fields(3),
        "java/nio/charset/CharsetDecoder" => {
            let field = |n: &'static str, d: &'static str| ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc(n),
                descriptor: cratonvm_types::intern_arc(d),
                attributes: vec![],
            };
            vec![
                field("charset", "Ljava/nio/charset/Charset;"),
                field("averageCharsPerByte", "F"),
                field("maxCharsPerByte", "F"),
                field("replacement", "Ljava/lang/String;"),
                field(
                    "malformedInputAction",
                    "Ljava/nio/charset/CodingErrorAction;",
                ),
                field(
                    "unmappableCharacterAction",
                    "Ljava/nio/charset/CodingErrorAction;",
                ),
            ]
        }
        "java/nio/charset/CharsetEncoder" => {
            let field = |n: &'static str, d: &'static str| ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc(n),
                descriptor: cratonvm_types::intern_arc(d),
                attributes: vec![],
            };
            vec![
                field("charset", "Ljava/nio/charset/Charset;"),
                field("averageBytesPerChar", "F"),
                field("maxBytesPerChar", "F"),
                field("replacement", "[B"),
                field(
                    "malformedInputAction",
                    "Ljava/nio/charset/CodingErrorAction;",
                ),
                field(
                    "unmappableCharacterAction",
                    "Ljava/nio/charset/CodingErrorAction;",
                ),
            ]
        }
        "java/nio/charset/CodingErrorAction" => {
            let mk = |n: &'static str| ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: cratonvm_types::intern_arc(n),
                descriptor: cratonvm_types::intern_arc("Ljava/nio/charset/CodingErrorAction;"),
                attributes: vec![],
            };
            vec![
                ClassFileField {
                    access_flags: FieldAccessFlags::empty(),
                    name: cratonvm_types::intern_arc("name"),
                    descriptor: cratonvm_types::intern_arc("Ljava/lang/String;"),
                    attributes: vec![],
                },
                mk("IGNORE"),
                mk("REPLACE"),
                mk("REPORT"),
            ]
        }
        // StandardCharsets — 6 public static final Charset fields
        "java/nio/charset/StandardCharsets" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: cratonvm_types::intern_arc("UTF_8"),
                descriptor: cratonvm_types::intern_arc("Ljava/nio/charset/Charset;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: cratonvm_types::intern_arc("US_ASCII"),
                descriptor: cratonvm_types::intern_arc("Ljava/nio/charset/Charset;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: cratonvm_types::intern_arc("ISO_8859_1"),
                descriptor: cratonvm_types::intern_arc("Ljava/nio/charset/Charset;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: cratonvm_types::intern_arc("UTF_16"),
                descriptor: cratonvm_types::intern_arc("Ljava/nio/charset/Charset;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: cratonvm_types::intern_arc("UTF_16BE"),
                descriptor: cratonvm_types::intern_arc("Ljava/nio/charset/Charset;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: cratonvm_types::intern_arc("UTF_16LE"),
                descriptor: cratonvm_types::intern_arc("Ljava/nio/charset/Charset;"),
                attributes: vec![],
            },
        ],
        // Random = 2 fields (seed, has-next-gaussian)
        "java/util/Random" => instance_fields(2),
        // UUID = 2 fields (msb, lsb)
        "java/util/UUID" => instance_fields(2),
        // Date = 2 fields, as on JDK 25 (`fastTime`, `cdate`): every native
        // writes the millis at slot 0, `jdbc.rs` clears `cdate` at slot 1. It
        // was 4, which floored every REAL `Date` (and so every `java.sql`
        // date) to 4 against 2. Wave 17, lane fieldsite3.
        "java/util/Date" => instance_fields(2),
        // `java.sql.Timestamp` adds one field, `nanos`, which `jdbc.rs`
        // `timestamp_nanos_index` finds as the LAST slot. With `Date` at 2 a
        // fabricated `Timestamp` needs its own slot for it, or the last slot
        // would be `cdate`'s; the real class floors to its own 3.
        "java/sql/Timestamp" => vec![named_field("nanos", "I")],
        // Instant = 2 fields (epochSecond, nano). This matches the synthetic
        // java.time bridge in native-builtins/src/util_time.rs.
        "java/time/Instant" => instance_fields(2),
        // Properties = 4 fields
        "java/util/Properties" => instance_fields(16),
        // Formatter = 2 fields (output=0, locale=1)
        "java/util/Formatter" => instance_fields(2),
        // DecimalFormat = 4 fields (pattern=0, groupingUsed=1, maxFracDigits=2, minFracDigits=3)
        "java/text/DecimalFormat" => instance_fields(4),
        // NumberFormat = 4 fields
        "java/text/NumberFormat" => instance_fields(4),
        // MessageFormat = 1 field (pattern=0)
        "java/text/MessageFormat" => instance_fields(1),
        // Thread = 8 fields, and **every NAMED slot sits at its real JDK
        // index**. The anonymous ones carry CratonVM's fabricated conventions
        // (name=0, priority=1, tid=2, target/runnable=3, virtualFlag=5) and
        // must stay anonymous — an `_fN` asserts nothing, which is exactly
        // right for a slot whose meaning differs between the two layouts.
        //
        // Real JDK 21–25 `java.lang.Thread`, instance fields in declaration
        // order: `eetop`(J), `tid`(J), `name`, `interrupted`(Z),
        // `contextClassLoader`, `holder`, `threadLocals`,
        // `inheritableThreadLocals`, … (19 in all).
        //
        // Until 2026-08-05 this arm was `instance_fields(5)` followed by
        // `contextClassLoader`, i.e. it declared that name at index **5**,
        // which is `holder` on every real image; `threadLocals` and
        // `inheritableThreadLocals` at 6 and 7 were right by accident. No
        // accessor was writing the wrong field — they all resolve on the
        // receiver's own class — but it is a live landmine for anything that
        // resolves against the CLASS NAME while the receiver carries the other
        // layout, and this tree already carries a scar from one such instance:
        // see the defensive type check in
        // `native_thread_get_context_class_loader`, whose comment records a
        // mis-slotted read observed as a `String` during Mockito plugin
        // discovery.
        //
        // Slot 5 is deliberately NOT named `holder`.
        // `populate_real_thread_holder` detects the fabricated layout with
        // `get_field_by_name(this, "holder").is_none()` and falls back to its
        // own slot conventions, so declaring `holder` here would silently turn
        // that fallback off. It is the fabricated-only virtual-thread flag
        // instead — `SYNTHETIC_THREAD_VIRTUAL_SLOT` in
        // `native-builtins/src/jdk25_concurrency.rs` moved off index 4 in the
        // same change so it stops sharing a slot with `contextClassLoader`.
        "java/lang/Thread" => {
            let mut fields = instance_fields(4);
            fields.push(ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("contextClassLoader"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/ClassLoader;"),
                attributes: vec![],
            });
            // Slot 5 — anonymous on purpose (see above).
            fields.push(ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("_f5"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/Object;"),
                attributes: vec![],
            });
            fields.push(ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("threadLocals"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/ThreadLocal$ThreadLocalMap;"),
                attributes: vec![],
            });
            fields.push(ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("inheritableThreadLocals"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/ThreadLocal$ThreadLocalMap;"),
                attributes: vec![],
            });
            fields
        }
        "java/lang/Thread$FieldHolder" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("group"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/ThreadGroup;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("task"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/Runnable;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("stackSize"),
                descriptor: cratonvm_types::intern_arc("J"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("priority"),
                descriptor: cratonvm_types::intern_arc("I"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("daemon"),
                descriptor: cratonvm_types::intern_arc("Z"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("threadStatus"),
                descriptor: cratonvm_types::intern_arc("I"),
                attributes: vec![],
            },
        ],
        // DECLARATION ORDER IS THE REAL JDK'S, and must stay that way.
        //
        // It was `name, parent, daemon, maxPriority` until 2026-08-05 — BOTH
        // pairs transposed against the image, which declares
        // `parent, name, maxPriority, daemon` (JDK 21 through 25, `javap -p
        // --module java.base java.lang.ThreadGroup`). Every field here has a
        // real counterpart at a different index, so the model was wrong in the
        // one way no value-tag census can see: a `ThreadGroup` reference over a
        // `String` reference and an `int` over an `int` both type-check. The
        // L4 shadow-layout diff reported all four
        // (`jdk-only-fabricated-object-layouts-FIXED-20260810.md`).
        //
        // The natives in `native-builtins/src/phases_late/concurrent.rs`
        // resolve these by NAME first and only fall back to a hard-coded index,
        // so on a real image they were already correct — the transposition bit
        // wherever a raw index was used instead, and it kept the census
        // reporting four rows that were not defects. The fallback constants
        // there are the real order now too; the two tables must move together.
        "java/lang/ThreadGroup" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("parent"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/ThreadGroup;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("name"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/String;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("maxPriority"),
                descriptor: cratonvm_types::intern_arc("I"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("daemon"),
                descriptor: cratonvm_types::intern_arc("Z"),
                attributes: vec![],
            },
        ],
        // ThreadLocal native semantics live in side tables keyed by identity
        // hash; the object itself carries nothing.
        //
        // The model used to name slot 0 `value:Ljava/lang/Object;`, which a
        // real `java.lang.ThreadLocal` declares as `threadLocalHashCode:I` --
        // its ONE instance field, and the int the real `ThreadLocalMap` hashes
        // with. Naming the real field is what it is: the fabricated stub gets
        // one unused int, and the shadow-layout diff agrees with the image
        // instead of reporting a slot nobody uses.
        "java/lang/ThreadLocal" | "java/lang/InheritableThreadLocal" => {
            vec![named_field("threadLocalHashCode", "I")]
        }
        "java/lang/Thread$State" => pad_to(
            vec![
                ClassFileField {
                    access_flags: FieldAccessFlags::PUBLIC
                        | FieldAccessFlags::STATIC
                        | FieldAccessFlags::FINAL,
                    name: cratonvm_types::intern_arc("NEW"),
                    descriptor: cratonvm_types::intern_arc("Ljava/lang/Thread$State;"),
                    attributes: vec![],
                },
                ClassFileField {
                    access_flags: FieldAccessFlags::PUBLIC
                        | FieldAccessFlags::STATIC
                        | FieldAccessFlags::FINAL,
                    name: cratonvm_types::intern_arc("RUNNABLE"),
                    descriptor: cratonvm_types::intern_arc("Ljava/lang/Thread$State;"),
                    attributes: vec![],
                },
                ClassFileField {
                    access_flags: FieldAccessFlags::PUBLIC
                        | FieldAccessFlags::STATIC
                        | FieldAccessFlags::FINAL,
                    name: cratonvm_types::intern_arc("BLOCKED"),
                    descriptor: cratonvm_types::intern_arc("Ljava/lang/Thread$State;"),
                    attributes: vec![],
                },
                ClassFileField {
                    access_flags: FieldAccessFlags::PUBLIC
                        | FieldAccessFlags::STATIC
                        | FieldAccessFlags::FINAL,
                    name: cratonvm_types::intern_arc("WAITING"),
                    descriptor: cratonvm_types::intern_arc("Ljava/lang/Thread$State;"),
                    attributes: vec![],
                },
                ClassFileField {
                    access_flags: FieldAccessFlags::PUBLIC
                        | FieldAccessFlags::STATIC
                        | FieldAccessFlags::FINAL,
                    name: cratonvm_types::intern_arc("TIMED_WAITING"),
                    descriptor: cratonvm_types::intern_arc("Ljava/lang/Thread$State;"),
                    attributes: vec![],
                },
                ClassFileField {
                    access_flags: FieldAccessFlags::PUBLIC
                        | FieldAccessFlags::STATIC
                        | FieldAccessFlags::FINAL,
                    name: cratonvm_types::intern_arc("TERMINATED"),
                    descriptor: cratonvm_types::intern_arc("Ljava/lang/Thread$State;"),
                    attributes: vec![],
                },
            ],
            2,
        ),
        "java/security/Permission"
        | "java/security/BasicPermission"
        | "java/lang/RuntimePermission"
        | "java/util/PropertyPermission"
        | "java/util/logging/LoggingPermission" => pad_to(
            vec![ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("name"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/String;"),
                attributes: vec![],
            }],
            2,
        ),

        // Atomic types: 1 field at slot 0, and it is `value` — the name and
        // descriptor the real classes declare, not an `_f0` placeholder.
        //
        // The slot INDEX is unchanged, so every native that addresses it
        // positionally is unaffected; what changes is that a by-NAME lookup
        // now resolves. `findVarHandle(AtomicLong.class, "value", long.class)`
        // used to be answered by a by-name fallback that laundered a failed
        // resolution (removed in a01ccc442), and once that laundering was gone
        // the honest answer here was `NoSuchFieldException` — because the stub
        // declared no field by that name to find.
        //
        // `AtomicStampedReference` / `AtomicMarkableReference` keep the
        // placeholder: their real single field is `pair`, a reference to a
        // private `Pair` record this VM does not model, so naming the slot
        // `value` would be a fabrication rather than a correction.
        "java/util/concurrent/atomic/AtomicInteger" => vec![named_field("value", "I")],
        "java/util/concurrent/atomic/AtomicLong" => vec![named_field("value", "J")],
        // Real `AtomicBoolean.value` is an `int`, not a `boolean`.
        "java/util/concurrent/atomic/AtomicBoolean" => vec![named_field("value", "I")],
        "java/util/concurrent/atomic/AtomicReference" => {
            vec![named_field("value", "Ljava/lang/Object;")]
        }
        "java/util/concurrent/atomic/AtomicStampedReference"
        | "java/util/concurrent/atomic/AtomicMarkableReference" => instance_fields(1),
        // Atomic int/long arrays: 1 field (`array`), as on JDK 25. j27 wave 4
        // (lane TRACEROWS, FS3-2): the synthetic-JDK `register_phase54_atomics`
        // `<init>(I)V` / `length()` / `toString()` no longer keep a length at
        // slot 1 (they read the backing array's), so the real classes are no
        // longer floored to 2.
        "java/util/concurrent/atomic/AtomicIntegerArray"
        | "java/util/concurrent/atomic/AtomicLongArray" => instance_fields(1),
        // `AtomicReferenceArray` = 1 field (`array`), as on JDK 25: its natives
        // (`register_atomic_reference_array_natives`) resolve `array` by name
        // in both modes and never touch slot 1. Wave 17, lane fieldsite3.
        "java/util/concurrent/atomic/AtomicReferenceArray" => instance_fields(1),
        // ReentrantLock = 1 field (`sync`), as on JDK 25. The `native_rl_*`
        // natives keep the lock state in `rl_state_table` and read or write no
        // slot at all (the old owner/holdCount/fair slot writes were removed),
        // so the three-slot table only floored the REAL lock to 3 against 1.
        // Wave 17, lane fieldsite3.
        "java/util/concurrent/locks/ReentrantLock" => instance_fields(1),
        // Condition: 1 field (lock=0)
        "java/util/concurrent/locks/Condition" => instance_fields(1),
        // ReentrantReadWriteLock: 3 fields (readers=0, writer=1, fair=2)
        "java/util/concurrent/locks/ReentrantReadWriteLock" => instance_fields(3),
        // ReadLock/WriteLock: 1 field (parent RWL ref=0)
        "java/util/concurrent/locks/ReentrantReadWriteLock$ReadLock"
        | "java/util/concurrent/locks/ReentrantReadWriteLock$WriteLock" => instance_fields(1),
        // CountDownLatch: 1 field (count=0)
        "java/util/concurrent/CountDownLatch" => instance_fields(1),
        // Semaphore: 2 fields (permits=0, fair=1)
        "java/util/concurrent/Semaphore" => instance_fields(2),
        // CyclicBarrier: 3 fields (parties=0, count=1, broken=2)
        "java/util/concurrent/CyclicBarrier" => instance_fields(3),
        // Phaser: 3 fields (parties=0, arrivals=1, phase=2)
        "java/util/concurrent/Phaser" => instance_fields(3),
        // Exchanger: 1 field (slot=0)
        "java/util/concurrent/Exchanger" => instance_fields(1),
        // CompletableFuture: 4 fields (result=0, done=1, source=2, handler=3)
        "java/util/concurrent/CompletableFuture" => instance_fields(4),
        // LinkedBlockingQueue/Deque: 4 fields (head=0, tail=1, size=2, capacity=3)
        "java/util/concurrent/LinkedBlockingQueue" | "java/util/concurrent/LinkedBlockingDeque" => {
            instance_fields(4)
        }
        // ArrayBlockingQueue: 4 fields (same as LBQ)
        "java/util/concurrent/ArrayBlockingQueue" => instance_fields(4),
        // ConcurrentLinkedQueue/Deque: 4 fields (same layout as LBQ)
        "java/util/concurrent/ConcurrentLinkedQueue"
        | "java/util/concurrent/ConcurrentLinkedDeque" => instance_fields(4),
        // PriorityBlockingQueue: 4 fields
        "java/util/concurrent/PriorityBlockingQueue" => instance_fields(4),
        // ForkJoinPool: 1 field (parallelism=0)
        "java/util/concurrent/ForkJoinPool" => instance_fields(1),
        // ForkJoinTask: 2 fields (result=0, done=1)
        "java/util/concurrent/ForkJoinTask"
        | "java/util/concurrent/RecursiveTask"
        | "java/util/concurrent/RecursiveAction" => instance_fields(2),
        // ThreadPoolExecutor: 2 fields (poolSize=0, isShutdown=1)
        "java/util/concurrent/ThreadPoolExecutor" => instance_fields(2),
        // ScheduledThreadPoolExecutor: 2 fields
        "java/util/concurrent/ScheduledThreadPoolExecutor" => instance_fields(2),
        // ScheduledFuture: 2 fields (result=0, done=1)
        "java/util/concurrent/ScheduledFuture" => instance_fields(2),
        // Future: 2 fields (result=0, done=1)
        "java/util/concurrent/Future" => instance_fields(4),
        // StampedLock: 4 fields
        "java/util/concurrent/locks/StampedLock" => instance_fields(4),
        // ConcurrentSkipListMap: 3 fields
        "java/util/concurrent/ConcurrentSkipListMap" => instance_fields(3),
        // CopyOnWriteArraySet: 2 fields (same as COWAL)
        "java/util/concurrent/CopyOnWriteArraySet" => instance_fields(2),
        // TimeUnit: 1 instance field (ordinal) + 7 static fields (enum constants)
        "java/util/concurrent/TimeUnit" => {
            let mut fields = vec![ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("_f0"),
                descriptor: cratonvm_types::intern_arc("I"),
                attributes: vec![],
            }];
            for name in &[
                "NANOSECONDS",
                "MICROSECONDS",
                "MILLISECONDS",
                "SECONDS",
                "MINUTES",
                "HOURS",
                "DAYS",
            ] {
                fields.push(ClassFileField {
                    access_flags: FieldAccessFlags::PUBLIC | FieldAccessFlags::STATIC,
                    name: cratonvm_types::intern_arc(name),
                    descriptor: cratonvm_types::intern_arc("Ljava/util/concurrent/TimeUnit;"),
                    attributes: vec![],
                });
            }
            // `$VALUES`, added 2026-09-02. Its absence was the last row of
            // `apps/probes/SyntheticEnumSurface`: every direct use of TimeUnit
            // worked — `MILLISECONDS.name()`, `toNanos`, `values()` — while
            // `EnumSet.allOf(TimeUnit.class)` answered 0 against HotSpot's 7,
            // because `Class.getEnumConstants` reads `$VALUES` and
            // `set_static_field_by_name` is a silent no-op on a static this
            // table never declared. The clinit's publish is the other half.
            fields.push(ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: cratonvm_types::intern_arc("$VALUES"),
                descriptor: cratonvm_types::intern_arc("[Ljava/util/concurrent/TimeUnit;"),
                attributes: vec![],
            });
            fields
        }
        // java.lang.ref: Reference = 2 fields (referent=0, queue=1)
        "java/lang/ref/Reference"
        | "java/lang/ref/WeakReference"
        | "java/lang/ref/SoftReference"
        | "java/lang/ref/PhantomReference"
        | "java/lang/ref/Cleaner" => instance_fields(2),
        // ReferenceQueue = 2 fields (head=0, size=1)
        "java/lang/ref/ReferenceQueue" => instance_fields(2),
        // ScopedValue: 3 fields (value=0, isBound=1, hash=2)
        "java/lang/ScopedValue" => instance_fields(3),
        // ScopedValue$Carrier: 3 fields (svRef=0, valueRef=1, parentRef=2)
        "java/lang/ScopedValue$Carrier" => instance_fields(3),
        // ScopedValue$Snapshot: 2 fields (bindingsCount=0, timestamp=1)
        "java/lang/ScopedValue$Snapshot" => instance_fields(2),
        // StructuredTaskScope: 8 fields
        "java/util/concurrent/StructuredTaskScope"
        | "java/util/concurrent/StructuredTaskScope$ShutdownOnFailure"
        | "java/util/concurrent/StructuredTaskScope$ShutdownOnSuccess" => instance_fields(8),
        // StructuredTaskScope$Subtask: 4 fields (state=0, result=1, exception=2, callable=3)
        "java/util/concurrent/StructuredTaskScope$Subtask" => instance_fields(5),
        // Joiner: 4 fields (policy=0, results=1, exception=2, completed=3)
        "java/util/concurrent/StructuredTaskScope$Joiner" => instance_fields(4),
        // Config: 3 fields (name=0, threadFactory=1, timeoutMs=2)
        "java/util/concurrent/StructuredTaskScope$Config" => instance_fields(3),

        // ---- java.lang.reflect layouts (synthetic-jdk mode) ----
        //
        // Real JDK layout of Method/Field/Constructor uses JDK field names
        // that `create_*_object` in native-builtins/src/lang_class.rs populates
        // via `set_field_by_name`. Previously these stubs had ZERO fields,
        // so `set_field_by_name` silently no-op'd on `clazz`, `name`, etc.,
        // which caused `Method.invoke` to see a null `clazz` and abort with
        // "no declaring class". Declaring the fields here makes name-based
        // resolution find them via `resolve_field_index_in_hierarchy`.
        //
        // The descriptor strings are the real JDK types so bytecode
        // getfield/putfield resolves the correct static types; native code
        // only uses the name, so minor descriptor mismatches wouldn't matter
        // for synthetic-jdk's native-driven paths.
        "java/lang/reflect/AccessibleObject" => vec![
            named_field("override", "Z"),
            named_field("accessCheckCache", "Ljava/lang/Object;"),
        ],
        // `java.lang.reflect.Field` extends `AccessibleObject`, which declares
        // TWO instance fields, not one: `override` and `accessCheckCache`. The
        // model listed only `override`, so every slot from index 1 down was off
        // by one against the image -- `clazz` sat on `accessCheckCache`, `name`
        // on `slot`, and so on. Both halves of each pair are references, or both
        // int-class primitives, often enough that a value-tag check sees
        // nothing; the L4 shadow-layout diff reported slots 1/4/6 as NAME and
        // 2/3/5 as TYPE.
        //
        // Nothing positional had to change with it: `create_field_object` and
        // `read_field_meta` (native-builtins/src/lang_class.rs) address every
        // JDK-layout field BY NAME, and CratonVM's own extra metadata is
        // anchored at `base + FIELD_EXTRA_OFFSET_*` where
        // `base = max(FIELD_NUM_FIELDS_LEGACY_FLOOR, class_num_total_fields)` --
        // past whichever layout is wider. That is the shape every hand-numbered
        // model should be converging on.
        "java/lang/reflect/Field" => vec![
            named_field("override", "Z"),
            named_field("accessCheckCache", "Ljava/lang/Object;"),
            named_field("clazz", "Ljava/lang/Class;"),
            named_field("slot", "I"),
            named_field("name", "Ljava/lang/String;"),
            named_field("type", "Ljava/lang/Class;"),
            named_field("modifiers", "I"),
            named_field("trustedFinal", "Z"),
        ],
        // `Method` and `Constructor` extend `java.lang.reflect.Executable`,
        // which adds `parameterData` and `declaredAnnotations` on top of
        // `AccessibleObject`'s two -- so their own fields start at index 4, not
        // index 1. Same by-name access story as `Field` above.
        //
        // `callerSensitive` is dropped rather than moved: it is the LAST of
        // `Method`'s sixteen fields (index 19 on the real layout), and naming it
        // would require modelling the nine between. The pad keeps the model's
        // width where `create_method_object`'s legacy floor expects it.
        "java/lang/reflect/Method" => pad_to(
            vec![
                named_field("override", "Z"),
                named_field("accessCheckCache", "Ljava/lang/Object;"),
                named_field(
                    "parameterData",
                    "Ljava/lang/reflect/Executable$ParameterData;",
                ),
                named_field("declaredAnnotations", "Ljava/util/Map;"),
                named_field("clazz", "Ljava/lang/Class;"),
                named_field("slot", "I"),
                named_field("name", "Ljava/lang/String;"),
                named_field("returnType", "Ljava/lang/Class;"),
                named_field("parameterTypes", "[Ljava/lang/Class;"),
                named_field("exceptionTypes", "[Ljava/lang/Class;"),
                named_field("modifiers", "I"),
            ],
            15,
        ),
        "java/lang/reflect/Constructor" => vec![
            named_field("override", "Z"),
            named_field("accessCheckCache", "Ljava/lang/Object;"),
            named_field(
                "parameterData",
                "Ljava/lang/reflect/Executable$ParameterData;",
            ),
            named_field("declaredAnnotations", "Ljava/util/Map;"),
            named_field("clazz", "Ljava/lang/Class;"),
            named_field("slot", "I"),
            named_field("parameterTypes", "[Ljava/lang/Class;"),
            named_field("exceptionTypes", "[Ljava/lang/Class;"),
            named_field("modifiers", "I"),
        ],

        // ---- T16.5 / T16.6: NIO async channels + UDP ----
        // Field layouts shared with `native-io::nio_native::register_t16_channel_overrides`
        // and `phases_late::register_p67_async_channels` /
        // `register_datagram_channel`. These ensure `alloc_object` backed by
        // `ensure_class_initialized` reserves enough slots so subsequent
        // `set_field(i, ...)` calls don't drop writes.
        //
        // AsynchronousFileChannel = 3 (path_str=0, open=1, _unused=2)
        "java/nio/channels/AsynchronousFileChannel" => instance_fields(3),
        // AsynchronousSocketChannel = 4 (connected=0, open=1, fd=2, remote=3)
        "java/nio/channels/AsynchronousSocketChannel" => instance_fields(4),
        // AsynchronousServerSocketChannel = 1 (open=0)
        "java/nio/channels/AsynchronousServerSocketChannel" => instance_fields(1),
        // AsynchronousChannelGroup = 1 (state=0)
        "java/nio/channels/AsynchronousChannelGroup" => instance_fields(1),
        // DatagramChannel = 5 (port=0, open=1, connected=2, blocking=3, sock_id=4)
        "java/nio/channels/DatagramChannel" => instance_fields(5),
        // URLConnection = 10 instance fields matching real JDK layout
        "java/net/URLConnection" => pad_to(
            vec![
                named_field("url", "Ljava/net/URL;"),
                named_field("doInput", "Z"),
                named_field("doOutput", "Z"),
                named_field("allowUserInteraction", "Z"),
                named_field("useCaches", "Z"),
                named_field("ifModifiedSince", "J"),
                named_field("connected", "Z"),
                named_field("connectTimeout", "I"),
                named_field("readTimeout", "I"),
                named_field("requests", "Ljava/lang/Object;"),
            ],
            10,
        ),
        // HttpURLConnection = 6 instance fields (inherits 10 from URLConnection -> 16 total across chain)
        "java/net/HttpURLConnection" => pad_to(
            vec![
                named_field("method", "Ljava/lang/String;"),
                named_field("chunkLength", "I"),
                named_field("fixedContentLength", "I"),
                named_field("fixedContentLengthLong", "J"),
                named_field("responseCode", "I"),
                named_field("instanceFollowRedirects", "Z"),
            ],
            6,
        ),
        // MulticastSocket = 5 (port=0, closed=1, timeout=2, fd_id=3, ttl=4)
        "java/net/MulticastSocket" => instance_fields(5),
        // DatagramSocket = 4 (port=0, closed=1, timeout=2, fd_id=3), matching
        // the slot indices `phases_late::net_channels`'s phase-72 set reads.
        //
        // There was NO entry at all, so a synthetic instance fell to the
        // `_ => vec![]` arm and got ZERO slots — the real JDK 17+ class has one
        // field (`delegate`), and the padding path below is driven by this same
        // table, so nothing widened it. Every slot write in that set, including
        // the constructor's fd store, was therefore dropped in silence, which
        // is why `net_phase_e::register_re7_datagram_socket` keeps its own
        // ObjectRef-keyed side table instead (see its note about the layout).
        //
        // Third instance of this exact trap found on 2026-07-28, after
        // `HttpExchange` (8 vs 9) and `HttpServer` (5 vs 6). `set_field` past
        // the end of an object DISCARDS the write rather than erroring, so the
        // natives look implemented and store nothing.
        "java/net/DatagramSocket" => instance_fields(4),
        // DatagramPacket = 5 (buf=0, length=1, address=2, port=3, offset=4) per
        // `net_channels::register_p72_datagram`, and Preferences = 6 (backing
        // map=0, name=1, parent=2, children=3, removed=4, user=5) per
        // `beans_jndi::register_p72_preferences`.
        //
        // Both were ABSENT and fell to the `_ => vec![]` arm below, so a
        // synthetic-mode `new` produced a ZERO-slot object and every raw-slot
        // native for them was inert — the same failure mode wave 2 found with
        // DOMSource/DOMResult. Note the silence: `set_field` on a short object
        // drops the write rather than erroring, so the natives looked
        // implemented while storing nothing.
        "java/net/DatagramPacket" => instance_fields(5),
        // 6, not 5, since `isUserNode()` gained slot 5. A `new` that allocated
        // 5 would drop the write silently — exactly the failure the comment
        // above describes — and every node would then answer `isUserNode()`
        // with the default rather than the tree it was created for.
        "java/util/prefs/Preferences" => instance_fields(6),
        // Same layout, and it is reachable now that
        // `AbstractPreferences(AbstractPreferences, String)` is registered: a
        // user `class X extends AbstractPreferences` is constructed through
        // that constructor, and its slots have to exist before the constructor
        // writes them.
        "java/util/prefs/AbstractPreferences" => instance_fields(6),
        // Wave 3-B (RE.4): InetSocketAddress, HttpServer, HttpExchange,
        // HttpContext, Headers must be pre-sized so that the JVM `new` opcode
        // allocates enough slots for the synthetic-mode field layout used by
        // `native-builtins::net_phase_e`. The real-JDK classes have a much
        // smaller `num_total_fields` (e.g. InetSocketAddress has 1: holder),
        // and `upgrade_synthetic_class` preserves max(synthetic, real) so we
        // get the wider layout once the real bytecode loads.
        //
        // InetSocketAddress = 3 (holder=0 InetSocketAddressHolder, port=1 Int, addr=2 InetAddress).
        // Slot 0 mirrors the real-JDK layout (`private final InetSocketAddressHolder holder`)
        // so bytecode `getfield holder` sees the holder our synthetic helpers populate.
        "java/net/InetSocketAddress" => instance_fields(3),
        // InetSocketAddressHolder = 3 (hostname=0 String, addr=1 InetAddress, port=2 Int).
        // Wave 3-B² fix for the deeper dispatch bug: `InetSocketAddress.getPort()`
        // bytecode reads `this.holder` and invokevirtuals `Holder.getPort()` on it,
        // so the holder's class id MUST be honored — putting a String at slot 0 of
        // the InetSocketAddress would route the sub-invokevirtual to
        // `java/lang/String.getPort()` (NoSuchMethodError).
        "java/net/InetSocketAddress$InetSocketAddressHolder" => instance_fields(3),
        // InetAddress = 2 (hostName=0 String, address=1 String)
        "java/net/InetAddress" | "java/net/Inet4Address" | "java/net/Inet6Address" => {
            instance_fields(2)
        }
        // ProcessBuilder = 4 (command, directory, env, redirect flags);
        // Process = 4 (exit, stdout, stderr, pid / native process id).
        "java/lang/ProcessBuilder" | "java/lang/Process" => instance_fields(4),
        "java/lang/ProcessHandle" | "java/lang/ProcessHandleImpl" => instance_fields(1),
        // HttpServer = 6 (address, started, contexts, server_id, port,
        // executor) per `net_phase_e::HS_*`, where `HS_EXECUTOR = 5`.
        //
        // Two bugs fixed here together, both the silent-drop shape:
        //
        // 1. This said 5 while `HS_EXECUTOR = 5` needs a 6th slot, so
        //    `setExecutor` wrote past the end and the write was DISCARDED
        //    rather than erroring.
        // 2. The impl class was keyed `com/sun/net/httpserver/HttpServerImpl`,
        //    but `net_phase_e::HS_IMPL_CLASS` is
        //    `sun/net/httpserver/HttpServerImpl` — a DIFFERENT package. The
        //    name the allocator actually passes had no entry at all and fell
        //    to the `_ => vec![]` arm; it only survived because
        //    `alloc_concurrent_synthetic` requests an explicit slot count and
        //    falls back to `ensure_synthetic_class`. Both spellings are listed
        //    now so the `new` opcode path is covered too.
        //
        // Keep in lock-step with the `HS_*` constants in `net_phase_e.rs`.
        "com/sun/net/httpserver/HttpServer"
        | "com/sun/net/httpserver/HttpServerImpl"
        | "sun/net/httpserver/HttpServerImpl" => instance_fields(6),
        // HttpExchange = 13 (method, uri, reqHeaders, respHeaders, reqBody,
        // statusCode, owner_socket, response_chunks, principal, localAddress,
        // remoteAddress, httpContext, protocol)
        //
        // This read 8 while `net_phase_e.rs` has used `HEX_NUM_FIELDS = 9`
        // with `HEX_PRINCIPAL = 8`, so the last slot did not exist and every
        // write to it went nowhere — losing the authenticated principal on
        // every synthetic-mode exchange. A too-short object does not fail
        // loudly here; the store is simply dropped, which is why the
        // off-by-one survived. If you add a slot in `net_phase_e.rs`, add it
        // here in the same change.
        //
        // 11: `HEX_LOCAL_ADDR = 9` / `HEX_REMOTE_ADDR = 10` carry the
        // connection endpoints `HttpExchange.getLocalAddress()` /
        // `getRemoteAddress()` return, captured from the accepted `TcpStream`
        // in `net_phase_e::re10_dispatch_pending`.
        //
        // Now 13: `HEX_CONTEXT = 11` / `HEX_PROTOCOL = 12` back
        // `HttpExchange.getHttpContext()` / `getProtocol()`, both `abstract`
        // on the real class and, until this pair, registered in NO mode at
        // all — see
        // `the-httpserver-family-four-defects-20260902-RETIRED-20260920.md` §6.
        "com/sun/net/httpserver/HttpExchange" => instance_fields(13),
        // HttpExchange$ResponseBody = 2 (owner exchange, dummy)
        "com/sun/net/httpserver/HttpExchange$ResponseBody" => instance_fields(2),
        // HttpContext = 2 (path, handler)
        "com/sun/net/httpserver/HttpContext" => instance_fields(2),
        // Headers = 1 (delegate HashMap)
        "com/sun/net/httpserver/Headers" => instance_fields(1),
        // JMX ObjectName synthetic fallback: canonicalName string.
        "javax/management/ObjectName" => instance_fields(1),
        "javax/management/ObjectInstance" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::PRIVATE,
                name: cratonvm_types::intern_arc("name"),
                descriptor: cratonvm_types::intern_arc("Ljavax/management/ObjectName;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PRIVATE,
                name: cratonvm_types::intern_arc("className"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/String;"),
                attributes: vec![],
            },
        ],
        // java.beans event support used during WildFly MSC bootstrap.
        "java/beans/PropertyChangeEvent" => instance_fields(4),
        "java/beans/PropertyChangeSupport" | "java/beans/VetoableChangeSupport" => {
            instance_fields(2)
        }

        // Three JDK enums the synthetic model did not carry, added 2026-09-02.
        //
        // A synthetic JDK enum needs its constants DECLARED as statics here (so
        // `GETSTATIC` resolves) and POPULATED by a native `<clinit>` (so the
        // resolved value is not null) — `native-builtins`'
        // `publish_synthetic_enum_constants` is the second half, and one
        // without the other is worse than neither: a declared-but-unpopulated
        // constant turns `NoSuchFieldError` into a NULL enum, which is exactly
        // the shape two stale tests were asserting when they passed a null
        // TimeUnit and a null Class.
        //
        // Measured absent by `apps/probes/SyntheticEnumSurface`:
        // `DayOfWeek.MONDAY`, `HttpClient$Version.HTTP_1_1` and
        // `HttpClient$Redirect.NEVER` were each `NoSuchFieldError` in
        // `--synthetic-jdk` while `TimeUnit.MILLISECONDS` (which HAS a table
        // here) worked.
        //
        // DECLARATION ORDER IS THE ORDINAL, and it is `javap` order on
        // 25.0.3+9, not alphabetical: `compareTo`, `EnumMap` and `EnumSet` all
        // key on it.
        //
        // `$VALUES` is declared alongside (as it now is for
        // `PosixFilePermission` and `StackWalker$Option` below too) because
        // omitting it makes the `<clinit>`'s `$VALUES` publish a silent
        // no-op: `set_static_field_by_name` resolves a DECLARED static and
        // does nothing otherwise. `EnumSet.allOf` and `Class.getEnumConstants`
        // read `$VALUES`, so without the declaration they answer empty.
        "java/time/DayOfWeek" => enum_constant_fields(
            "Ljava/time/DayOfWeek;",
            &[
                "MONDAY",
                "TUESDAY",
                "WEDNESDAY",
                "THURSDAY",
                "FRIDAY",
                "SATURDAY",
                "SUNDAY",
            ],
        ),
        "java/net/http/HttpClient$Version" => enum_constant_fields(
            "Ljava/net/http/HttpClient$Version;",
            &["HTTP_1_1", "HTTP_2"],
        ),
        "java/net/http/HttpClient$Redirect" => enum_constant_fields(
            "Ljava/net/http/HttpClient$Redirect;",
            &["NEVER", "ALWAYS", "NORMAL"],
        ),

        // Four more (interpreter round i1, wave 10, lane L2): their natives
        // minted a fresh "constant" per answer because there were no statics
        // to publish into (`Month.of(1) != Month.of(1)`). The `<clinit>`s are
        // in `native-builtins` (`phases_early.rs` `register_phase52_time_enums`
        // / `_chrono_unit` / `_rounding_mode`, `phases_late/text_intl.rs`
        // `register_p65_datetime_builder`), each through
        // `publish_minted_enum_constants`, and their name lists must stay
        // identical to these. STATICS ONLY: the instance layouts stay what
        // the natives expect (slot 0 an `int` for the first three, which is
        // why none of them gets the `java/lang/Enum` superclass row --
        // `Month`'s `month_slot0_is_synthetic` witness depends on that;
        // `FormatStyle`'s natives carry `Enum`'s `name` / `ordinal` slots).
        "java/time/Month" => enum_constant_fields(
            "Ljava/time/Month;",
            &[
                "JANUARY",
                "FEBRUARY",
                "MARCH",
                "APRIL",
                "MAY",
                "JUNE",
                "JULY",
                "AUGUST",
                "SEPTEMBER",
                "OCTOBER",
                "NOVEMBER",
                "DECEMBER",
            ],
        ),
        "java/time/temporal/ChronoUnit" => enum_constant_fields(
            "Ljava/time/temporal/ChronoUnit;",
            &[
                "NANOS",
                "MICROS",
                "MILLIS",
                "SECONDS",
                "MINUTES",
                "HOURS",
                "HALF_DAYS",
                "DAYS",
                "WEEKS",
                "MONTHS",
                "YEARS",
                "DECADES",
                "CENTURIES",
                "MILLENNIA",
                "ERAS",
                "FOREVER",
            ],
        ),
        "java/math/RoundingMode" => enum_constant_fields(
            "Ljava/math/RoundingMode;",
            &[
                "UP",
                "DOWN",
                "CEILING",
                "FLOOR",
                "HALF_UP",
                "HALF_DOWN",
                "HALF_EVEN",
                "UNNECESSARY",
            ],
        ),
        "java/time/format/FormatStyle" => enum_constant_fields(
            "Ljava/time/format/FormatStyle;",
            &["FULL", "LONG", "MEDIUM", "SHORT"],
        ),

        // E36-1 §4c / E40-1 N3: `System$Logger$Level`'s seven constants,
        // converted from field-shaped dead rows to a real `<clinit>`
        // (`phases_late.rs`'s `system_logger_level_clinit`). This class
        // declared NO statics here before, which is the reason that
        // conversion was nominated rather than landed: a `<clinit>` alone
        // publishes into the void (`set_static_field_by_name` resolves a
        // DECLARED static and is a silent no-op otherwise).
        //
        // `severity` is the one OWN instance field this class declares
        // (`javap -p 'java.lang.System$Logger$Level'` on JDK 25.0.3+9-LTS:
        // `private final int severity;`, alongside `name`/`ordinal`
        // inherited from `java.lang.Enum`) — object_num_fields is 3, slot 2.
        // `Level.getSeverity()` reads it; the seven values match the oracle
        // (ALL=-2147483648, TRACE=400, DEBUG=500, INFO=800, WARNING=900,
        // ERROR=1000, OFF=2147483647) and `lib.rs::system_logger_severity_of`,
        // which `System.Logger.isLoggable` already compares against.
        "java/lang/System$Logger$Level" => {
            let mut fields = vec![named_field("severity", "I")];
            fields.extend(enum_constant_fields(
                "Ljava/lang/System$Logger$Level;",
                &["ALL", "TRACE", "DEBUG", "INFO", "WARNING", "ERROR", "OFF"],
            ));
            fields
        }

        // Spring Boot 3 JarFileArchive.<clinit> reads PosixFilePermission.OWNER_* statics.
        // When the JDK image is unavailable we fall back to a synthetic stub; declare
        // the enum constants so GETSTATIC resolves, and wire values from a native <clinit>
        // (see `register_posix_file_permission_stub_clinit` in native-builtins).
        "java/nio/file/attribute/PosixFilePermission" => {
            let mk = |n: &'static str| ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: cratonvm_types::intern_arc(n),
                descriptor: cratonvm_types::intern_arc(
                    "Ljava/nio/file/attribute/PosixFilePermission;",
                ),
                attributes: vec![],
            };
            let mut fields = vec![
                mk("OWNER_READ"),
                mk("OWNER_WRITE"),
                mk("OWNER_EXECUTE"),
                mk("GROUP_READ"),
                mk("GROUP_WRITE"),
                mk("GROUP_EXECUTE"),
                mk("OTHERS_READ"),
                mk("OTHERS_WRITE"),
                mk("OTHERS_EXECUTE"),
            ];
            // `$VALUES`, 2026-09-02 — the declaration `posix_publish_constants`
            // nominated and wrote its publish against: "this table declares the
            // nine constants for this stub but NOT `$VALUES` … so the `$VALUES`
            // publish below currently goes nowhere on the synthetic side … the
            // declaration is nominated; the publish is written now so it starts
            // working the moment that lands."
            //
            // It lands here. Nothing else changes: that `<clinit>` already
            // writes both spellings, and `values()` reads the nine statics
            // directly either way. What it fixes is `Class.getEnumConstants`,
            // which reads `$VALUES` and answered null — invisible until
            // `create_synthetic_stub` started setting ACC_ENUM in this same
            // change, because `class_is_declared_enum` refused before it ever
            // looked for the array.
            fields.push(ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: cratonvm_types::intern_arc("$VALUES"),
                descriptor: cratonvm_types::intern_arc(
                    "[Ljava/nio/file/attribute/PosixFilePermission;",
                ),
                attributes: vec![],
            });
            fields
        }

        // `Files.getOwner` on Windows returns one of these. Field NAMES (not
        // just a count) matter: `nio_owner_principal` writes them with
        // `set_field_by_name` so the same code works whether this carrier is
        // synthesized here or loaded from java.base.
        "sun/nio/fs/WindowsUserPrincipals$User" => vec![
            named_field("sidString", "Ljava/lang/String;"),
            named_field("sidType", "I"),
            named_field("accountName", "Ljava/lang/String;"),
        ],

        // ---- T19.5: sun.nio.ch.Net TCP cluster ----
        // Layouts shared with `native-io::net::register_sun_nio_ch_net`.
        // These classes don't have Java-side instance fields of interest to
        // our natives — the `fd` integer that identifies a socket is carried
        // on the FileDescriptor handed in as an arg. We still register them
        // so `ensure_class_initialized` succeeds when a user class does
        // e.g. `ServerSocketChannel.open()` (which calls
        // `new ServerSocketChannelImpl(provider)` internally).
        //
        // sun/nio/ch/Net                     = 0 (static utility class; no instance state)
        // sun/nio/ch/ServerSocketChannelImpl = 4 (fd, localAddress, state, blocking)
        // sun/nio/ch/SocketChannelImpl       = 5 (fd, localAddress, remoteAddress, state, blocking)
        // sun/nio/ch/SelectorImpl            = 5 — T19.7.a layout:
        //   field 0: id (Int, lookup key into nio_selector::selectors())
        //   field 1: registered_map (Object, Java-side Set<SelectionKey>)
        //   field 2: selected_set   (Object, Java-side Set<SelectionKey>)
        //   field 3: keys_set       (Object, Java-side Set<SelectionKey>)
        //   field 4: open_flag      (Int, 1 = open, 0 = closed)
        // sun/nio/ch/SelectionKeyImpl        = 5 — T19.7.a layout:
        //   field 0: selector      (Object, parent Selector)
        //   field 1: channel       (Object, the SelectableChannel)
        //   field 2: interestOps   (Int)
        //   field 3: readyOps      (Int)
        //   field 4: attachment    (Object, user attachment slot)
        // java/nio/channels/ServerSocketChannel = 1 (provider)
        // java/nio/channels/SocketChannel    = 1 (provider)
        "sun/nio/ch/Net" => instance_fields(0),
        "sun/nio/ch/ServerSocketChannelImpl" => instance_fields(4),
        "sun/nio/ch/SocketChannelImpl" => instance_fields(5),
        "sun/nio/ch/SelectorImpl" => instance_fields(5),
        "sun/nio/ch/SelectionKeyImpl" => instance_fields(5),
        "java/nio/channels/ServerSocketChannel" => instance_fields(5),
        "java/nio/channels/SocketChannel" => instance_fields(5),

        // ---- T19.N1: java.security ProtectionDomain / CodeSource ----
        // Minimal-viable field layouts so `Class.getProtectionDomain0` can
        // populate the reflected-protection-domain returned to user code.
        //
        // ProtectionDomain = 4 fields in the REAL DECLARATION ORDER:
        // `codesource, classloader, principals, permissions` (`javap -p
        // --module java.base java.security.ProtectionDomain`; the two trailing
        // booleans `hasAllPerm` / `staticPermissions` are not modelled because
        // no native touches them).
        //
        // **A constructor signature is not a field layout, and that is exactly
        // how this went wrong.** Until 2026-08-05 the arm was ordered
        // `(codesource, permissions, classloader, principals)` and said so:
        // *"Matches the constructor signature ProtectionDomain(CodeSource,
        // PermissionCollection, ClassLoader, Principal[]) that real JDK
        // bytecode targets."* The signature is right and irrelevant — the JDK
        // declares those fields in a different order, so three of the four sat
        // at the wrong index. All four are references, so no value-tag check
        // could see it; the L4 shadow-layout diff reported slots 1, 2 and 3.
        //
        // Nothing was observably broken, because
        // `populate_protection_domain_fields` wrote the raw slots and then the
        // same four by name, in that order. That raw pass is gone, which is
        // what makes this order load-bearing rather than decorative.
        "java/security/ProtectionDomain" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("codesource"),
                descriptor: cratonvm_types::intern_arc("Ljava/security/CodeSource;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("classloader"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/ClassLoader;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("principals"),
                descriptor: cratonvm_types::intern_arc("[Ljava/security/Principal;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("permissions"),
                descriptor: cratonvm_types::intern_arc("Ljava/security/PermissionCollection;"),
                attributes: vec![],
            },
        ],

        "sun/misc/Unsafe" => vec![ClassFileField {
            access_flags: FieldAccessFlags::STATIC | FieldAccessFlags::FINAL,
            name: cratonvm_types::intern_arc("theUnsafe"),
            descriptor: cratonvm_types::intern_arc("Lsun/misc/Unsafe;"),
            attributes: vec![],
        }],
        "jdk/internal/misc/Unsafe" => vec![ClassFileField {
            access_flags: FieldAccessFlags::STATIC | FieldAccessFlags::FINAL,
            name: cratonvm_types::intern_arc("theUnsafe"),
            descriptor: cratonvm_types::intern_arc("Ljdk/internal/misc/Unsafe;"),
            attributes: vec![],
        }],

        "java/security/AccessControlContext" => vec![ClassFileField {
            access_flags: FieldAccessFlags::empty(),
            name: cratonvm_types::intern_arc("context"),
            descriptor: cratonvm_types::intern_arc("[Ljava/security/ProtectionDomain;"),
            attributes: vec![],
        }],

        "java/security/PermissionCollection" | "java/security/Permissions" => {
            vec![
                ClassFileField {
                    access_flags: FieldAccessFlags::empty(),
                    name: cratonvm_types::intern_arc("allPermission"),
                    descriptor: cratonvm_types::intern_arc("Ljava/security/Permission;"),
                    attributes: vec![],
                },
                ClassFileField {
                    access_flags: FieldAccessFlags::empty(),
                    name: cratonvm_types::intern_arc("readOnly"),
                    descriptor: cratonvm_types::intern_arc("Z"),
                    attributes: vec![],
                },
            ]
        }

        // CodeSource = 2 fields (location URL, signer certs array).  The
        // JDK layout has additional internals (`signers`, `codeSigners`) that
        // are computed lazily; a 2-field stub is enough for our
        // reflectively-retrieved PD to expose `getLocation()` + `getCertificates()`.
        // DECLARATION ORDER, like its `ProtectionDomain` sibling: `javap -p
        // --module java.base java.security.CodeSource` gives `location,
        // signers, certs, sp, factory, locationNoFragString`.
        //
        // The model was `(location, certs)` — the CONSTRUCTOR's argument order,
        // `CodeSource(URL, Certificate[])` — so `certs` sat on `signers`. Same
        // mistake as `ProtectionDomain`, in the class right next to it. Slot 0
        // is `location` either way, which is why the dozen raw
        // `get_field(cs, 0)` readers around the tree were all correct and only
        // slot 1 was wrong.
        "java/security/CodeSource" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("location"),
                descriptor: cratonvm_types::intern_arc("Ljava/net/URL;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("signers"),
                descriptor: cratonvm_types::intern_arc("[Ljava/security/CodeSigner;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("certs"),
                descriptor: cratonvm_types::intern_arc("[Ljava/security/cert/Certificate;"),
                attributes: vec![],
            },
        ],

        // ---- T19.2.b: WildFly / JBoss Naming (JNDI) field layouts ----
        //
        // Companion to `native-builtins/src/wildfly_naming.rs`. The native
        // naming-store owns a process-wide `parking_lot::RwLock<HashMap>` that
        // holds the canonical `(jndi_name -> BindingEntry)` map; the Java
        // mirrors reserve minimum slots for bytecode that peeks at
        // `InitialContext.environment` / `Binding.name` / etc. The canonical
        // state lives Rust-side.
        //
        // InitialContext: 2 fields (environment_map, default_init_ctx_handle).
        "javax/naming/InitialContext" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("environment"),
                descriptor: cratonvm_types::intern_arc("Ljava/util/Hashtable;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("defaultInitCtx"),
                descriptor: cratonvm_types::intern_arc("J"),
                attributes: vec![],
            },
        ],
        // Binding: 3 fields (name, className, object) — wraps a single JNDI
        // entry for enumeration-style APIs (listBindings / list).
        "javax/naming/Binding" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("name"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/String;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("className"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/String;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("object"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/Object;"),
                attributes: vec![],
            },
        ],
        // NameNotFoundException / NamingException / InvalidNameException inherit
        // Throwable — reuse the 2-slot (message, cause) shape.
        "javax/naming/NameNotFoundException"
        | "javax/naming/NamingException"
        | "javax/naming/InvalidNameException" => instance_fields(2),

        // ---- T19.H5: AtomicReferenceFieldUpdater / AtomicIntegerFieldUpdater
        // / AtomicLongFieldUpdater synthetic implementation classes. The
        // factory natives in `native-builtins::atomic_updater` allocate
        // these with FU_NUM_SLOTS=5 slots (tclassId, slotIndex, descTag,
        // vclassId, cclassId -- the fifth since JIT round 21).  Reserving the layout here means alloc_object has
        // room and getfield by name (if ever used reflectively) reads
        // the right slot.
        "java/util/concurrent/atomic/AtomicReferenceFieldUpdater$RustJvmImpl"
        | "java/util/concurrent/atomic/AtomicIntegerFieldUpdater$RustJvmImpl"
        | "java/util/concurrent/atomic/AtomicLongFieldUpdater$RustJvmImpl" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("tclassId"),
                descriptor: cratonvm_types::intern_arc("I"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("slotIndex"),
                descriptor: cratonvm_types::intern_arc("I"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("descTag"),
                descriptor: cratonvm_types::intern_arc("I"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("vclassId"),
                descriptor: cratonvm_types::intern_arc("I"),
                attributes: vec![],
            },
            // JIT round 21 wave 1, lane updaters: `FU_SLOT_CCLASS_ID` (the
            // JDK's `cclass`), FU_NUM_SLOTS = 5.
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("cclassId"),
                descriptor: cratonvm_types::intern_arc("I"),
                attributes: vec![],
            },
        ],
        // LogManager singleton: the real JDK 21-25 declaration order (`javap -p --module java.logging`).
        // The old model was a compact four-field guess — `properties`,
        // `loggerRegistry`, `rootLogger`, `ready` at 0..3 — which put
        // `loggerRegistry` on `systemContext`, `rootLogger` on `userContext`,
        // and the `ready` INT on `rootLogger`, a reference field.
        //
        // j27 wave 4 (lane TRACEROWS, FS3-4): the two `_vmN` slots that
        // followed (`loggerRegistry` at 12, `ready` at 13) are gone: written by
        // `logmanager::allocate_log_manager`, read by nothing, they padded
        // every real `LogManager` and its subclasses (jboss-logmanager's) by
        // two. Slot indices in `native-builtins/src/logmanager.rs` must agree
        // with this table.
        "java/util/logging/LogManager" => vec![
            named_field("props", "Ljava/util/Properties;"),
            named_field(
                "systemContext",
                "Ljava/util/logging/LogManager$LoggerContext;",
            ),
            named_field(
                "userContext",
                "Ljava/util/logging/LogManager$LoggerContext;",
            ),
            named_field("rootLogger", "Ljava/util/logging/Logger;"),
            named_field("readPrimordialConfiguration", "Z"),
            named_field("globalHandlersState", "I"),
            named_field(
                "configurationLock",
                "Ljava/util/concurrent/locks/ReentrantLock;",
            ),
            named_field(
                "closeOnResetLoggers",
                "Ljava/util/concurrent/CopyOnWriteArrayList;",
            ),
            named_field("listeners", "Ljava/util/Map;"),
            named_field("initializedCalled", "Z"),
            named_field("initializationDone", "Z"),
            named_field("loggerRefQueue", "Ljava/lang/ref/ReferenceQueue;"),
        ],
        // Real JDK 21-25 declaration order. The old model was `name`, `level`,
        // `parent` at 0..2 — which put the logger's NAME on `config`, its level
        // on `manager`, and its PARENT on `name`. All references, so no
        // value-tag check could ever see it; found by the L4 shadow-layout
        // census running under real workloads.
        //
        // A real `Logger` has no `level` field at all — the effective level
        // lives inside `config` (`Logger$ConfigurationData`) — so that slot is
        // `_vm12`, anchored past the real field count.
        "java/util/logging/Logger" => vec![
            named_field("config", "Ljava/util/logging/Logger$ConfigurationData;"),
            named_field("manager", "Ljava/util/logging/LogManager;"),
            named_field("name", "Ljava/lang/String;"),
            named_field("loggerBundle", "Ljava/util/logging/Logger$LoggerBundle;"),
            named_field("anonymous", "Z"),
            named_field("catalogRef", "Ljava/lang/ref/WeakReference;"),
            named_field("catalogName", "Ljava/lang/String;"),
            named_field("catalogLocale", "Ljava/util/Locale;"),
            named_field("parent", "Ljava/util/logging/Logger;"),
            named_field("kids", "Ljava/util/ArrayList;"),
            named_field("callerModuleRef", "Ljava/lang/ref/WeakReference;"),
            named_field("isSystemLogger", "Z"),
            // VM-internal, anchored past the real layout: LOGGER_FIELD_LEVEL.
            vm_internal_field(12),
        ],
        "java/util/logging/LogRecord" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("level"),
                descriptor: cratonvm_types::intern_arc("Ljava/util/logging/Level;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("message"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/String;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("sourceClassName"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/String;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("sourceMethodName"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/String;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("loggerName"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/String;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("millis"),
                descriptor: cratonvm_types::intern_arc("J"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("parameters"),
                descriptor: cratonvm_types::intern_arc("[Ljava/lang/Object;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("resourceBundle"),
                descriptor: cratonvm_types::intern_arc("Ljava/util/ResourceBundle;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("resourceBundleName"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/String;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("sequenceNumber"),
                descriptor: cratonvm_types::intern_arc("J"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("threadID"),
                descriptor: cratonvm_types::intern_arc("I"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("thrown"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/Throwable;"),
                attributes: vec![],
            },
        ],

        // T19.H2: SharedSecrets JavaLangAccess shim class. `java.lang.System$1`
        // is the JDK's anonymous inner class that implements every
        // JavaLangAccess method. When we allocate our shim via
        // `alloc_concurrent_synthetic`, any slot writes are no-ops today,
        // but the forward-compat layout reserves one reference slot for a
        // future back-pointer to a JLA-interior cache. This entry keeps the
        // synthetic fallback layout stable when the real JDK class file
        // can't be loaded.
        "java/lang/System$1" | "java/lang/System$2" => instance_fields(1),

        // T19.H2: synthetic `ModuleLayer` fallback when the real JDK class
        // can't be resolved during boot (pre-init). Slot 0 = boot flag.
        "java/lang/ModuleLayer" => instance_fields(2),

        // T19.H2: synthetic `Module` fallback. Slots: name/layer/packages/
        // descriptor/loader — see `jboss_jdkspecific.rs` for the layout.
        "java/lang/Module" => instance_fields(5),

        // T19.H2: StackWalker synthetic fallback — options, estimateDepth,
        // extendedOption, retainClassRef, contScope, continuation.
        // `phases_late::p59_sw_walk` already allocates 0-field stubs for
        // walker objects; the 6-field layout here upgrades that path.
        "java/lang/StackWalker" => instance_fields(6),
        "java/lang/StackWalker$StackFrame" => instance_fields(8),

        "java/lang/StackWalker$Option" => {
            // E36-1 N1: `$VALUES` declared alongside the constants, for the
            // same reason `PosixFilePermission`'s block above needed it —
            // `stack_walker.rs`'s `native_option_clinit` already WRITES
            // `$VALUES`/`ENUM$VALUES`, but `set_static_field_by_name` resolves
            // a DECLARED static and is a silent no-op otherwise, so that
            // publish went nowhere without this. `EnumSet.allOf` and
            // `Class.getEnumConstants` read `$VALUES`.
            //
            // All FOUR constants, not three: this stub was missing
            // `DROP_METHOD_INFO` (JDK 22), which shadowed
            // `stack_walker.rs::OPTION_FALLBACK_CONSTANTS`'s complete 4-name
            // list — `option_constant_names` only falls back to it when
            // `declared_fields` for this class is EMPTY, and a 3-field stub
            // makes it non-empty. `JceSecurityManager.<clinit>` on JDK 25
            // builds `Set.of(Option.DROP_METHOD_INFO,
            // Option.RETAIN_CLASS_REFERENCE)` — a null `DROP_METHOD_INFO`
            // there is an NPE inside `ImmutableCollections$Set12.<init>` that
            // takes the whole clinit down (see `jca/cipher.rs`'s
            // `getConfiguredPermission` comment). Order verified against
            // `javap -p java.lang.StackWalker$Option` on JDK 25.0.3+9-LTS:
            // `RETAIN_CLASS_REFERENCE, DROP_METHOD_INFO, SHOW_REFLECT_FRAMES,
            // SHOW_HIDDEN_FRAMES` — declaration order is the ordinal.
            enum_constant_fields(
                "Ljava/lang/StackWalker$Option;",
                &[
                    "RETAIN_CLASS_REFERENCE",
                    "DROP_METHOD_INFO",
                    "SHOW_REFLECT_FRAMES",
                    "SHOW_HIDDEN_FRAMES",
                ],
            )
        }

        // T19.H2: ClassFileDumper synthetic fallback — key, dumpDir,
        // enabled, counter slots used by `register_t19_h2_lookup_clinit_deps`.
        "jdk/internal/util/ClassFileDumper" => instance_fields(4),

        // T19_M1_PLATFORM_MXBEANS — synthetic stubs for JDK 25 JMX
        // OpenType machinery used by `jmx_openmbean::alloc_*`.
        // CompositeType: typeName, description, className, isArray,
        // itemNames, nameToDescription, nameToType, nameToIndex.
        "javax/management/openmbean/CompositeType" => instance_fields(8),
        // SimpleType: className, typeName, description, isArray,
        // primitive (cached identity).
        "javax/management/openmbean/SimpleType" => instance_fields(5),
        // OpenType base: className, typeName, description (may be
        // hit if subclassing path resolves base before subclass).
        "javax/management/openmbean/OpenType" => instance_fields(3),
        // MXBeanMapping: javaType, openType, openClass.
        "com/sun/jmx/mbeanserver/MXBeanMapping" => instance_fields(3),
        // ConvertingMethod: method, returnMapping, paramMappings,
        // paramConversionIsIdentity.
        "com/sun/jmx/mbeanserver/ConvertingMethod" => instance_fields(4),
        // OpenConverter: targetType, openType, openClass,
        // identityConverter.
        "com/sun/jmx/mbeanserver/OpenConverter" => instance_fields(4),
        // MappedMXBeanType: openType, typeName, isBasicType,
        // arrayMapping.
        "com/sun/jmx/mbeanserver/MappedMXBeanType" => instance_fields(4),

        // `java.nio.file.FileSystemException` and the six subclasses CratonVM
        // throws by hand. The Throwable-family fallback below would give these
        // three slots, but `p57_no_such_file` and its five siblings in
        // `native-builtins::phases_late::nio_file` allocate FOUR and then do
        // `set_field_by_name(exc, "file", path)` -- and `file` is not in
        // `synthetic_throwable_slot`'s map (`detailMessage`, `cause`,
        // `suppressedExceptions`), so on a synthesized instance that write has
        // nowhere to go and is dropped.
        //
        // That matters because `FileSystemException.getMessage()` is BUILT from
        // `file`; the factories deliberately leave `detailMessage` null to
        // avoid a doubled "<path>: <path>". So a dropped write is not a missing
        // detail, it is an exception with no message at all.
        //
        // Named rather than padded, and all three of the real class's own
        // fields rather than just the one in use, because the whole point of a
        // named slot is that `set_field_by_name` finds it -- a fourth anonymous
        // `_f3` would satisfy the width gate and still drop the write.
        //
        // Not reachable in the default real-JDK mode, where the genuine
        // `java.nio.file.*` classes load and this table is never consulted:
        // measured on 2026-08-30, CratonVM and Temurin 25 both answer
        // `getMessage() == "definitely-absent-file.txt"`. It is the synthesized
        // path this repairs.
        "java/nio/file/FileSystemException"
        | "java/nio/file/NoSuchFileException"
        | "java/nio/file/AccessDeniedException"
        | "java/nio/file/DirectoryNotEmptyException"
        | "java/nio/file/FileAlreadyExistsException"
        | "java/nio/file/NotDirectoryException"
        | "java/nio/file/NotLinkException" => vec![
            // Indices 0..2 are the throwable slots `synthetic_throwable_slot`
            // maps by name, kept in that exact order. Naming them (rather
            // than the anonymous `_fN` the fallback uses) costs nothing and
            // lets a by-name write resolve directly instead of falling
            // through to that map.
            named_field("detailMessage", "Ljava/lang/String;"),
            named_field("cause", "Ljava/lang/Throwable;"),
            named_field("suppressedExceptions", "Ljava/util/List;"),
            named_field("file", "Ljava/lang/String;"),
            named_field("other", "Ljava/lang/String;"),
            named_field("reason", "Ljava/lang/String;"),
        ],

        // Throwable-family fallback: the three slots every Throwable native in
        // `native-builtins::lang_misc` addresses when a synthetic receiver
        // resolves neither a cached field index nor a field NAME —
        // `synthetic_throwable_slot`'s map, which is
        // `[0] = detailMessage, [1] = cause, [2] = suppressedExceptions`.
        //
        // Without this arm those classes reach `_ => vec![]`, i.e. ZERO instance
        // fields, and every one of those writes is dropped by its own
        // `slot < object_num_fields(this)` guard. A synthetic stub's superclass
        // is a blanket `java/lang/Object` unless special-cased (see
        // `synthetic_superclass`), so a throwable stub does NOT inherit
        // `java.lang.Throwable`'s layout the way the real hierarchy would.
        //
        // Measured: with the constructors registered but this arm absent,
        // `new ParseException("bad", 5).getMessage()` answered null on a VM that
        // had just been handed "bad" — the constructor ran and stored nothing.
        // The classes that already worked (`IllegalStateException`, `IOException`)
        // are the ones with an explicit entry above; ordering matters, so this
        // arm must stay LAST and catch only what nothing else claimed.
        name if name == "java/lang/Throwable"
            || name.ends_with("Exception")
            || name.ends_with("Error") =>
        {
            // ZERO when the chain actually reaches `java/lang/Throwable`, so
            // the three slots are INHERITED rather than re-declared at every
            // level.
            //
            // The comment above is why this arm exists and it is still true —
            // "a synthetic stub's superclass is a blanket `java/lang/Object`
            // unless special-cased" — but the special case is exactly what
            // `jdk_superclass` IS, and for a name it knows, the fabricated
            // parent chain is built from it (`synthesize_stub_class` loads
            // `jdk_superclass(name)` and sums `parent.num_total_fields +
            // own`). So for those names re-declaring three is not insurance,
            // it is DOUBLE COUNTING, and it compounds with depth:
            //
            // ```text
            //                              before   after   model
            //   java/lang/Throwable             2       3       3
            //   java/lang/Exception             4       3       3
            //   java/io/IOException             6       3       3
            //   java/io/FileNotFoundException   8       3       3
            // ```
            //
            // In real-JDK mode the same sum is applied as a FLOOR on top of the
            // real class, whose `Throwable` already declares six — so
            // `IOException` was floored to 10 against a real 6 and
            // `FileNotFoundException` to 12, and `build_compact_layout` refuses
            // any padded class, which put every exception object this VM
            // allocates on the legacy 16-byte-per-slot layout.
            //
            // An application exception (`…/DbException`) has no
            // `jdk_superclass` edge, so its chain does NOT reach `Throwable`,
            // so it still gets the three this arm was written for — which is
            // the case the `ParseException` measurement above pins.
            if jdk_chain_reaches_throwable(name) {
                Vec::new()
            } else {
                instance_fields(3)
            }
        }

        _ => vec![],
    }
}


/// The four constructor descriptors a throwable-family class is ASSUMED to have
/// when this table has not measured it.
///
/// They are the `Throwable` set, and for a class that really does declare all
/// four (`Exception`, `RuntimeException`, `IOException`, …) they are exactly
/// right. The assumption is what [`throwable_ctor_descriptors`] exists to stop
/// applying to classes where it is false.
pub const THROWABLE_DEFAULT_CTORS: &[&str] = &[
    "()V",
    "(Ljava/lang/String;)V",
    "(Ljava/lang/String;Ljava/lang/Throwable;)V",
    "(Ljava/lang/Throwable;)V",
];

/// The constructor descriptors JDK 25 declares for a throwable-family class —
/// measured, not assumed.
///
/// # Why this table exists
///
/// Both the synthetic stub's method table (below) and the native registry
/// (`native-builtins::register_throwable_subclass_natives`) used to declare the
/// same blanket four: `()V`, `(String)V`, `(String,Throwable)V`, `(Throwable)V`.
/// Reflected against JDK 25 across the 62 classes that registrar names, those
/// four are **not a declared constructor 103 times**, and **15 constructors
/// javac actually emits were absent**.
///
/// Both halves of that are bugs, and they are different bugs:
///
/// * A **missing** descriptor is a `NoSuchMethodError` at a call site that
///   compiles fine. The worst is `java.lang.AssertionError`: its `(String)V` is
///   PRIVATE, and both `throw new AssertionError(msg)` and `assert cond : msg`
///   compile to `<init>:(Ljava/lang/Object;)V` — which was neither registered
///   nor declared. It is the single most reachable gap in the census: the error
///   path of anything using `assert`.
/// * A **dead** descriptor is a fabricated constructor that the real class does
///   not have. It can only ever win a race it should lose — shadowing real JDK
///   bytecode in Compatible mode, or, in synthetic-JDK mode, letting code
///   compile against a shape the JDK would have rejected.
///
/// # Declared-and-callable, not merely public
///
/// The rule is "every constructor a `javac`-compiled call site can reference",
/// which is the PUBLIC set plus the PROTECTED ones: a subclass in another
/// package compiles `super(...)` against a protected constructor, and under
/// `--real-jdk` the synthetic stub is the only declaration that call will ever
/// find. So `CompletionException()`, `CompletionException(String)`,
/// `ExecutionException()`, `ExecutionException(String)` and
/// `InvocationTargetException()` — all protected, all reachable — are here.
///
/// PRIVATE constructors are not, and that is the same rule rather than an
/// exception to it: `AssertionError(String)` is private, so nothing outside
/// `AssertionError` itself can name it. Its only caller is
/// `AssertionError(Object)`'s own bytecode via `this(...)`, and that bytecode
/// either never runs (synthetic mode: the stub's `<init>` is native) or
/// resolves against the real class file (Compatible mode). Declaring it would
/// be a dead descriptor.
///
/// Nothing the real class does not declare is in this table.
///
/// # One table, two consumers
///
/// The stub's method table and the registry MUST agree about which constructors
/// exist, or a call resolves against a declaration with no implementation (or
/// the reverse). They are in different crates, so the list lives here — in the
/// crate `native-builtins` already depends on — and both read it.
/// `native-builtins`' `every_declared_throwable_ctor_has_a_native` is the gate
/// that says so out loud: every descriptor named here must have a native body.
///
/// # The default is deliberate
///
/// An unmeasured name falls back to [`THROWABLE_DEFAULT_CTORS`]. The caller's
/// `is_throwable_like` test is a NAME heuristic (`ends_with("Exception")`), so
/// it fires for application classes this table has never seen; those still need
/// the common four. Narrowing a class whose real constructor set we have NOT
/// measured would remove answers with nothing to put in their place. Only the
/// classes measured against a real JDK get an exact answer.
///
/// # Provenance
///
/// Measured 2026-08-13 against JDK 25 by reflecting `getDeclaredConstructors()`
/// over the class names the registrar walks — `probes/ThrowableCtorCensusProbe.java`,
/// cross-checked against `scratchpad/e34/Gen34.java`. Re-run after a JDK bump:
/// a bump then shows up as a diff of this function rather than as drift.
pub fn throwable_ctor_descriptors(name: &str) -> &'static [&'static str] {
    match name {
        // -- the full four, genuinely --
        "java/lang/Throwable"
        | "java/lang/Exception"
        | "java/lang/RuntimeException"
        | "java/lang/Error"
        | "java/lang/SecurityException"
        | "java/lang/ReflectiveOperationException"
        | "java/lang/IllegalArgumentException"
        | "java/lang/IllegalStateException"
        | "java/lang/UnsupportedOperationException"
        | "java/util/NoSuchElementException"
        | "java/io/IOException"
        | "java/util/ConcurrentModificationException"
        | "java/util/concurrent/RejectedExecutionException"
        | "java/lang/InternalError" => THROWABLE_DEFAULT_CTORS,

        // -- the full four as well, but here the no-arg and message forms are
        //    PROTECTED rather than public. A subclass's `super()` / `super(msg)`
        //    is a call site `javac` compiles, so the stub must declare them
        //    (see "Declared-and-callable" above).
        "java/util/concurrent/CompletionException" | "java/util/concurrent/ExecutionException" => {
            THROWABLE_DEFAULT_CTORS
        }

        // -- message-only families: no cause-taking constructor at all --
        "java/lang/NoClassDefFoundError"
        | "java/lang/NoSuchMethodError"
        | "java/lang/NoSuchFieldError"
        | "java/lang/NoSuchMethodException"
        | "java/lang/NoSuchFieldException"
        | "java/lang/CloneNotSupportedException"
        | "java/lang/InstantiationException"
        | "java/lang/IllegalAccessException"
        | "java/lang/reflect/InaccessibleObjectException"
        | "java/lang/InterruptedException"
        | "java/lang/NullPointerException"
        | "java/lang/ArithmeticException"
        | "java/lang/ClassCastException"
        | "java/lang/StackOverflowError"
        | "java/lang/OutOfMemoryError"
        | "java/util/InputMismatchException"
        | "java/io/FileNotFoundException"
        | "java/io/NotSerializableException"
        | "java/io/EOFException"
        | "java/io/UnsupportedEncodingException"
        | "java/net/MalformedURLException"
        | "java/net/UnknownHostException"
        | "java/lang/NumberFormatException"
        | "java/util/concurrent/TimeoutException"
        | "java/util/concurrent/CancellationException"
        | "java/util/concurrent/BrokenBarrierException"
        | "java/lang/NegativeArraySizeException"
        | "java/lang/IncompatibleClassChangeError"
        | "java/lang/IllegalAccessError"
        | "java/lang/VerifyError"
        | "java/lang/AbstractMethodError"
        | "java/lang/UnsatisfiedLinkError" => &["()V", "(Ljava/lang/String;)V"],

        "java/lang/LinkageError" => &[
            "()V",
            "(Ljava/lang/String;)V",
            "(Ljava/lang/String;Ljava/lang/Throwable;)V",
        ],
        "java/lang/ClassNotFoundException" => &[
            "()V",
            "(Ljava/lang/String;)V",
            "(Ljava/lang/String;Ljava/lang/Throwable;)V",
        ],
        "java/lang/ExceptionInInitializerError" => {
            &["()V", "(Ljava/lang/String;)V", "(Ljava/lang/Throwable;)V"]
        }

        // -- cause-only --
        "java/lang/TypeNotPresentException" | "java/lang/MatchException" => {
            &["(Ljava/lang/String;Ljava/lang/Throwable;)V"]
        }
        "java/util/FormatterClosedException" => &["()V"],

        // -- the index families: an `int`/`long` overload nobody registered --
        "java/lang/ArrayIndexOutOfBoundsException"
        | "java/lang/StringIndexOutOfBoundsException" => &["()V", "(Ljava/lang/String;)V", "(I)V"],
        "java/lang/IndexOutOfBoundsException" => &["()V", "(Ljava/lang/String;)V", "(I)V", "(J)V"],

        // -- `AssertionError`: the headline. `(String)V` is PRIVATE and
        //    `(Throwable)V` does not exist; `(Object)V` is what
        //    `assert x : msg` compiles to.
        "java/lang/AssertionError" => &[
            "()V",
            "(Ljava/lang/Object;)V",
            "(Z)V",
            "(C)V",
            "(I)V",
            "(J)V",
            "(F)V",
            "(D)V",
            "(Ljava/lang/String;Ljava/lang/Throwable;)V",
        ],

        // -- classes where ALL FOUR blanket descriptors are dead --
        "java/io/UncheckedIOException" => &[
            "(Ljava/io/IOException;)V",
            "(Ljava/lang/String;Ljava/io/IOException;)V",
        ],
        "java/util/MissingResourceException" => {
            &["(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V"]
        }
        "java/text/ParseException" => &["(Ljava/lang/String;I)V"],
        // `()V` is PROTECTED here and kept for the same reason as
        // `CompletionException`'s. `(String)V` and `(String,Throwable)V` are not
        // declared by this class at all and are gone; the wrapped throwable
        // lives in this class's own `target` field rather than
        // `Throwable.cause`, which is what the two remaining natives implement.
        "java/lang/reflect/InvocationTargetException" => &[
            "()V",
            "(Ljava/lang/Throwable;)V",
            "(Ljava/lang/Throwable;Ljava/lang/String;)V",
        ],

        _ => THROWABLE_DEFAULT_CTORS,
    }
}

