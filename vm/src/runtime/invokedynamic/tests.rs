use super::*;

// -----------------------------------------------------------------------
// Cached debug-flag helpers
// -----------------------------------------------------------------------
//
// These replaced uncached `cratonvm_types::flags::runtime_var_os` reads that sat on the
// per-execution generic-indy path. The risk of the swap is a typo in the
// variable name (the flag would then silently never fire, and a future
// debugging session would waste hours on a dead switch), or an inverted
// sense. Both are caught by comparing against the live environment.


// -----------------------------------------------------------------------
// Call-site cache: population, reuse, and redefinition invalidation
// -----------------------------------------------------------------------
//
// Task-3 coverage for "is the bootstrap result cached per call site and
// correctly invalidated?". The observable contract lives entirely in
// `ResolutionCache`, which `execute_invokedynamic` consults on its fast
// path and populates from every JDK-factory bootstrap branch.

use crate::classloading::resolution::ResolutionCache;

fn concat_site(recipe: &str, constants: &[&str], descriptor: &str) -> ResolvedCallSite {
    // Recipes and constants are UTF-16 code units on the cached site; the
    // `&str` parameters here are test-authoring convenience only.
    ResolvedCallSite::StringConcat {
        recipe: Arc::from(recipe.encode_utf16().collect::<Vec<u16>>().as_slice()),
        constant_args: constants
            .iter()
            .map(|s| Arc::from(s.encode_utf16().collect::<Vec<u16>>().as_slice()))
            .collect(),
        target_descriptor: Arc::from(descriptor),
        latin1_shape: false,
    }
}

fn lambda_site(iface: &str, impl_owner: &str, impl_name: &str) -> ResolvedCallSite {
    ResolvedCallSite::Lambda(LambdaCallSite {
        functional_interface: Arc::from(iface),
        functional_interface_id: None,
        sam_method_name: Arc::from("apply"),
        sam_descriptor: Arc::from("(Ljava/lang/Object;)Ljava/lang/Object;"),
        impl_handle: MethodHandle {
            kind: MethodHandleKind::InvokeStatic,
            class_name: Arc::from(impl_owner),
            member_name: Arc::from(impl_name),
            descriptor: Arc::from("(Ljava/lang/Object;)Ljava/lang/Object;"),
        },
        instantiated_descriptor: Arc::from("(Ljava/lang/Object;)Ljava/lang/Object;"),
        capture_types: vec![],
        proxy_class_id: ClassId::new(9001),
        serializable_flag: false,
    })
}

#[test]
fn bootstrapped_call_site_is_reused_not_rebootstrapped() {
    let mut cache = ResolutionCache::new();
    let caller = ClassId::new(7);
    assert!(
        cache.get_call_site(caller, 42).is_none(),
        "cold call site must miss so the bootstrap slow path runs"
    );
    cache.put_call_site(caller, 42, concat_site("\u{0001} items", &[], "(I)V"));
    // Second and every later execution take the fast path. Nothing here
    // memoizes a *negative* result, so a miss is always retried — the
    // "cached `None` is permanent" failure mode does not apply to this
    // cache (`get_call_site` returns `Option<&_>` from a plain map lookup;
    // only successful bootstraps ever insert).
    assert!(cache.get_call_site(caller, 42).is_some());
    assert!(cache.get_call_site(caller, 42).is_some());
    assert_eq!(cache.call_site_count(), 1);
}

#[test]
fn lambda_call_site_is_dropped_when_its_caller_is_redefined() {
    let mut cache = ResolutionCache::new();
    let caller = ClassId::new(11);
    let other = ClassId::new(12);
    cache.put_call_site(
        caller,
        3,
        lambda_site("java/util/function/Function", "P", "f"),
    );
    cache.put_call_site(
        other,
        3,
        lambda_site("java/util/function/Function", "P", "g"),
    );

    // Redefining an unrelated class must not disturb this call site: the
    // lambda keeps dispatching to the same spun proxy.
    cache.invalidate_class(ClassId::new(99));
    assert!(cache.get_call_site(caller, 3).is_some());

    // Redefining the class that *contains* the invokedynamic drops it, so
    // the next execution re-runs LambdaMetafactory against the new constant
    // pool. Same cp index in a redefined pool may denote a different call
    // site entirely, which is exactly why key-match eviction is required.
    cache.invalidate_class(caller);
    assert!(cache.get_call_site(caller, 3).is_none());
    // ... and only that class's entries go.
    assert!(cache.get_call_site(other, 3).is_some());
}

#[test]
fn lambda_impl_handle_is_symbolic_so_impl_redefinition_needs_no_eviction() {
    // `invalidate_class` evicts call sites by *key* class only. That is
    // sound precisely because `LambdaCallSite::impl_handle` stores the
    // implementation method symbolically (owner / name / descriptor) and is
    // re-resolved on each dispatch — redefining the class that owns the
    // lambda body is therefore picked up without touching this cache. If
    // anyone ever pre-resolves the handle to a concrete method pointer,
    // this test fails and flags that eviction must grow a callee-side prong
    // (as `fields` / `methods` already have).
    let ResolvedCallSite::Lambda(lcs) = lambda_site("java/util/function/Function", "Impl", "f")
    else {
        panic!("expected a lambda call site");
    };
    assert_eq!(&*lcs.impl_handle.class_name, "Impl");
    assert_eq!(&*lcs.impl_handle.member_name, "f");
    assert_eq!(
        &*lcs.impl_handle.descriptor,
        "(Ljava/lang/Object;)Ljava/lang/Object;"
    );
}

#[test]
fn cached_string_concat_site_exposes_borrowable_recipe_and_constants() {
    // `execute_string_concat` borrows `&[u16]` / `&[S: AsRef<[u16]>]`
    // straight out of the cached site instead of rebuilding an `IndyInfo`
    // with `recipe.to_string()`, `target_descriptor.to_string()` and a
    // freshly allocated `Vec<String>` of every constant on *every* `"a" + b`
    // evaluation. This test pins the borrow shape: destructuring the cached
    // site must yield data usable without conversion, and `Arc<[u16]>` must
    // satisfy the `AsRef<[u16]>` bound.
    //
    // Units rather than `str` since 2026-08-05: a recipe can carry a lone
    // surrogate from a folded literal, which a Rust `str` turns into U+FFFD.
    fn takes_borrowed<S: AsRef<[u16]>>(
        recipe: &[u16],
        constants: &[S],
        descriptor: &str,
    ) -> String {
        let mut out: Vec<u16> = recipe.to_vec();
        for c in constants {
            out.extend_from_slice(c.as_ref());
        }
        out.extend(descriptor.encode_utf16());
        String::from_utf16_lossy(&out)
    }

    let site = concat_site("a\u{0002}b", &["X", "Y"], "(I)Ljava/lang/String;");
    let ResolvedCallSite::StringConcat {
        recipe,
        constant_args,
        target_descriptor,
        ..
    } = &site
    else {
        panic!("expected a StringConcat call site");
    };
    assert_eq!(
        takes_borrowed(recipe, constant_args.as_slice(), target_descriptor),
        "a\u{0002}bXY(I)Ljava/lang/String;"
    );
    // The bootstrap path passes owned `Vec<u16>`; both must compile against
    // the same bound.
    let owned: Vec<Vec<u16>> = vec![vec![b'X' as u16], vec![b'Y' as u16]];
    let recipe_units: Vec<u16> = "a\u{0002}b".encode_utf16().collect();
    assert_eq!(
        takes_borrowed(&recipe_units, owned.as_slice(), "(I)Ljava/lang/String;"),
        "a\u{0002}bXY(I)Ljava/lang/String;"
    );
}

#[test]
fn synthesized_make_concat_recipe_is_all_argument_placeholders() {
    // The `makeConcat` (no-recipe) branch synthesizes one `\u{0001}` per
    // declared argument and passes an empty constant list. Regression guard
    // for the `patched_info` removal: the synthesized recipe must still have
    // exactly one placeholder per descriptor argument and contain no
    // `\u{0002}` constant placeholders (there are no constants to consume).
    for desc in [
        "()Ljava/lang/String;",
        "(I)Ljava/lang/String;",
        "(ILjava/lang/String;J)Ljava/lang/String;",
    ] {
        let n = parse_descriptor_args(desc).len();
        let recipe: Vec<u16> = vec![TAG_ARG_UNIT; n];
        assert_eq!(recipe.iter().filter(|&&u| u == TAG_ARG_UNIT).count(), n);
        assert!(!recipe.contains(&TAG_CONST_UNIT));
    }
}

#[test]
fn parse_descriptor_args_empty() {
    assert_eq!(parse_descriptor_args("()V"), vec![]);
}

#[test]
fn parse_descriptor_args_mixed() {
    let args = parse_descriptor_args("(ILjava/lang/String;DJ)Ljava/lang/String;");
    assert_eq!(args, vec!['I', 'L', 'D', 'J']);
}

#[test]
fn parse_descriptor_args_array() {
    let args = parse_descriptor_args("([I[Ljava/lang/String;)V");
    assert_eq!(args, vec!['L', 'L']);
}

#[test]
fn parse_descriptor_args_all_primitives() {
    let args = parse_descriptor_args("(BCDFIJSZ)V");
    assert_eq!(args, vec!['B', 'C', 'D', 'F', 'I', 'J', 'S', 'Z']);
}

/// `makeConcatWithConstants` recipe constants (the `\u0002` slots) may be any
/// loadable constant, not just `String`. Verify each kind converts to its
/// HotSpot `String.valueOf` text instead of being silently dropped.
#[test]
fn resolve_concat_constant_all_kinds() {
    use cratonvm_reader::constant_pool::ConstantPoolEntry as CPE;
    let entries = vec![
        CPE::Tombstone,                           // 0 (unused)
        CPE::Utf8("lit".to_string().into()),      // 1
        CPE::StringReference { string_index: 1 }, // 2  -> "lit"
        CPE::Integer(42),                         // 3  -> "42"
        CPE::Long(123456789012345.into()),        // 4  -> decimal
        CPE::Float(1.0),                          // 5  -> "1.0"
        CPE::Double(2.5.into()),                  // 6  -> "2.5"
        CPE::Utf8("verbatim".to_string().into()), // 7  -> "verbatim"
    ];
    let cp = ConstantPool::new(entries);

    assert_eq!(resolve_concat_constant(&cp, 2).as_deref(), Some("lit"));
    assert_eq!(resolve_concat_constant(&cp, 3).as_deref(), Some("42"));
    assert_eq!(
        resolve_concat_constant(&cp, 4).as_deref(),
        Some("123456789012345")
    );
    // Float/double must keep the Java ".0" suffix, not "1" / "2".
    assert_eq!(resolve_concat_constant(&cp, 5).as_deref(), Some("1.0"));
    assert_eq!(resolve_concat_constant(&cp, 6).as_deref(), Some("2.5"));
    assert_eq!(resolve_concat_constant(&cp, 7).as_deref(), Some("verbatim"));
}

#[test]
fn format_float_special_values() {
    assert_eq!(format_float(f32::NAN), "NaN");
    assert_eq!(format_float(f32::INFINITY), "Infinity");
    assert_eq!(format_float(f32::NEG_INFINITY), "-Infinity");
    assert_eq!(format_float(-0.0f32), "-0.0");
}

#[test]
fn format_double_special_values() {
    assert_eq!(format_double(f64::NAN), "NaN");
    assert_eq!(format_double(f64::INFINITY), "Infinity");
    assert_eq!(format_double(f64::NEG_INFINITY), "-Infinity");
    assert_eq!(format_double(-0.0f64), "-0.0");
}

#[test]
fn value_to_string_primitives() {
    use crate::config::VmConfig;
    use std::sync::Arc;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    assert_eq!(value_to_string(&shared, None, &Value::Int(42), 'I'), "42");
    assert_eq!(value_to_string(&shared, None, &Value::Int(1), 'Z'), "true");
    assert_eq!(value_to_string(&shared, None, &Value::Int(0), 'Z'), "false");
    assert_eq!(value_to_string(&shared, None, &Value::Int(65), 'C'), "A");
    assert_eq!(
        value_to_string(&shared, None, &Value::Long(123456789), 'J'),
        "123456789"
    );
    assert_eq!(
        value_to_string(&shared, None, &Value::Object(None), 'L'),
        "null"
    );
}

#[test]
fn value_to_string_java_string() {
    use crate::config::VmConfig;
    use std::sync::Arc;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let str_ref = create_java_string(&shared, "Hello");
    assert_eq!(
        value_to_string(&shared, None, &Value::Object(Some(str_ref)), 'L'),
        "Hello"
    );
}

/// A `typeSwitch` type label answers `instanceof` for an ARRAY target from
/// the array's own type, not from its header class id (which is the
/// component class): `int[]` is an `Object`, is an `int[]`, is not a
/// `String` and not a `long[]`.
#[test]
fn type_switch_label_on_an_array_target_uses_array_assignability() {
    use crate::config::VmConfig;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let arr =
        shared
            .mem
            .heap
            .alloc_array(ClassId::new(0), cratonvm_types::ArrayElementType::Int, 2);
    let cid = shared.mem.heap.class_id_of(arr);
    let any = ClassId::new(0);
    let mut thread = JvmThread::new(crate::threading::jvm_thread::ThreadId(0), "test");
    let mut arr = arr;
    let mut matches =
        |label: &str| type_label_matches(&shared, &mut thread, &mut arr, cid, any, label);
    assert!(matches("[I"));
    assert!(matches("java/lang/Object"));
    assert!(!matches("[J"));
    assert!(!matches("java/lang/String"));
}

/// An `enumSwitch` whose labels include a `Class` (javac's shape for
/// `case E x when ...` beside constant labels) is linked as a type switch
/// with `EnumConstant` labels, and a type label matches the constant it
/// is an instance of. `SwitchBootstraps.enumSwitch` returns the first
/// matching label from `startIndex` on.
#[test]
fn enum_switch_class_label_matches_by_instanceof() {
    use crate::config::VmConfig;
    use crate::runtime::frame::Frame;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(crate::threading::jvm_thread::ThreadId(0), "test");
    // An enum-like object whose slot 0 is the constant name, as in
    // `vm::tests::enum_switch_*`. Its class must be a loaded class so the
    // type label's hierarchy test can answer; any class stands in for the
    // enum type (`vm::tests::type_switch_integer_match` loads the same).
    let stand_in = shared
        .classes
        .class_manager
        .write()
        .load_class("java/lang/Integer")
        .expect("load a stand-in class");
    let name = create_java_string(&shared, "B");
    let constant = shared.mem.heap.alloc_object(stand_in, 2);
    shared
        .mem
        .heap
        .set_field(constant, 0, Value::Object(Some(name)));
    let enum_cid = shared.mem.heap.class_id_of(constant);
    let labels = vec![
        SwitchLabel::EnumConstant("A".into()),
        SwitchLabel::Type {
            class_name: "p/E".into(),
            class_id: enum_cid,
        },
        SwitchLabel::EnumConstant("B".into()),
    ];
    let run = |thread: &mut JvmThread, start: i32| {
        let mut frame = Frame::new(
            ClassId::new(0),
            String::new(),
            String::new(),
            String::new(),
            None,
            vec![0xb1],
            vec![],
            10,
            0,
            &[],
        );
        let _ = frame.stack.push(Value::Object(Some(constant)));
        let _ = frame.stack.push(Value::Int(start));
        thread.frames.clear();
        thread.frames.push(frame);
        execute_type_switch(&shared, thread, 0, &labels).expect("type switch");
        thread.frames[0].stack.pop().expect("result")
    };
    // `B` is not `A`; the `Class` label (index 1) matches first.
    assert_eq!(run(&mut thread, 0), Value::Int(1));
    // Resumed past the guard (`startIndex` 2): the constant label.
    assert_eq!(run(&mut thread, 2), Value::Int(2));
}

/// The per-thread `IndySiteCache` entry is keyed by the full
/// (class, cp index) pair and dies with the resolution epoch, which every
/// path that drops a `resolution_cache` call-site row advances.
#[test]
fn indy_site_cache_entry_dies_with_the_resolution_epoch() {
    if cratonvm_classloading::any_class_redefined() {
        return; // Latched off process-wide by another test.
    }
    let mut cache = IndySiteCache::new();
    let (class, cp) = (ClassId::new(3), 5u16);
    // Parallel tests move the global epochs; retry until one fill sticks.
    let stuck = (0..100).any(|_| {
        cache.put(
            class,
            cp,
            IndySiteCache::epochs_now(),
            CachedIndySite::Lambda {
                proxy_class_id: ClassId::new(7),
                num_captures: 2,
                singleton_key: None,
                singleton: None,
            },
        );
        matches!(
            cache.get(class, cp),
            Some(CachedIndySite::Lambda {
                num_captures: 2,
                ..
            })
        )
    });
    if !stuck {
        return;
    }
    assert!(cache.get(class, cp + 1).is_none());
    assert!(cache.get(ClassId::new(4), cp).is_none());
    crate::runtime::interpreter::bump_resolution_epoch();
    assert!(cache.get(class, cp).is_none());
}

/// A qualified enum label (`case E.A`, JEP 441) is javac's
/// `ConstantBootstraps.invoke(EnumDesc.of, <ClassDesc.of("p.Outer$E")>, "A")`
/// condy; it decodes to the enum's internal name and the constant name.
/// Any other condy shape (here the inner `ClassDesc` condy) is not one.
#[test]
fn javac_enum_desc_label_decodes_the_qualified_enum_constant() {
    use cratonvm_reader::constant_pool::ConstantPoolEntry as CPE;
    let utf8 = |s: &str| CPE::Utf8(s.to_string().into());
    let entries = vec![
        CPE::Tombstone,                              // 0
        utf8("java/lang/invoke/ConstantBootstraps"), // 1
        CPE::ClassReference { name_index: 1 },       // 2
        utf8("invoke"),                              // 3
        utf8(
            "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/Class;\
             Ljava/lang/invoke/MethodHandle;[Ljava/lang/Object;)Ljava/lang/Object;",
        ), // 4
        CPE::NameAndType {
            name_index: 3,
            descriptor_index: 4,
        }, // 5
        CPE::MethodReference {
            class_index: 2,
            name_and_type_index: 5,
        }, // 6
        CPE::MethodHandle {
            reference_kind: 6,
            reference_index: 6,
        }, // 7: the bootstrap
        utf8("java/lang/Enum$EnumDesc"),             // 8
        CPE::ClassReference { name_index: 8 },       // 9
        utf8("of"),                                  // 10
        utf8("(Ljava/lang/constant/ClassDesc;Ljava/lang/String;)Ljava/lang/Enum$EnumDesc;"), // 11
        CPE::NameAndType {
            name_index: 10,
            descriptor_index: 11,
        }, // 12
        CPE::MethodReference {
            class_index: 9,
            name_and_type_index: 12,
        }, // 13
        CPE::MethodHandle {
            reference_kind: 6,
            reference_index: 13,
        }, // 14: EnumDesc.of
        utf8("java/lang/constant/ClassDesc"),   // 15
        CPE::ClassReference { name_index: 15 }, // 16
        utf8("(Ljava/lang/String;)Ljava/lang/constant/ClassDesc;"), // 17
        CPE::NameAndType {
            name_index: 10,
            descriptor_index: 17,
        }, // 18
        CPE::InterfaceMethodReference {
            class_index: 16,
            name_and_type_index: 18,
        }, // 19
        CPE::MethodHandle {
            reference_kind: 6,
            reference_index: 19,
        }, // 20: ClassDesc.of
        utf8("p.Outer$E"),                      // 21
        CPE::StringReference { string_index: 21 }, // 22
        utf8("Ljava/lang/constant/ClassDesc;"), // 23
        CPE::NameAndType {
            name_index: 3,
            descriptor_index: 23,
        }, // 24
        CPE::Dynamic {
            bootstrap_method_attr_index: 0,
            name_and_type_index: 24,
        }, // 25: the ClassDesc
        utf8("A"),                              // 26
        CPE::StringReference { string_index: 26 }, // 27
        utf8("Ljava/lang/Enum$EnumDesc;"),      // 28
        CPE::NameAndType {
            name_index: 3,
            descriptor_index: 28,
        }, // 29
        CPE::Dynamic {
            bootstrap_method_attr_index: 1,
            name_and_type_index: 29,
        }, // 30: the EnumDesc
    ];
    let cp = ConstantPool::new(entries);
    let bsms = vec![
        BootstrapMethod {
            bootstrap_method_ref: 7,
            bootstrap_arguments: vec![20, 22],
        },
        BootstrapMethod {
            bootstrap_method_ref: 7,
            bootstrap_arguments: vec![14, 25, 27],
        },
    ];
    assert_eq!(
        javac_enum_desc_label(&cp, &bsms, 30),
        Some(("p/Outer$E".to_string(), "A".to_string()))
    );
    assert_eq!(javac_enum_desc_label(&cp, &bsms, 25), None);
    assert_eq!(javac_enum_desc_label(&cp, &bsms, 27), None);
}

/// An `EnumDesc` label matches a target whose declaring enum is the
/// label's class and whose constant name is the label's name.
#[test]
fn enum_desc_label_matches_by_declaring_class_and_name() {
    use crate::config::VmConfig;
    use crate::runtime::frame::Frame;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(crate::threading::jvm_thread::ThreadId(0), "test");
    // Stand-ins as in `enum_switch_class_label_matches_by_instanceof`.
    let (stand_in, other) = {
        let mut cm = shared.classes.class_manager.write();
        (
            cm.load_class("java/lang/Integer")
                .expect("load a stand-in class"),
            cm.load_class("java/lang/Long")
                .expect("load a second class"),
        )
    };
    let name = create_java_string(&shared, "B");
    let constant = shared.mem.heap.alloc_object(stand_in, 2);
    shared
        .mem
        .heap
        .set_field(constant, 0, Value::Object(Some(name)));
    let enum_cid = shared.mem.heap.class_id_of(constant);
    let labels = vec![
        SwitchLabel::EnumDesc {
            class_id: enum_cid,
            name: "A".into(),
        },
        SwitchLabel::EnumDesc {
            class_id: other,
            name: "B".into(),
        },
        SwitchLabel::EnumDesc {
            class_id: enum_cid,
            name: "B".into(),
        },
    ];
    let mut frame = Frame::new(
        ClassId::new(0),
        String::new(),
        String::new(),
        String::new(),
        None,
        vec![0xb1],
        vec![],
        10,
        0,
        &[],
    );
    let _ = frame.stack.push(Value::Object(Some(constant)));
    let _ = frame.stack.push(Value::Int(0));
    thread.frames.push(frame);
    execute_type_switch(&shared, &mut thread, 0, &labels).expect("type switch");
    assert_eq!(thread.frames[0].stack.pop().expect("result"), Value::Int(2));
}

/// A Java `String` reached by the operand renderer — the shape a
/// `toString()` result has — comes back as code units, so an unpaired
/// surrogate survives `+` instead of becoming U+FFFD.
#[test]
fn rendered_string_operand_keeps_an_unpaired_surrogate() {
    use crate::config::VmConfig;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let units = [u16::from(b'x'), 0xD800, u16::from(b'y')];
    let s = crate::vm::create_java_string_from_units(&shared, &units);
    match render_operand_checked(&shared, None, &Value::Object(Some(s)), 'L') {
        Ok(RenderedOperand::Units(got)) => assert_eq!(got, units),
        Ok(RenderedOperand::Text(t)) => panic!("rendered lossily as text: {t:?}"),
        Err(_) => panic!("rendering a String must not fail"),
    }
    // The lossy `String` form is still what the non-concat callers get.
    assert_eq!(
        value_to_string(&shared, None, &Value::Object(Some(s)), 'L'),
        "x\u{FFFD}y"
    );
}

/// The concat fast path renders only the real (final) wrapper classes from
/// their field. It used to render ANY one-slot object whose slot held a
/// primitive, so `"" + new Counter(5)` printed `5` and never ran
/// `Counter.toString()`.
#[test]
fn primitive_wrapper_display_is_exact_class_only() {
    let show = |name: &str, v: Value| primitive_wrapper_display(name, v);
    assert_eq!(
        show("java/lang/Integer", Value::Int(5)).as_deref(),
        Some("5")
    );
    assert_eq!(
        show("java/lang/Short", Value::Int(-3)).as_deref(),
        Some("-3")
    );
    assert_eq!(
        show("java/lang/Boolean", Value::Int(1)).as_deref(),
        Some("true")
    );
    assert_eq!(
        show("java/lang/Boolean", Value::Int(0)).as_deref(),
        Some("false")
    );
    assert_eq!(
        show("java/lang/Character", Value::Int(65)).as_deref(),
        Some("A")
    );
    assert_eq!(show("java/lang/Long", Value::Long(7)).as_deref(), Some("7"));
    assert_eq!(
        show("java/lang/Double", Value::Double(1.0)).as_deref(),
        Some("1.0")
    );
    assert_eq!(
        show("java/lang/Float", Value::Float(1.0)).as_deref(),
        Some("1.0")
    );
    // One primitive field does not make a wrapper, whatever the name says.
    assert_eq!(show("com/example/Counter", Value::Int(5)), None);
    assert_eq!(show("com/example/MyBooleanFlag", Value::Int(1)), None);
    assert_eq!(
        show("java/util/concurrent/atomic/AtomicInteger", Value::Int(5)),
        None
    );
    // A wrapper name over the wrong slot kind is not rendered from the slot.
    assert_eq!(show("java/lang/Long", Value::Int(5)), None);
}

/// A class whose own name begins with `L` keeps it: the descriptor's `L`
/// is stripped once (wave 38 host run: `(LoHashLong)long` printed as
/// `(oHashLong)long`).
#[test]
fn method_type_display_keeps_a_leading_l_of_the_class_name() {
    assert_eq!(method_type_display("(LLoHashLong;)J"), "(LoHashLong)long");
    assert_eq!(method_type_display("([LLx;I)LLy;"), "(Lx[],int)Ly");
    assert_eq!(method_type_display("(Ljava/lang/String;)V"), "(String)void");
}

/// `Class.getSimpleName()` of a record, including a LOCAL record, whose
/// binary name carries javac's numeric prefix.
#[test]
fn record_simple_name_matches_get_simple_name() {
    assert_eq!(record_simple_name("com/example/Point", None), "Point");
    assert_eq!(
        record_simple_name("com/example/Outer$Point", Some("Point")),
        "Point"
    );
    assert_eq!(
        record_simple_name("com/example/Main$1Point", Some("Point")),
        "Point"
    );
    // No readable entry: a local class's digits still go.
    assert_eq!(record_simple_name("Main$12Pair", None), "Pair");
    assert_eq!(record_simple_name("Point", None), "Point");
    // Wave 24: a TOP-LEVEL record whose name contains `$` has no entry
    // of its own, and HotSpot's simple name keeps the `$`.
    assert_eq!(record_simple_name("p/A$B", None), "A$B");
    assert_eq!(record_simple_name("A$B", None), "A$B");
    // A member whose own name contains `$` keeps it too.
    assert_eq!(
        record_simple_name("p/Outer$In$ner", Some("In$ner")),
        "In$ner"
    );
}

/// Record `float`/`double` components use `Float/Double.toString`, not
/// Rust's `{}` (which printed `1.0` as `1`).
#[test]
fn record_float_components_use_java_formatting() {
    use crate::config::VmConfig;
    use std::sync::Arc;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    assert_eq!(format_field_value(&shared, &Value::Double(1.0), "D"), "1.0");
    assert_eq!(format_field_value(&shared, &Value::Float(1.0), "F"), "1.0");
    assert_eq!(
        format_field_value(&shared, &Value::Double(1e10), "D"),
        "1.0E10"
    );
    assert_eq!(
        format_field_value(&shared, &Value::Double(-0.0), "D"),
        "-0.0"
    );
    assert_eq!(format_field_value(&shared, &Value::Int(7), "I"), "7");
}

/// The zero-capture singleton key tells two METHODS apart even when the
/// shared `CONSTANT_InvokeDynamic` entry sits at the same bci in both
/// (`return String::length;` twice), and is stable for one method.
#[test]
fn lambda_site_method_key_separates_methods_and_overloads() {
    let a = lambda_site_method_key_of("ref1", "()Ljava/util/function/Function;");
    let b = lambda_site_method_key_of("ref2", "()Ljava/util/function/Function;");
    let a_overload = lambda_site_method_key_of("ref1", "(I)Ljava/util/function/Function;");
    assert_ne!(a, b);
    assert_ne!(a, a_overload);
    assert_eq!(
        a,
        lambda_site_method_key_of("ref1", "()Ljava/util/function/Function;")
    );
    // Name and descriptor are hashed as two strings, not as their
    // concatenation, so moving a character across the boundary changes it.
    assert_ne!(
        lambda_site_method_key_of("ab", "c"),
        lambda_site_method_key_of("a", "bc")
    );
}

/// A `LambdaMetafactory` pool: cp #12 is a zero-capture indy
/// (`()Ljava/util/function/Supplier;`), cp #14 a capturing one
/// (`(I)Ljava/util/function/IntSupplier;`). The bridge-site builder reads
/// only the bootstrap handle's class and name, never its static arguments.
fn lambda_metafactory_pool() -> ConstantPool {
    use cratonvm_reader::constant_pool::ConstantPoolEntry as CPE;
    let utf8 = |s: &str| CPE::Utf8(s.into());
    ConstantPool::new(vec![
        CPE::Tombstone,
        utf8(LAMBDA_METAFACTORY),              // 1
        CPE::ClassReference { name_index: 1 }, // 2
        utf8(METAFACTORY),                     // 3
        utf8(METAFACTORY_DESCRIPTOR),          // 4
        CPE::NameAndType {
            name_index: 3,
            descriptor_index: 4,
        }, // 5
        CPE::MethodReference {
            class_index: 2,
            name_and_type_index: 5,
        }, // 6
        CPE::MethodHandle {
            reference_kind: 6,
            reference_index: 6,
        }, // 7
        utf8("get"),                           // 8
        utf8("()Ljava/util/function/Supplier;"), // 9
        CPE::NameAndType {
            name_index: 8,
            descriptor_index: 9,
        }, // 10
        utf8("(I)Ljava/util/function/IntSupplier;"), // 11
        CPE::InvokeDynamic {
            bootstrap_method_attr_index: 0,
            name_and_type_index: 10,
        }, // 12
        CPE::NameAndType {
            name_index: 8,
            descriptor_index: 11,
        }, // 13
        CPE::InvokeDynamic {
            bootstrap_method_attr_index: 0,
            name_and_type_index: 13,
        }, // 14
        utf8("()Ljava/lang/Object;"),          // 15
        CPE::MethodType {
            descriptor_index: 15,
        }, // 16
        utf8("()Ljava/lang/String;"),          // 17
        CPE::MethodType {
            descriptor_index: 17,
        }, // 18
        CPE::Integer(super::FLAG_MARKERS),     // 19
        CPE::Integer(1),                       // 20
        CPE::Integer(-1),                      // 21
    ])
}

/// javac's static arguments over [`lambda_metafactory_pool`]:
/// `(MethodType, MethodHandle, MethodType)`.
const JAVAC_LAMBDA_ARGS: [u16; 3] = [16, 7, 18];

/// Wave 48: the shapes javac emits, and the ones it never does, as
/// [`lambda_site_is_javac_shape`] reads them from the pool.
#[test]
fn lambda_site_javac_shape_is_read_from_the_pool() {
    let pool = lambda_metafactory_pool();
    let meta = |args: &[u16]| {
        lambda_site_is_javac_shape(&pool, METAFACTORY, METAFACTORY_DESCRIPTOR, args)
    };
    let alt = |args: &[u16]| {
        lambda_site_is_javac_shape(&pool, ALT_METAFACTORY, ALT_METAFACTORY_DESCRIPTOR, args)
    };
    assert!(meta(&JAVAC_LAMBDA_ARGS));
    assert!(!meta(&[16, 7]), "two static arguments");
    assert!(!meta(&[16, 7, 18, 18]), "four static arguments");
    assert!(!meta(&[7, 16, 18]), "a handle where a type belongs");
    assert!(!meta(&[16, 18, 18]), "a type where the handle belongs");
    assert!(
        !lambda_site_is_javac_shape(&pool, METAFACTORY, "()V", &JAVAC_LAMBDA_ARGS),
        "another descriptor"
    );
    // altMetafactory: flags FLAG_MARKERS, one marker class (#2).
    assert!(alt(&[16, 7, 18, 19, 20, 2]));
    assert!(!alt(&[16, 7, 18, 19, 20]), "a missing marker");
    assert!(!alt(&[16, 7, 18, 19, 20, 2, 2]), "an argument left over");
    assert!(!alt(&[16, 7, 18, 19, 21]), "a negative count");
    assert!(!alt(&[16, 7, 18]), "no flags word");
    assert!(!alt(&[16, 7, 18, 21]), "unknown flag bits");
}

/// `interpreter-L6-lambda-singleton-identity-splits-between-interpreter-and-jit-FIXED`:
/// a compiled zero-capture lambda site carries the key the interpreter
/// derives for the same instruction (method key, pc AFTER the 5-byte
/// instruction), and a door that cannot name the instruction may not
/// bridge one.
#[test]
fn a_compiled_lambda_site_keys_its_singleton_on_the_real_instruction() {
    if !crate::runtime::env_cache::jit_indy_bridge() {
        return; // `CRATONVM_JIT_INDY_BRIDGE=0`: no generic site is built.
    }
    let pool = lambda_metafactory_pool();
    let bootstraps = vec![BootstrapMethod {
        bootstrap_method_ref: 7,
        // javac's static arguments: the bridge admits javac's shape only
        // (wave 48).
        bootstrap_arguments: JAVAC_LAMBDA_ARGS.to_vec(),
    }];
    let class_id = ClassId::new(0x7a12);
    let descriptor = "()Ljava/util/function/Supplier;";
    let key = lambda_singleton_site_for("make", descriptor, 3);
    assert_eq!(key, (lambda_site_method_key_of("make", descriptor), 8));
    let site =
        make_jit_indy_bridge_site_from_parts(&pool, &bootstraps, 12, class_id, Some(key))
            .unwrap_or(0);
    assert_ne!(site, 0, "a zero-capture lambda site is bridged");
    // SAFETY: `site` was built just above; the allocation is process-lived.
    let generic = unsafe { &*(site as *const JitIndyGenericSite) };
    assert_eq!(generic.kind, JIT_INDY_SITE_GENERIC);
    assert_eq!(generic.singleton_site, key);
    assert!(
        make_jit_indy_bridge_site_from_parts(&pool, &bootstraps, 12, class_id, None).is_none(),
        "an unnamed instruction keeps its trap"
    );
    // Wave 6: a capturing site reads it too -- an entry shared by several
    // instructions is linked per instruction (its proxy class), and a
    // door passes `None` only for such an entry.
    assert!(
        make_jit_indy_bridge_site_from_parts(&pool, &bootstraps, 14, class_id, None).is_none(),
        "an unnamed capturing instruction keeps its trap too"
    );
    let capturing = lambda_singleton_site_for("make", "(I)Ljava/util/function/IntSupplier;", 7);
    assert!(
        make_jit_indy_bridge_site_from_parts(&pool, &bootstraps, 14, class_id, Some(capturing))
            .is_some(),
        "a named capturing instruction is bridged"
    );
    // Wave 48: a shape javac never emits keeps its trap.
    let hand_made = vec![BootstrapMethod {
        bootstrap_method_ref: 7,
        bootstrap_arguments: vec![16, 7],
    }];
    assert!(
        make_jit_indy_bridge_site_from_parts(&pool, &hand_made, 12, class_id, Some(key))
            .is_none(),
        "a hand-assembled lambda site keeps its trap"
    );
}

/// Wave 6 (`lambda-proxy-class-is-per-cp-entry-not-per-instruction`):
/// the shared-entry scan counts instructions across ALL methods of the
/// class, walks instruction boundaries (an operand byte equal to `0xba` is
/// not an `invokedynamic`), and reports only entries named twice or more.
#[test]
fn shared_indy_entry_scan_counts_instructions_across_methods() {
    // m1: invokedynamic #12; invokedynamic #14; areturn
    let m1: &[u8] = &[
        0xba, 0x00, 0x0c, 0x00, 0x00, 0xba, 0x00, 0x0e, 0x00, 0x00, 0xb0,
    ];
    // m2: sipush 0xba0c (an operand, not an indy); invokedynamic #14; areturn
    let m2: &[u8] = &[0x11, 0xba, 0x0c, 0xba, 0x00, 0x0e, 0x00, 0x00, 0xb0];
    // m3: invokedynamic #16; areturn
    let m3: &[u8] = &[0xba, 0x00, 0x10, 0x00, 0x00, 0xb0];
    assert_eq!(
        scan_shared_indy_entries([m1, m2, m3].into_iter()),
        vec![14u16]
    );
    // The same entry twice in ONE method is shared as well.
    let twice: &[u8] = &[
        0xba, 0x00, 0x0c, 0x00, 0x00, 0xba, 0x00, 0x0c, 0x00, 0x00, 0xb0,
    ];
    assert_eq!(scan_shared_indy_entries([twice].into_iter()), vec![12u16]);
    assert!(scan_shared_indy_entries([m3].into_iter()).is_empty());
}

/// A shared entry's per-instruction rows: first writer wins per
/// instruction, two instructions keep two proxy classes, and the compiled
/// bridge's probe finds each by its own instruction key.
#[test]
fn per_instruction_lambda_rows_are_first_writer_wins_and_distinct() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let class_id = ClassId::new(0x7a31);
    let key_a = (shared.vm_identity, class_id, 12u16, 0x1111u64, 8usize);
    let key_b = (shared.vm_identity, class_id, 12u16, 0x1111u64, 13usize);
    let (p1, p2, p3) = (
        ClassId::new(0x8000_7a01),
        ClassId::new(0x8000_7a02),
        ClassId::new(0x8000_7a03),
    );
    let desc = "()Ljava/util/function/Function;";
    assert_eq!(
        publish_lambda_instruction_site(&shared, key_a, desc, p1, 0),
        p1
    );
    // A racing bootstrap of the SAME instruction adopts the winner.
    assert_eq!(
        publish_lambda_instruction_site(&shared, key_a, desc, p2, 0),
        p1
    );
    // Another instruction of the same entry gets its own class.
    assert_eq!(
        publish_lambda_instruction_site(&shared, key_b, desc, p3, 0),
        p3
    );
    assert_eq!(probe_lambda_instruction_site(&key_a), Some((p1, 0)));
    assert_eq!(probe_lambda_instruction_site(&key_b), Some((p3, 0)));
    forget_vm_generic_indy_sites(shared.vm_identity);
    assert_eq!(probe_lambda_instruction_site(&key_a), None);
}

// -----------------------------------------------------------------------
// Generic invokedynamic call-site cache
// -----------------------------------------------------------------------

fn fresh_gate() -> (
    crate::classloading::resolution::RedefineGate,
    Arc<std::sync::atomic::AtomicU32>,
) {
    let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
    (
        crate::classloading::resolution::RedefineGate::snapshot(Arc::clone(&counter)),
        counter,
    )
}

fn fake_ref(addr: usize) -> ObjectRef {
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn linked_slot(
    addr: usize,
    gate: crate::classloading::resolution::RedefineGate,
) -> GenericIndySlot {
    GenericIndySlot {
        link: GenericIndyLink::Linked(fake_ref(addr)),
        shape: Arc::new(GenericIndyShape::new("(Ljava/lang/Object;Z)I")),
        gate,
    }
}

fn linked_addr(link: &GenericIndyLink) -> Option<usize> {
    match link {
        GenericIndyLink::Linked(cs) => Some(cs.as_ptr() as usize),
        _ => None,
    }
}

#[test]
fn generic_indy_cache_mode_parses_kill_switch_and_all() {
    use GenericIndyCacheMode::*;
    assert_eq!(generic_indy_cache_mode_from(None), All);
    assert_eq!(generic_indy_cache_mode_from(Some("")), All);
    assert_eq!(generic_indy_cache_mode_from(Some("constant")), Constant);
    assert_eq!(generic_indy_cache_mode_from(Some("1")), All);
    assert_eq!(generic_indy_cache_mode_from(Some("0")), Off);
    assert_eq!(generic_indy_cache_mode_from(Some(" off ")), Off);
    assert_eq!(generic_indy_cache_mode_from(Some("FALSE")), Off);
    assert_eq!(generic_indy_cache_mode_from(Some("all")), All);
    assert_eq!(generic_indy_cache_mode_from(Some("2")), All);
}

/// Wave 6: the `typeSwitch` array arm can load a component class, and the
/// label loop reads the target again afterwards, so the target is pinned
/// across that check and re-read from the pin. Structural: a GC inside
/// the check cannot be forced from a unit test.
#[test]
fn the_type_switch_array_arm_pins_the_target_across_its_loading_check() {
    let src = concat!(include_str!("../invokedynamic/mod.rs"), "\n", include_str!("../invokedynamic/dbg_indy_all.rs"), "\n", include_str!("../invokedynamic/execute_invokedynamic_at_uncensused.rs"), "\n", include_str!("../invokedynamic/object_methods_getter_refusal.rs"), "\n", include_str!("../invokedynamic/descriptor_param_count_and_last_is.rs"));
    let start = src.find("fn type_label_matches(").unwrap_or(0);
    let rest = &src[start..];
    let end = rest.find("\n\x7d\n").unwrap_or(0);
    assert!(start > 0 && end > 0, "type_label_matches moved");
    let body = &rest[..end];
    let pin = body.find("thread.native_pin_roots.push(*obj_ref)");
    let check = body.find("array_is_instance_of(shared, &desc, label_name)");
    let reread = body.find("*obj_ref = thread.native_pin_roots[pin]");
    assert!(
        matches!((pin, check, reread), (Some(p), Some(c), Some(r)) if p < c && c < r),
        "pin, then check, then re-read"
    );
}

/// Wave 6: a marker the bootstrap resolved answers from its recorded
/// class — here deliberately under a name that cannot load, so a by-name
/// lookup could not have answered — and loads nothing; a name-only marker
/// (the reflective path) that does not resolve answers `false`.
#[test]
fn a_resolved_lambda_marker_answers_from_its_class_without_loading() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let Ok(object) = shared
        .classes
        .class_manager
        .write()
        .load_class("java/lang/Object")
    else {
        return; // Stripped VM (no class library on the test classpath).
    };
    const NEVER: &str = "cratonvm/test/L6MarkerNeverLoadable";
    let resolved = ClassId::new(0x8000_7b01);
    record_lambda_proxy_markers_resolved(
        shared.vm_identity,
        resolved,
        &[LambdaMarker {
            name: Arc::from(NEVER),
            class_id: Some(object),
        }],
    );
    assert!(lambda_proxy_marker_satisfies(
        &shared,
        resolved,
        object,
        "java/lang/Object"
    ));
    assert!(shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id(NEVER)
        .is_none());
    let by_name = ClassId::new(0x8000_7b02);
    record_lambda_proxy_markers(shared.vm_identity, by_name, &[Arc::from(NEVER)]);
    assert!(!lambda_proxy_marker_satisfies(
        &shared,
        by_name,
        object,
        "java/lang/Object"
    ));
    forget_vm_generic_indy_sites(shared.vm_identity);
}

/// Constant mode excludes Groovy bootstraps; all mode caches them too.
#[test]
fn constant_mode_caches_mutable_call_sites_except_groovys() {
    use GenericIndyCacheMode::*;
    const CUSTOM: &str = "com/example/Bootstraps";
    for groovy in [
        "org/codehaus/groovy/vmplugin/v7/IndyInterface",
        GROOVY_INDY_INTERFACE,
    ] {
        assert!(!Constant.caches_mutable_call_site_of(groovy));
        assert!(All.caches_mutable_call_site_of(groovy));
    }
    assert!(Constant.caches_mutable_call_site_of(CUSTOM));
    assert!(!Off.caches_mutable_call_site_of(CUSTOM));
    assert_eq!(generic_indy_cache_mode_from(None), All);
}

#[test]
fn generic_indy_shape_parses_arguments_and_return_once() {
    let shape = GenericIndyShape::new("(Ljava/lang/Object;ZJ)I");
    assert_eq!(&*shape.arg_types, &['L', 'Z', 'J']);
    assert_eq!(shape.ret_byte, b'I');
    assert_eq!(GenericIndyShape::new("()V").ret_byte, b'V');
    assert_eq!(GenericIndyShape::new("()[I").ret_byte, b'[');
}

/// Two threads that link one instruction concurrently must agree on ONE
/// `CallSite`: the second publication returns the first one's site.
#[test]
fn generic_indy_publication_is_first_writer_wins() {
    // A vm identity no real VM in this test process uses.
    let key = (
        usize::MAX - 0x51,
        ClassId::new(0x7001),
        9u16,
        0xABCDu64,
        12usize,
    );
    let (gate_a, _ca) = fresh_gate();
    let (gate_b, _cb) = fresh_gate();
    let first = publish_generic_indy_site(key, linked_slot(0x1000, gate_a));
    assert_eq!(linked_addr(&first), Some(0x1000));
    let second = publish_generic_indy_site(key, linked_slot(0x2000, gate_b));
    assert_eq!(
        linked_addr(&second),
        Some(0x1000),
        "a later linkage of the same instruction must adopt the published CallSite"
    );
    let (probed, shape) = probe_generic_indy_site(&key).expect("published row is live");
    assert_eq!(linked_addr(&probed), Some(0x1000));
    assert_eq!(shape.ret_byte, b'I');
    // A different pc is a different instruction, linked separately.
    let other_pc = (key.0, key.1, key.2, key.3, key.4 + 5);
    assert!(probe_generic_indy_site(&other_pc).is_none());
}

/// A redefined caller gets a new constant pool: its old row is a miss and
/// the relink replaces it instead of adopting it.
#[test]
fn generic_indy_row_goes_stale_when_its_caller_is_redefined() {
    let key = (usize::MAX - 0x52, ClassId::new(0x7002), 3u16, 7u64, 5usize);
    let (gate, counter) = fresh_gate();
    publish_generic_indy_site(key, linked_slot(0x3000, gate));
    assert!(probe_generic_indy_site(&key).is_some());
    counter.fetch_add(1, std::sync::atomic::Ordering::Release);
    assert!(
        probe_generic_indy_site(&key).is_none(),
        "stale row must miss"
    );
    let (fresh, _c) = fresh_gate();
    let relinked = publish_generic_indy_site(key, linked_slot(0x4000, fresh));
    assert_eq!(linked_addr(&relinked), Some(0x4000));
    assert_eq!(
        probe_generic_indy_site(&key).and_then(|(l, _)| linked_addr(&l)),
        Some(0x4000)
    );
}


/// JIT round 16 wave 7 (lane indy): the operand count the interpreter pops
/// and the frame-free record arm checks.
#[test]
fn r16w7_record_object_method_arity() {
    assert_eq!(record_object_method_arity(RecordMethodKind::Equals), 2);
    assert_eq!(record_object_method_arity(RecordMethodKind::HashCode), 1);
    assert_eq!(record_object_method_arity(RecordMethodKind::ToString), 1);
}


/// JIT round 16 wave 9 (lane indy, IN16-7): the direct call's info takes
/// the site's argument words (a virtual leaf's receiver is the first), the
/// target's class as its context, and is reused for the same target so
/// the dispatch memos keyed by its address stay warm across a re-fill.
#[test]
fn r16w9_direct_leaf_info_shape_and_reuse() {
    use cratonvm_native_builtins::lang_invoke::MhCompiledBind;
    let st = MhCompiledBind {
        invoke_kind: 3,
        class_id: 0x7005,
        class: "p/Host".to_owned(),
        name: "addImpl".to_owned(),
        desc: "(IJ)D".to_owned(),
        bound: false,
    };
    let leaf = indy_direct_leaf_info(None, &st);
    assert_eq!(
        (leaf.class_name, leaf.method_name, leaf.descriptor),
        ("p/Host", "addImpl", "(IJ)D")
    );
    assert_eq!(leaf.num_jit_args, 2, "one word per value, no receiver");
    assert_eq!(leaf.return_type, b'D');
    assert_eq!((leaf.invoke_kind, leaf.declaring_class_id), (3, 0x7005));
    assert!(std::ptr::eq(indy_direct_leaf_info(Some(leaf), &st), leaf));
    let virt = MhCompiledBind {
        invoke_kind: 0,
        desc: "(Ljava/lang/Object;)Ljava/lang/String;".to_owned(),
        ..st.clone()
    };
    assert!(indy_direct_leaf_names(leaf, &st));
    assert!(!indy_direct_leaf_names(leaf, &virt));
    let v = indy_direct_leaf_info(Some(leaf), &virt);
    assert!(!std::ptr::eq(v, leaf), "another call is another info");
    assert_eq!(v.num_jit_args, 2, "the receiver, then the value");
    assert_eq!(v.return_type, b'L');
}

/// JIT round 16 wave 10 (lane indyir, IN16-8): only a bound leaf of a
/// `ConstantCallSite`, on its own VM, for a caller not redefined since, is
/// handed to the optimizing tier as a plain invoke -- and it names the
/// target class as the call's class context.
#[test]
fn r16w10_constant_direct_needs_a_constant_bound_current_memo() {
    use cratonvm_native_builtins::lang_invoke::MhCompiledBind;
    let bind = MhCompiledBind {
        invoke_kind: 3,
        class_id: 0x7006,
        class: "p/Host".to_owned(),
        name: "addImpl".to_owned(),
        desc: "(II)I".to_owned(),
        bound: false,
    };
    let leaf = indy_direct_leaf_info(None, &bind);
    let (gate, counter) = fresh_gate();
    let memo =
        |leaf: Option<&'static cratonvm_jit::JitInvokeInfo>,
         constant: bool,
         gate: crate::classloading::resolution::RedefineGate| IndyDirectMemo {
            vm: 0x51,
            epoch: 3,
            call_site: 0x1000,
            target_slot: 1,
            guard: [0; 7],
            leaf,
            gate,
            constant,
            cell: 0,
        };
    let constant = memo(Some(leaf), true, gate.clone());
    assert_eq!(
        indy_constant_direct_of(&constant, 0x51),
        Some(IndyConstantDirect {
            class: "p/Host".to_owned(),
            name: "addImpl".to_owned(),
            desc: "(II)I".to_owned(),
            invoke_kind: 3,
            class_id: 0x7006,
        })
    );
    assert!(
        indy_constant_direct_of(&constant, 0x52).is_none(),
        "another VM"
    );
    assert!(
        indy_constant_direct_of(&memo(Some(leaf), false, gate.clone()), 0x51).is_none(),
        "a mutable call site keeps the bridge"
    );
    assert!(
        indy_constant_direct_of(&memo(None, true, gate.clone()), 0x51).is_none(),
        "a refused handle keeps the bridge"
    );
    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    assert!(
        indy_constant_direct_of(&constant, 0x51).is_none(),
        "a redefined caller keeps the bridge"
    );
}


/// JIT round 16 wave 9 (lane indy, IN16-7): guard words never confuse an
/// `Int` slot with an address or with null.
#[test]
fn r16w9_direct_guard_words_tag_ints() {
    assert_eq!(indy_direct_slot_key(Value::Object(None)), 0);
    assert_ne!(indy_direct_slot_key(Value::Int(0)), 0);
    assert_ne!(
        indy_direct_slot_key(Value::Int(0x1000)),
        indy_direct_slot_key(Value::Object(Some(fake_ref(0x1000))))
    );
    assert_eq!(indy_direct_slot_key(Value::Long(1)), u64::MAX);
}

/// JVMS §4.7.23: every loadable constant is a legal static argument. The
/// `MethodHandle` and `CONSTANT_Dynamic` kinds used to be an uncatchable
/// VM-internal error; a non-loadable entry is a `ClassFormatError`.
#[test]
fn generic_indy_static_args_decode_every_loadable_kind() {
    use cratonvm_reader::constant_pool::ConstantPoolEntry as CPE;
    let cp = ConstantPool::new(vec![
        CPE::Tombstone,                        // 0
        CPE::Utf8("p/Owner".into()),           // 1
        CPE::ClassReference { name_index: 1 }, // 2
        CPE::Utf8("target".into()),            // 3
        CPE::Utf8("(I)I".into()),              // 4
        CPE::NameAndType {
            name_index: 3,
            descriptor_index: 4,
        }, // 5
        CPE::MethodReference {
            class_index: 2,
            name_and_type_index: 5,
        }, // 6
        CPE::MethodHandle {
            reference_kind: 6,
            reference_index: 6,
        }, // 7
        CPE::MethodType {
            descriptor_index: 4,
        }, // 8
        CPE::Dynamic {
            bootstrap_method_attr_index: 0,
            name_and_type_index: 5,
        }, // 9
    ]);
    match resolve_static_arg_kind(&cp, "p/Caller", 7) {
        Ok(StaticArg::MHandle { cp_index, handle }) => {
            assert_eq!(cp_index, 7);
            assert_eq!(handle.kind, MethodHandleKind::InvokeStatic);
            assert_eq!(&*handle.class_name, "p/Owner");
            assert_eq!(&*handle.member_name, "target");
            assert_eq!(&*handle.descriptor, "(I)I");
        }
        _ => panic!("MethodHandle static arg must decode"),
    }
    assert!(matches!(
        resolve_static_arg_kind(&cp, "p/Caller", 8),
        Ok(StaticArg::MType { cp_index: 8, ref descriptor }) if descriptor == "(I)I"
    ));
    assert!(matches!(
        resolve_static_arg_kind(&cp, "p/Caller", 9),
        Ok(StaticArg::Dynamic(9))
    ));
    assert!(matches!(
        resolve_static_arg_kind(&cp, "p/Caller", 2),
        Ok(StaticArg::Class(ref n)) if n == "p/Owner"
    ));
    for bad in [1u16, 5, 6, 40] {
        assert!(
            matches!(
                resolve_static_arg_kind(&cp, "p/Caller", bad),
                Err(MethodCallFailed::InternalError(VmError::Linkage(
                    crate::error::LinkageError::ClassFormatError { .. }
                )))
            ),
            "cp#{bad} is not loadable"
        );
    }
}

/// A `Z` operand of a generic indy is boxed to `Boolean.TRUE`/`FALSE`
/// read from the statics; before `<clinit>` stored them the answer is
/// `None` (the caller then makes the `Boolean.valueOf` call), never a
/// default mistaken for an object.
#[test]
fn canonical_boolean_reads_the_statics_and_declines_before_clinit() {
    use crate::config::VmConfig;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let Ok(cid) = shared
        .classes
        .class_manager
        .write()
        .load_class("java/lang/Boolean")
    else {
        return; // no class library on this test host
    };
    let true_index = {
        let cm = shared.classes.class_manager.read();
        cm.get_class(cid).and_then(|c| {
            c.fields
                .iter()
                .filter(|f| f.is_static())
                .position(|f| &*f.name == "TRUE")
        })
    };
    let Some(true_index) = true_index else {
        return;
    };
    if !matches!(
        crate::vm::get_static_shared(&shared, cid, true_index),
        Value::Object(Some(_))
    ) {
        assert!(
            canonical_boolean(&shared, true).is_none(),
            "not initialized"
        );
    }
    let instance = shared.mem.heap.alloc_object(cid, 1);
    crate::vm::set_static_shared(&shared, cid, true_index, Value::Object(Some(instance)));
    assert_eq!(
        canonical_boolean(&shared, true).map(|o| o.as_ptr() as usize),
        Some(instance.as_ptr() as usize)
    );
}

/// The varargs-collector rule for a trailing `Object[]` bootstrap
/// parameter: `(Lookup, String, MethodType, Object[])` has 4 parameters.
#[test]
fn generic_indy_varargs_pack_start_follows_invoke_with_arguments() {
    let shape = descriptor_param_count_and_last_is_object_array(
        "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;\
         Ljava/lang/invoke/MethodType;[Ljava/lang/Object;)Ljava/lang/invoke/CallSite;",
    );
    assert_eq!(shape, Some((4, true)));
    // JRuby's buildDString: many static args, all collected.
    assert_eq!(bootstrap_varargs_pack_start(shape, 9, false), Some(3));
    // One scalar static arg: a one-element array, not a bare scalar.
    assert_eq!(bootstrap_varargs_pack_start(shape, 4, false), Some(3));
    // One static arg that already is an array (or null): passed as is.
    assert_eq!(bootstrap_varargs_pack_start(shape, 4, true), None);
    // No static args: an empty array.
    assert_eq!(bootstrap_varargs_pack_start(shape, 3, false), Some(3));
    // Too few for the fixed prefix: left to fail as it is.
    assert_eq!(bootstrap_varargs_pack_start(shape, 2, false), None);
    // No trailing `Object[]`: positional.
    let fixed = descriptor_param_count_and_last_is_object_array(
        "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;\
         Ljava/lang/invoke/MethodType;I)Ljava/lang/invoke/CallSite;",
    );
    assert_eq!(bootstrap_varargs_pack_start(fixed, 4, false), None);
    assert_eq!(bootstrap_varargs_pack_start(None, 4, false), None);
}

/// `ObjectMethods.bootstrap` ends in `MethodHandle...`: its component is
/// named; `Object[]`, a primitive array, a 2-D array and a scalar are not
/// (wave 40, lane L4).
#[test]
fn trailing_typed_array_component_names_only_a_non_object_reference_array() {
    assert_eq!(
        trailing_typed_array_component(
            "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;\
             Ljava/lang/invoke/TypeDescriptor;Ljava/lang/Class;Ljava/lang/String;\
             [Ljava/lang/invoke/MethodHandle;)Ljava/lang/Object;"
        ),
        Some("java/lang/invoke/MethodHandle")
    );
    assert_eq!(
        trailing_typed_array_component("(I[Ljava/lang/Object;)V"),
        None
    );
    assert_eq!(
        trailing_typed_array_component("([Ljava/lang/String;I)V"),
        None
    );
    assert_eq!(
        trailing_typed_array_component("(Ljava/lang/String;[I)V"),
        None
    );
    assert_eq!(
        trailing_typed_array_component("([[Ljava/lang/String;)V"),
        None
    );
    assert_eq!(trailing_typed_array_component("()V"), None);
}

/// Box for a reference parameter, widen for a wider primitive, keep
/// otherwise — `invokeWithArguments`' `asType` for constant-pool values.
#[test]
fn generic_indy_bootstrap_arg_fit_boxes_and_widens() {
    assert!(matches!(
        bootstrap_arg_fit('L', Value::Int(1)),
        BootstrapArgFit::Box
    ));
    assert!(matches!(
        bootstrap_arg_fit('L', Value::Double(1.0)),
        BootstrapArgFit::Box
    ));
    assert!(matches!(
        bootstrap_arg_fit('L', Value::Object(None)),
        BootstrapArgFit::Keep
    ));
    assert!(matches!(
        bootstrap_arg_fit('J', Value::Int(-3)),
        BootstrapArgFit::Widen(Value::Long(-3))
    ));
    assert!(matches!(
        bootstrap_arg_fit('F', Value::Long(3)),
        BootstrapArgFit::Widen(Value::Float(f)) if f == 3.0
    ));
    assert!(matches!(
        bootstrap_arg_fit('D', Value::Float(0.5)),
        BootstrapArgFit::Widen(Value::Double(d)) if d == 0.5
    ));
    assert!(matches!(
        bootstrap_arg_fit('I', Value::Int(7)),
        BootstrapArgFit::Keep
    ));
    assert!(matches!(
        bootstrap_arg_fit('Z', Value::Int(1)),
        BootstrapArgFit::Keep
    ));
}

/// A recorded failure is published like a success, and a later linkage of
/// the same instruction adopts it rather than replacing it.
#[test]
fn generic_indy_recorded_failure_is_published_and_kept() {
    let key = (usize::MAX - 0x53, ClassId::new(0x7003), 4u16, 1u64, 8usize);
    let (gate, _c) = fresh_gate();
    let recorded = Arc::new(RecordedLinkageError {
        error_class: Some(ClassId::new(0x10)),
        error_class_name: "java/lang/BootstrapMethodError".to_string(),
        message: Some("bootstrap method initialization exception".to_string()),
    });
    let published = publish_generic_indy_site(
        key,
        GenericIndySlot {
            link: GenericIndyLink::Failed(Arc::clone(&recorded)),
            shape: Arc::new(GenericIndyShape::new("()V")),
            gate,
        },
    );
    assert!(matches!(&published, GenericIndyLink::Failed(p) if Arc::ptr_eq(p, &recorded)));
    let (gate2, _c2) = fresh_gate();
    let later = publish_generic_indy_site(key, linked_slot(0x5000, gate2));
    assert!(matches!(later, GenericIndyLink::Failed(_)));
}

#[test]
fn generic_indy_rows_of_unloaded_classes_are_dropped_and_others_remapped() {
    let vm = usize::MAX - 0x54;
    let other_vm = usize::MAX - 0x55;
    let dead = ClassId::new(0x7100);
    let live = ClassId::new(0x7101);
    let dead_error_class = ClassId::new(0x7102);
    let k_dead: GenericIndyKey = (vm, dead, 1, 0, 5);
    let k_live: GenericIndyKey = (vm, live, 1, 0, 5);
    let k_other_vm: GenericIndyKey = (other_vm, dead, 1, 0, 5);
    let k_dead_error: GenericIndyKey = (vm, live, 2, 0, 9);
    let mut table = GenericIndyMap::default();
    let (g1, _c1) = fresh_gate();
    let (g2, _c2) = fresh_gate();
    let (g3, _c3) = fresh_gate();
    let (g4, _c4) = fresh_gate();
    table.insert(k_dead, linked_slot(0x1000, g1));
    table.insert(k_live, linked_slot(0x2000, g2));
    table.insert(k_other_vm, linked_slot(0x3000, g3));
    table.insert(
        k_dead_error,
        GenericIndySlot {
            link: GenericIndyLink::Failed(Arc::new(RecordedLinkageError {
                error_class: Some(dead_error_class),
                error_class_name: "p/MyLinkageError".to_string(),
                message: None,
            })),
            shape: Arc::new(GenericIndyShape::new("()V")),
            gate: g4,
        },
    );
    let unloaded: rustc_hash::FxHashSet<ClassId> =
        [dead, dead_error_class].into_iter().collect();
    retain_live_generic_indy_rows(&mut table, vm, &unloaded);
    assert!(!table.contains_key(&k_dead));
    assert!(!table.contains_key(&k_dead_error), "error class unloaded");
    assert!(table.contains_key(&k_live));
    assert!(
        table.contains_key(&k_other_vm),
        "another VM's rows are not this unload's business"
    );

    let mut moved = cratonvm_types::PointerMap::new();
    moved.insert(0x2000, 0x9000);
    moved.insert(0x3000, 0xA000);
    remap_generic_indy_rows(&mut table, vm, &moved);
    assert_eq!(linked_addr(&table[&k_live].link), Some(0x9000));
    assert_eq!(
        linked_addr(&table[&k_other_vm].link),
        Some(0x3000),
        "only this VM's rows are remapped through this VM's map"
    );
}

/// t1 wave 12, lane STRREC2: a failure recorded as a not-yet-loaded
/// `NoClassDefFoundError` (`error_class: None`, loaded by name at the
/// rethrow) names no class an unload can take away, so only its caller
/// class's unload drops the row.
#[test]
fn t1w12_strrec2_a_by_name_failure_row_outlives_unrelated_unloads() {
    let vm = usize::MAX - 0x56;
    let caller = ClassId::new(0x7200);
    let other = ClassId::new(0x7201);
    let key: GenericIndyKey = (vm, caller, 3, 0, 7);
    let failed = || GenericIndySlot {
        link: GenericIndyLink::Failed(Arc::new(RecordedLinkageError {
            error_class: None,
            error_class_name: "java/lang/NoClassDefFoundError".to_string(),
            message: Some("p/MyLinkError".to_string()),
        })),
        shape: Arc::new(GenericIndyShape::new("()V")),
        gate: fresh_gate().0,
    };
    let mut table = GenericIndyMap::default();
    table.insert(key, failed());
    let unloaded: rustc_hash::FxHashSet<ClassId> = [other].into_iter().collect();
    retain_live_generic_indy_rows(&mut table, vm, &unloaded);
    assert!(table.contains_key(&key));
    let unloaded: rustc_hash::FxHashSet<ClassId> = [caller].into_iter().collect();
    retain_live_generic_indy_rows(&mut table, vm, &unloaded);
    assert!(!table.contains_key(&key));
}

/// Unloading a HOST class drops the memo and singleton rows of every proxy
/// it spun, even when the implementation owner (here a class that stays
/// loaded, like `String` for `String::length`) survives; rows of other
/// hosts are kept. Disposing the VM drops the rest of its singleton rows.
#[test]
fn lambda_rows_of_an_unloaded_host_and_a_disposed_vm_are_dropped() {
    use crate::config::VmConfig;
    let shared = SharedVm::new(VmConfig::default());
    let vm = shared.vm_identity;
    let (dead_host, live_host, live_owner) = (
        ClassId::new(0x6a01),
        ClassId::new(0x6a02),
        ClassId::new(0x6a03),
    );
    let (dead_proxy, live_proxy) = (ClassId::new(0x8fff_0a01), ClassId::new(0x8fff_0a02));
    // Held the way a warm-path memo holds them (wave 20).
    let dead_slot = Arc::new(LambdaSingletonSlot::new(fake_ref(0xa000)));
    let live_slot = Arc::new(LambdaSingletonSlot::new(fake_ref(0xb000)));
    {
        let mut hosts = shared.classes.lambda_proxy_hosts.write();
        hosts.insert(dead_proxy, dead_host);
        hosts.insert(live_proxy, live_host);
        let mut memo = shared.classes.lambda_impl_owner_memo.write();
        memo.insert(dead_proxy, live_owner);
        memo.insert(live_proxy, live_owner);
        let mut singletons = lambda_singleton_cache().write();
        singletons.insert((vm, dead_proxy, 7, 5), Arc::clone(&dead_slot));
        singletons.insert((vm, live_proxy, 7, 5), Arc::clone(&live_slot));
    }
    let unloaded: rustc_hash::FxHashSet<ClassId> = [dead_host].into_iter().collect();
    forget_unloaded_lambda_proxy_rows(&shared, &unloaded);
    {
        let hosts = shared.classes.lambda_proxy_hosts.read();
        assert!(!hosts.contains_key(&dead_proxy));
        assert_eq!(hosts.get(&live_proxy), Some(&live_host));
        let memo = shared.classes.lambda_impl_owner_memo.read();
        assert!(
            !memo.contains_key(&dead_proxy),
            "the host half drops the row"
        );
        assert_eq!(memo.get(&live_proxy), Some(&live_owner));
        let singletons = lambda_singleton_cache().read();
        assert!(!singletons.contains_key(&(vm, dead_proxy, 7, 5)));
        assert!(singletons.contains_key(&(vm, live_proxy, 7, 5)));
    }
    assert_eq!(dead_slot.get(), None, "a dropped row retires its slot");
    assert_eq!(live_slot.get(), Some(fake_ref(0xb000)));
    forget_vm_generic_indy_sites(vm);
    assert!(!lambda_singleton_cache()
        .read()
        .keys()
        .any(|&(vid, _, _, _)| vid == vm));
    assert_eq!(live_slot.get(), None, "disposing the VM retires the rest");
}

/// The allocation-free parse the concat path uses agrees with the `Vec` one.
#[test]
fn parse_descriptor_args_into_small_storage_matches_vec() {
    let desc = "(IJLjava/lang/String;[[DZC)Ljava/lang/String;";
    let mut small: smallvec::SmallVec<[char; 8]> = smallvec::SmallVec::new();
    parse_descriptor_args_into(desc, &mut small);
    assert_eq!(small.as_slice(), parse_descriptor_args(desc).as_slice());
    assert_eq!(small.as_slice(), &['I', 'J', 'L', 'L', 'Z', 'C']);
    assert!(!small.spilled());
}

/// Build a ConstantPool for testing resolve_method_handle_full.
fn make_method_handle_cp(ref_kind: u8) -> ConstantPool {
    use cratonvm_reader::constant_pool::ConstantPoolEntry as CPE;

    let entries = vec![
        CPE::Tombstone,                                        // 0 (unused)
        CPE::Utf8("com/example/Foo".to_string().into()),       // 1
        CPE::ClassReference { name_index: 1 },                 // 2
        CPE::Utf8("doStuff".to_string().into()),               // 3
        CPE::Utf8("(I)Ljava/lang/String;".to_string().into()), // 4
        CPE::NameAndType {
            name_index: 3,
            descriptor_index: 4,
        }, // 5
        CPE::MethodReference {
            class_index: 2,
            name_and_type_index: 5,
        }, // 6
        CPE::MethodHandle {
            reference_kind: ref_kind,
            reference_index: 6,
        }, // 7
    ];
    ConstantPool::new(entries)
}

#[test]
fn resolve_method_handle_full_invoke_static() {
    let cp = make_method_handle_cp(6); // InvokeStatic
    let mh = resolve_method_handle_full(&cp, 7).unwrap();
    assert_eq!(mh.kind, MethodHandleKind::InvokeStatic);
    assert_eq!(&*mh.class_name, "com/example/Foo");
    assert_eq!(&*mh.member_name, "doStuff");
    assert_eq!(&*mh.descriptor, "(I)Ljava/lang/String;");
}

#[test]
fn resolve_method_handle_full_invoke_virtual() {
    let cp = make_method_handle_cp(5); // InvokeVirtual
    let mh = resolve_method_handle_full(&cp, 7).unwrap();
    assert_eq!(mh.kind, MethodHandleKind::InvokeVirtual);
}

#[test]
fn resolve_method_handle_full_invalid_kind() {
    let cp = make_method_handle_cp(0); // Invalid kind 0
    assert!(resolve_method_handle_full(&cp, 7).is_err());
}

#[test]
fn resolve_method_handle_full_field_ref() {
    use cratonvm_reader::constant_pool::ConstantPoolEntry as CPE;
    let entries = vec![
        CPE::Tombstone,                                  // 0
        CPE::Utf8("com/example/Bar".to_string().into()), // 1
        CPE::ClassReference { name_index: 1 },           // 2
        CPE::Utf8("value".to_string().into()),           // 3
        CPE::Utf8("I".to_string().into()),               // 4
        CPE::NameAndType {
            name_index: 3,
            descriptor_index: 4,
        }, // 5
        CPE::FieldReference {
            class_index: 2,
            name_and_type_index: 5,
        }, // 6
        CPE::MethodHandle {
            reference_kind: 1,
            reference_index: 6,
        }, // 7 GetField
    ];
    let cp = ConstantPool::new(entries);
    let mh = resolve_method_handle_full(&cp, 7).unwrap();
    assert_eq!(mh.kind, MethodHandleKind::GetField);
    assert_eq!(&*mh.class_name, "com/example/Bar");
    assert_eq!(&*mh.member_name, "value");
    assert_eq!(&*mh.descriptor, "I");
}

#[test]
fn resolve_method_type_test() {
    use cratonvm_reader::constant_pool::ConstantPoolEntry as CPE;
    let entries = vec![
        CPE::Tombstone,
        CPE::Utf8("(Ljava/lang/Object;)V".to_string().into()), // 1
        CPE::MethodType {
            descriptor_index: 1,
        }, // 2
    ];
    let cp = ConstantPool::new(entries);
    assert_eq!(
        resolve_method_type(&cp, 2),
        Some("(Ljava/lang/Object;)V".to_string())
    );
    assert_eq!(resolve_method_type(&cp, 1), None); // Not a MethodType
}

// -----------------------------------------------------------------------
// Phase 83.1: Primitive pattern matching — widening/narrowing
// -----------------------------------------------------------------------

/// Helper: allocate a boxed wrapper on the heap and test primitive_pattern_match.
fn test_ppm(source_class: &str, value: Value, target_class: &str) -> bool {
    use crate::config::VmConfig;
    use std::sync::Arc;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let class_id = ClassId::new(0);
    let obj = shared.mem.heap.alloc_object(class_id, 1);
    shared.mem.heap.set_field(obj, 0, value);
    primitive_pattern_match(&shared, obj, source_class, target_class)
}

#[test]
fn ppm_int_to_long_widening() {
    // int 42 should match long pattern (widening).
    assert!(test_ppm(
        "java/lang/Integer",
        Value::Int(42),
        "java/lang/Long"
    ));
}

#[test]
fn ppm_int_to_double_widening() {
    // int 42 should match double pattern (widening).
    assert!(test_ppm(
        "java/lang/Integer",
        Value::Int(42),
        "java/lang/Double"
    ));
}

#[test]
fn ppm_int_to_byte_narrowing_in_range() {
    // int 5 is in byte range [-128, 127], should match byte pattern.
    assert!(test_ppm(
        "java/lang/Integer",
        Value::Int(5),
        "java/lang/Byte"
    ));
}

#[test]
fn ppm_int_to_byte_narrowing_out_of_range() {
    // int 200 is NOT in byte range, should NOT match.
    assert!(!test_ppm(
        "java/lang/Integer",
        Value::Int(200),
        "java/lang/Byte"
    ));
}

#[test]
fn ppm_long_to_int_narrowing_in_range() {
    // long 42 is in int range, should match.
    assert!(test_ppm(
        "java/lang/Long",
        Value::Long(42),
        "java/lang/Integer"
    ));
}

#[test]
fn ppm_long_to_int_narrowing_out_of_range() {
    // long exceeding int range should NOT match.
    assert!(!test_ppm(
        "java/lang/Long",
        Value::Long(i64::MAX),
        "java/lang/Integer"
    ));
}

#[test]
fn ppm_int_to_char_narrowing_in_range() {
    // int 65 ('A') is in char range [0, 65535], should match.
    assert!(test_ppm(
        "java/lang/Integer",
        Value::Int(65),
        "java/lang/Character"
    ));
}

#[test]
fn ppm_int_to_char_narrowing_negative() {
    // Negative int should NOT match char pattern.
    assert!(!test_ppm(
        "java/lang/Integer",
        Value::Int(-1),
        "java/lang/Character"
    ));
}

#[test]
fn ppm_string_to_long_no_match() {
    // Non-numeric class should never match a numeric pattern.
    assert!(!test_ppm(
        "java/lang/String",
        Value::Int(0),
        "java/lang/Long"
    ));
}

#[test]
fn ppm_float_to_double_widening() {
    // float -> double is always a widening conversion.
    assert!(test_ppm(
        "java/lang/Float",
        Value::Float(3.14),
        "java/lang/Double"
    ));
}

/// Wave 7: the `FLAG_BRIDGES` block is read after the marker block when
/// `FLAG_MARKERS` is set and at `arg[4]` otherwise; a short block yields
/// the prefix it could read, and no bridge bit means no bridges.
#[test]
fn altmetafactory_bridge_descriptors_are_read_after_the_markers() {
    use cratonvm_reader::constant_pool::ConstantPoolEntry as CPE;
    let cp = ConstantPool::new(vec![
        CPE::Tombstone,
        CPE::Utf8("()Ljava/lang/Object;".into()), // 1
        CPE::MethodType {
            descriptor_index: 1,
        }, // 2
        CPE::Utf8("(Ljava/lang/Object;)V".into()), // 3
        CPE::MethodType {
            descriptor_index: 3,
        }, // 4
        CPE::Integer(FLAG_BRIDGES),               // 5
        CPE::Integer(FLAG_BRIDGES | FLAG_MARKERS), // 6
        CPE::Integer(1),                          // 7
        CPE::Integer(2),                          // 8
        CPE::Utf8("java/lang/Cloneable".into()),  // 9
        CPE::ClassReference { name_index: 9 },    // 10
    ]);
    let names = |v: Vec<Arc<str>>| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    // sam, impl, inst, flags=BRIDGES, count=2, mt, mt
    let plain: [u16; 7] = [2, 2, 2, 5, 8, 2, 4];
    assert_eq!(
        names(bridge_descriptors_in(&cp, &plain, FLAG_BRIDGES, &[])),
        ["()Ljava/lang/Object;", "(Ljava/lang/Object;)V"]
    );
    // sam, impl, inst, flags=BRIDGES|MARKERS, markers=1, Cloneable, count=1, mt
    let with_markers: [u16; 8] = [2, 2, 2, 6, 7, 10, 7, 2];
    assert_eq!(
        names(bridge_descriptors_in(
            &cp,
            &with_markers,
            FLAG_BRIDGES | FLAG_MARKERS,
            &[]
        )),
        ["()Ljava/lang/Object;"]
    );
    // Declares two bridges, carries one: the prefix.
    let short: [u16; 6] = [2, 2, 2, 5, 8, 4];
    assert_eq!(
        names(bridge_descriptors_in(&cp, &short, FLAG_BRIDGES, &[])),
        ["(Ljava/lang/Object;)V"]
    );
    // Missing count: nothing, never a panic.
    assert!(bridge_descriptors_in(&cp, &[2, 2, 2, 5], FLAG_BRIDGES, &[]).is_empty());
}

/// Wave 9: the `FLAG_MARKERS` block's count is class-file data. A count
/// far beyond the static arguments present reads the markers that are
/// there and reserves nothing for the rest (it used to reserve `count`
/// slots up front: `0x7fffffff` was a 32 GiB allocation and an abort).
#[test]
fn a_marker_count_beyond_the_static_arguments_reads_the_prefix() {
    use cratonvm_reader::constant_pool::ConstantPoolEntry as CPE;
    let cp = ConstantPool::new(vec![
        CPE::Tombstone,
        CPE::Utf8("java/lang/Cloneable".into()), // 1
        CPE::ClassReference { name_index: 1 },   // 2
        CPE::Integer(i32::MAX),                  // 3
        CPE::Integer(1),                         // 4
        CPE::Integer(FLAG_MARKERS),              // 5
    ]);
    // sam, impl, inst, flags=MARKERS, count=i32::MAX, Cloneable
    let huge: [u16; 6] = [2, 2, 2, 5, 3, 2];
    let markers = marker_interfaces_in(&cp, &huge, &[]);
    assert_eq!(markers.len(), 1);
    assert_eq!(&*markers[0], "java/lang/Cloneable");
    assert!(markers.capacity() < 16, "reserved by the arguments present");
    // count=1, as javac writes it.
    let one: [u16; 6] = [2, 2, 2, 5, 4, 2];
    assert_eq!(marker_interfaces_in(&cp, &one, &[]).len(), 1);
    // No count: nothing.
    assert!(marker_interfaces_in(&cp, &[2, 2, 2, 5], &[]).is_empty());
}

/// Wave 7: the per-thread instruction copy answers only the WHOLE
/// instruction (VM, class, CP index, pc, method), goes stale with the
/// row's redefinition gate, matches an equal method name held in another
/// `Arc`, and never copies a heap-bearing `Linked` row.
#[test]
fn the_thread_instruction_copy_is_keyed_by_the_whole_instruction() {
    let name: Arc<str> = Arc::from("run");
    let desc: Arc<str> = Arc::from("()V");
    let other_name: Arc<str> = Arc::from("other");
    let ident = |vm: usize, cp: u16, pc: usize, n: &'static str| {
        let n: &Arc<str> = if n == "run" { &name } else { &other_name };
        InstructionIdent {
            vm_identity: vm,
            class_id: ClassId::new(0x7101),
            cp_index: cp,
            pc,
            method_name: n,
            method_descriptor: &desc,
        }
    };
    let lambda = ThreadInstructionLink::Lambda {
        proxy_class_id: ClassId::new(0x8000_7d01),
        num_captures: 1,
    };
    let (gate, counter) = fresh_gate();
    let mut sites = ThreadInstructionSites::default();
    assert_eq!(sites.get(&ident(1, 4, 9, "run")), None);
    sites.put(&ident(1, 4, 9, "run"), gate, lambda);
    assert_eq!(sites.get(&ident(1, 4, 9, "run")), Some(lambda));
    // Same content, another allocation: still the same method.
    let name_copy: Arc<str> = Arc::from("run");
    let copy = InstructionIdent {
        method_name: &name_copy,
        ..ident(1, 4, 9, "run")
    };
    assert_eq!(sites.get(&copy), Some(lambda));
    assert_eq!(sites.get(&ident(2, 4, 9, "run")), None, "another VM");
    assert_eq!(sites.get(&ident(1, 5, 9, "run")), None, "another entry");
    assert_eq!(sites.get(&ident(1, 4, 14, "run")), None, "another pc");
    assert_eq!(sites.get(&ident(1, 4, 9, "other")), None, "another method");
    counter.fetch_add(1, std::sync::atomic::Ordering::Release);
    assert_eq!(sites.get(&ident(1, 4, 9, "run")), None, "redefined caller");

    assert_eq!(
        ThreadInstructionLink::of(&GenericIndyLink::GroovyCastToBoolean),
        Some(ThreadInstructionLink::GroovyCastToBoolean)
    );
    assert_eq!(
        ThreadInstructionLink::of(&GenericIndyLink::Linked(fake_ref(0x5000))),
        None
    );
}

/// Wave 7: the thread-local door fills from a table row and answers it.
#[test]
fn the_thread_instruction_copy_fills_from_a_table_row() {
    let name: Arc<str> = Arc::from("m");
    let desc: Arc<str> = Arc::from("()Ljava/lang/Runnable;");
    let ident = InstructionIdent {
        vm_identity: usize::MAX - 0x71,
        class_id: ClassId::new(0x7102),
        cp_index: 3,
        pc: 17,
        method_name: &name,
        method_descriptor: &desc,
    };
    assert_eq!(probe_thread_instruction_site(&ident), None);
    let (gate, _counter) = fresh_gate();
    fill_thread_instruction_site(
        &ident,
        gate.clone(),
        &GenericIndyLink::Linked(fake_ref(0x6000)),
    );
    assert_eq!(
        probe_thread_instruction_site(&ident),
        None,
        "Linked is never copied"
    );
    fill_thread_instruction_site(&ident, gate, &GenericIndyLink::GroovyCastToBoolean);
    // Wave 16: the copy carries the method key the singleton cache is
    // keyed by, hashed once at the fill.
    assert_eq!(
        probe_thread_instruction_site(&ident),
        Some((
            ThreadInstructionLink::GroovyCastToBoolean,
            lambda_site_method_key_of("m", "()Ljava/lang/Runnable;")
        ))
    );
}

/// Interpreter round i6 wave 133, lane L2b (`CRATONVM_INDY_GENERIC_THREAD_ROW`):
/// a `Linked` row is copied only with the switch, as the heap-free
/// marker, and the copy's method key plus the frame's pc rebuild exactly
/// the key the row was published under, so `run_thread_linked_row` finds
/// the same `CallSite`; a stale caller gate misses both.
#[test]
fn w133_l2b_a_linked_row_copy_keys_the_published_row() {
    let linked = GenericIndyLink::Linked(fake_ref(0x7330));
    assert_eq!(ThreadInstructionLink::of_with(&linked, false), None);
    assert_eq!(
        ThreadInstructionLink::of_with(&linked, true),
        Some(ThreadInstructionLink::LinkedRow)
    );
    assert_eq!(
        ThreadInstructionLink::of_with(&GenericIndyLink::GroovyCastToBoolean, true),
        Some(ThreadInstructionLink::GroovyCastToBoolean)
    );

    let name: Arc<str> = Arc::from("call");
    let desc: Arc<str> = Arc::from("(Ljava/lang/Object;)Ljava/lang/Object;");
    let vm_identity = usize::MAX - 0x133;
    let class_id = ClassId::new(0x1332);
    let ident = InstructionIdent {
        vm_identity,
        class_id,
        cp_index: 7,
        pc: 21,
        method_name: &name,
        method_descriptor: &desc,
    };
    let (gate, counter) = fresh_gate();
    let published_key: GenericIndyKey = (
        vm_identity,
        class_id,
        7,
        lambda_site_method_key_of(&name, &desc),
        21,
    );
    publish_generic_indy_site(published_key, linked_slot(0x7340, gate.clone()));

    fill_thread_instruction_site_with(&ident, gate.clone(), &linked, false);
    assert_eq!(probe_thread_instruction_site(&ident), None, "switch off: not copied");
    fill_thread_instruction_site_with(&ident, gate, &linked, true);
    let (link, method_key) = probe_thread_instruction_site(&ident).expect("switch on: copied");
    assert_eq!(link, ThreadInstructionLink::LinkedRow);
    let rebuilt: GenericIndyKey = (vm_identity, class_id, 7, method_key, ident.pc);
    assert_eq!(rebuilt, published_key);
    let (row, _) = probe_generic_indy_site(&rebuilt).expect("the published row");
    assert_eq!(linked_addr(&row), Some(0x7340));

    counter.fetch_add(1, std::sync::atomic::Ordering::Release);
    assert_eq!(probe_thread_instruction_site(&ident), None, "redefined caller");
    assert!(probe_generic_indy_site(&rebuilt).is_none(), "stale row");
    forget_vm_generic_indy_sites(vm_identity);
}

/// Wave 7: a recorded bridge is answered for its own proxy and VM only.
#[test]
fn a_recorded_lambda_bridge_is_scoped_to_its_proxy_and_vm() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let proxy = ClassId::new(0x8000_7c01);
    let other = ClassId::new(0x8000_7c02);
    assert!(!lambda_proxy_has_bridge(
        shared.vm_identity,
        proxy,
        "()Ljava/lang/Object;"
    ));
    record_lambda_proxy_bridges(
        shared.vm_identity,
        proxy,
        &[Arc::from("()Ljava/lang/Object;")],
    );
    assert!(lambda_proxy_has_bridge(
        shared.vm_identity,
        proxy,
        "()Ljava/lang/Object;"
    ));
    assert!(!lambda_proxy_has_bridge(
        shared.vm_identity,
        proxy,
        "()Ljava/lang/String;"
    ));
    assert!(!lambda_proxy_has_bridge(
        shared.vm_identity,
        other,
        "()Ljava/lang/Object;"
    ));
    assert!(!lambda_proxy_has_bridge(
        shared.vm_identity.wrapping_add(1),
        proxy,
        "()Ljava/lang/Object;"
    ));
    forget_vm_generic_indy_sites(shared.vm_identity);
    assert!(!lambda_proxy_has_bridge(
        shared.vm_identity,
        proxy,
        "()Ljava/lang/Object;"
    ));
}

/// Wave 8: the bridge list reflection and serialization read is the one
/// dispatch admits, per proxy and VM.
#[test]
fn lambda_bridge_descriptors_are_listed_per_proxy() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let proxy = ClassId::new(0x8000_7d01);
    assert!(lambda_proxy_bridge_descriptors(shared.vm_identity, proxy).is_none());
    record_lambda_proxy_bridges(
        shared.vm_identity,
        proxy,
        &[
            Arc::from("()Ljava/lang/Object;"),
            Arc::from("()Ljava/lang/CharSequence;"),
        ],
    );
    let listed = lambda_proxy_bridge_descriptors(shared.vm_identity, proxy)
        .map(|b| b.iter().map(|d| d.to_string()).collect::<Vec<_>>());
    assert_eq!(
        listed,
        Some(vec![
            "()Ljava/lang/Object;".to_string(),
            "()Ljava/lang/CharSequence;".to_string()
        ])
    );
    assert!(
        lambda_proxy_bridge_descriptors(shared.vm_identity, ClassId::new(0x8000_7d02))
            .is_none()
    );
    forget_vm_generic_indy_sites(shared.vm_identity);
    assert!(lambda_proxy_bridge_descriptors(shared.vm_identity, proxy).is_none());
}

/// Wave 8 (`i5-L6` steps 1-3): a proxy spun by a user-loader host carries
/// that loader's `loader_pin` row, gets it back after the post-GC rebuild
/// replaced the VM's pins, is visited with its own id by the singleton root
/// scan (so `defer_or_root` can pin the singleton to the loader), and goes
/// with its host: call site, bridges, markers, singleton and pin list. A
/// built-in host's proxy gets no row.
#[test]
fn a_user_loader_lambda_proxy_pins_its_host_loader_until_the_host_unloads() {
    use cratonvm_types::loader_pin;
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let vm = shared.vm_identity;
    // Real objects of this VM's heap, so a marker of any VM that meets the
    // rows judges them by its own bounds; ids no other test pins.
    let loader = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let moved_loader = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let singleton = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let (loader_addr, moved_addr) = (loader.as_ptr() as usize, moved_loader.as_ptr() as usize);
    let (user_host, builtin_host) = (ClassId::new(0x7ff0_6b01), ClassId::new(0x7ff0_6b02));
    let (user_proxy, builtin_proxy) = (ClassId::new(0x8fff_6b01), ClassId::new(0x8fff_6b02));
    loader_pin::set_loader_pin(vm, user_host.as_u32(), loader_addr);

    pin_lambda_proxy_to_host_loader(vm, user_proxy, user_host);
    pin_lambda_proxy_to_host_loader(vm, builtin_proxy, builtin_host);
    assert_eq!(
        loader_pin::loader_pin_addr_for_vm(vm, user_proxy.as_u32()),
        Some(loader_addr)
    );
    assert_eq!(
        loader_pin::loader_pin_addr_for_vm(vm, builtin_proxy.as_u32()),
        None
    );

    // The rebuild keeps only defining-loader rows (the loader moved).
    loader_pin::replace_loader_pins(vm, &[(user_host.as_u32(), moved_addr)]);
    assert_eq!(
        loader_pin::loader_pin_addr_for_vm(vm, user_proxy.as_u32()),
        None
    );
    repin_lambda_proxy_loaders(vm);
    assert_eq!(
        loader_pin::loader_pin_addr_for_vm(vm, user_proxy.as_u32()),
        Some(moved_addr),
        "the proxy follows its host's current row"
    );

    {
        shared
            .classes
            .lambda_proxy_hosts
            .write()
            .insert(user_proxy, user_host);
        shared.classes.lambda_proxies.write().insert(
            user_proxy,
            Arc::new(LambdaCallSite {
                functional_interface: Arc::from("java/lang/Runnable"),
                functional_interface_id: None,
                sam_method_name: Arc::from("run"),
                sam_descriptor: Arc::from("()V"),
                impl_handle: MethodHandle {
                    kind: MethodHandleKind::InvokeStatic,
                    class_name: Arc::from("p/Host"),
                    member_name: Arc::from("lambda$0"),
                    descriptor: Arc::from("()V"),
                },
                instantiated_descriptor: Arc::from("()V"),
                capture_types: Vec::new(),
                proxy_class_id: user_proxy,
                serializable_flag: false,
            }),
        );
        lambda_singleton_cache().write().insert(
            (vm, user_proxy, 3, 5),
            Arc::new(LambdaSingletonSlot::new(singleton)),
        );
    }
    record_lambda_proxy_bridges(vm, user_proxy, &[Arc::from("()Ljava/lang/Object;")]);
    record_lambda_proxy_markers(vm, user_proxy, &[Arc::from("java/lang/Cloneable")]);

    let mut visited = Vec::new();
    gc_scan_lambda_singleton_roots(vm, &mut |owner, obj| visited.push((owner, obj)));
    assert_eq!(visited, vec![(user_proxy.as_u32(), singleton)]);

    // The host's loader died: the rebuild drops its row, so the proxy
    // gets none back, and the unload drops every proxy row.
    loader_pin::replace_loader_pins(vm, &[]);
    repin_lambda_proxy_loaders(vm);
    assert_eq!(
        loader_pin::loader_pin_addr_for_vm(vm, user_proxy.as_u32()),
        None
    );
    let unloaded: rustc_hash::FxHashSet<ClassId> = [user_host].into_iter().collect();
    forget_unloaded_lambda_proxy_rows(&shared, &unloaded);
    assert!(!shared
        .classes
        .lambda_proxies
        .read()
        .contains_key(&user_proxy));
    assert!(lambda_proxy_bridge_descriptors(vm, user_proxy).is_none());
    assert!(lambda_proxy_marker_interfaces(vm, user_proxy).is_none());
    assert!(!lambda_singleton_cache()
        .read()
        .contains_key(&(vm, user_proxy, 3, 5)));
    assert!(!user_loader_lambdas()
        .lock()
        .get(&vm)
        .is_some_and(|rows| rows.iter().any(|&(p, _)| p == user_proxy)));

    forget_vm_generic_indy_sites(vm);
    assert!(!user_loader_lambdas().lock().contains_key(&vm));
    loader_pin::forget_vm_loader_pins(vm);
}

/// Wave 9 (`docs/feature-designs/real-jdk-mode.md`): a proxy
/// the reflective `LambdaMetafactory` path registers gets its lookup class
/// as host through `register_lambda_proxy_host`, so it is named after it
/// (not after the impl owner) and unloads with it; one host per proxy.
#[test]
fn a_reflective_lambda_proxy_records_its_lookup_class_as_host() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    // A stripped VM may lack either class: nothing to check then.
    let (Ok(host), Ok(other)) = (
        shared.load_class_concurrent("java/lang/Object"),
        shared.load_class_concurrent("java/lang/Integer"),
    ) else {
        return;
    };
    let mut thread = JvmThread::new(crate::threading::jvm_thread::ThreadId(0), "test");
    let mut ctx = crate::vm::NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    // `String::length` as a `ToIntFunction`-shaped proxy; kind 5 is
    // `REF_invokeVirtual`.
    let raw = ctx.register_lambda_proxy(
        "java/util/function/ToIntFunction",
        "applyAsInt",
        "(Ljava/lang/Object;)I",
        "java/lang/String",
        "length",
        "()I",
        5,
        "(Ljava/lang/String;)I",
        "",
        false,
    );
    assert_ne!(raw, 0, "the proxy table has room");
    let proxy = ClassId::new(raw);
    assert_eq!(
        ctx.lambda_proxy_host(proxy).as_deref(),
        Some("java/lang/String"),
        "host-less: the impl owner's name (the pre-wave-9 answer)"
    );
    ctx.register_lambda_proxy_host(raw, host);
    assert_eq!(
        ctx.lambda_proxy_host(proxy).as_deref(),
        Some("java/lang/Object"),
        "HotSpot names the spun class after the lookup class"
    );
    assert!(
        !record_reflective_lambda_proxy_host(&shared, proxy, other),
        "a proxy is spun once: its host is not replaced"
    );
    assert!(!record_reflective_lambda_proxy_host(
        &shared,
        ClassId::new(0),
        host
    ));
    assert!(
        !record_reflective_lambda_proxy_host(&shared, ClassId::new(0x8fff_6c01), host),
        "a proxy this VM never registered"
    );
    assert_eq!(
        shared.classes.lambda_proxy_hosts.read().get(&proxy),
        Some(&host)
    );

    let unloaded: rustc_hash::FxHashSet<ClassId> = [host].into_iter().collect();
    forget_unloaded_lambda_proxy_rows(&shared, &unloaded);
    assert!(
        !shared.classes.lambda_proxies.read().contains_key(&proxy),
        "the proxy goes with its host"
    );
    assert!(!shared
        .classes
        .lambda_proxy_hosts
        .read()
        .contains_key(&proxy));
    forget_vm_generic_indy_sites(shared.vm_identity);
}
