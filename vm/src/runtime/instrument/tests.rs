use super::*;

/// gci i12/td: `jcmd Thread.dump_to_file` calls the `ThreadDumper` method
/// HotSpot 25's `ThreadDumpToFileDCmd` calls, with its descriptor
/// (`vmSymbols::string_bool_byte_array_signature`).
#[test]
fn gci_i12td_thread_dump_calls_hotspots_thread_dumper_method() {
    assert_eq!(thread_dumper_method(false), "dumpThreads");
    assert_eq!(thread_dumper_method(true), "dumpThreadsToJson");
    let src = include_str!("../instrument.rs").replace("\r\n", "\n");
    assert!(src.contains(
        "ctx.invoke(\"jdk/internal/vm/ThreadDumper\", method, \"(Ljava/lang/String;Z)[B\", &args)"
    ));
}

/// Interpreter round i2 wave 62, lane L1: the modelled HotSpot instance
/// sizes of `L1W62GetObjectSizeShapes`' classes, the numbers HotSpot
/// 25.0.3 answers (MEASURED). Each class's field tags, `Object` first.
#[test]
fn hotspot_instance_sizes_match_the_measured_shapes() {
    let size = |chain: &[&[u8]]| {
        let chain: Vec<Vec<u8>> = chain
            .iter()
            .map(|tags| tags.iter().map(|&t| hotspot_field_size(t)).collect())
            .collect();
        hotspot_instance_size(&chain)
    };
    let object: &[u8] = b"";
    assert_eq!(size(&[object]), 16, "Object");
    assert_eq!(size(&[object, b""]), 16, "Empty");
    assert_eq!(size(&[object, b"L"]), 16, "OneRef");
    assert_eq!(size(&[object, b"LL"]), 24, "TwoRefs");
    assert_eq!(size(&[object, b"LLL"]), 24, "ThreeRefs");
    assert_eq!(size(&[object, b"I"]), 16, "OneInt");
    assert_eq!(size(&[object, b"J"]), 24, "OneLong");
    assert_eq!(size(&[object, b"JL"]), 24, "LongAndRef");
    assert_eq!(size(&[object, b"BL"]), 24, "ByteAndRef");
    assert_eq!(size(&[object, b"I", b"I"]), 24, "IntSub");
    assert_eq!(size(&[object, b"L", b"L"]), 24, "RefSub");
    assert_eq!(size(&[object, b"ZBCSIFJDL"]), 48, "Mixed");
    // AbstractMap (keySet, values), HashMap (table, entrySet, size,
    // modCount, threshold, loadFactor).
    assert_eq!(size(&[object, b"LL", b"LLIIIF"]), 48, "HashMap");
    // String (value, coder, hash, hashIsZero) and its injected `flags`.
    assert_eq!(size(&[object, b"LBIZB"]), 24, "String");
    // Throwable (backtrace, detailMessage, cause, stackTrace, depth,
    // suppressedExceptions); InternalError's injected boolean.
    assert_eq!(size(&[object, b"LLLLIL"]), 40, "Throwable");
    assert_eq!(
        size(&[object, b"LLLLIL", b"", b"", b"Z"]),
        40,
        "InternalError"
    );
    // Thread: eetop, tid, name, interrupted, contextClassLoader, holder,
    // threadLocals, inheritableThreadLocals, scopedValueBindings,
    // interruptLock, parkBlocker, nioBlocker, cont,
    // uncaughtExceptionHandler, threadLocalRandomSeed, two ints,
    // container, headStackableScopes; and the four injected fields.
    let thread: &[u8] = b"JJLZLLLLLLLLLLJIILLJIZS";
    assert_eq!(size(&[object, thread]), 112, "Thread");
    // Mirrors: 120 bytes, then the mirrored class's static fields.
    let mirror = |tags: &[u8]| {
        let statics: Vec<u8> = tags.iter().map(|&t| hotspot_field_size(t)).collect();
        hotspot_layout_size(CLASS_MIRROR_STATICS_OFFSET, &[statics])
    };
    assert_eq!(mirror(b""), 120, "Object.class");
    assert_eq!(mirror(b"I"), 128, "StaticInt.class");
    assert_eq!(mirror(b"L"), 128, "StaticRef.class");
    assert_eq!(mirror(b"J"), 128, "StaticLong.class");
    assert_eq!(mirror(b"BJ"), 136, "StaticByteAndLong.class");
    assert_eq!(mirror(b"LILSD"), 144, "StaticMixed.class");
    assert_eq!(align_to_object(16 + 3 * 4), 32, "Object[3]");
    assert_eq!(align_to_object(16), 16, "Object[0]");
}

/// Interpreter round i2 wave 56, lane L1: `VirtualMachine.attach`'s id
/// checks, HotSpot 25's answers (measured, `L1W56AttachRefusals`).
#[test]
fn attach_refuses_what_hotspot_refuses() {
    const NS: &str = "com/sun/tools/attach/AttachNotSupportedException";
    let own = 4242u32;
    let refusal = |id: &str, allow: Option<&str>| attach_refusal(id, own, allow, false);
    let current = Some((
        "java/io/IOException",
        "Can not attach to current VM".to_string(),
    ));
    for allow in [None, Some("false"), Some("no")] {
        assert_eq!(refusal("4242", allow), current, "{allow:?}");
        assert_eq!(refusal("0", allow), current, "{allow:?}");
    }
    for allow in [Some("true"), Some("TRUE"), Some("")] {
        assert_eq!(refusal("4242", allow), None, "{allow:?}");
        assert_eq!(refusal("+4242", allow), None, "{allow:?}");
        assert_eq!(
            refusal("0", allow),
            Some((NS, "Invalid process identifier".to_string()))
        );
    }
    assert_eq!(
        refusal("abc", Some("true")),
        Some((NS, "Invalid process identifier: abc".to_string()))
    );
    assert_eq!(
        refusal("", None),
        Some((NS, "Invalid process identifier: ".to_string()))
    );
    assert_eq!(refusal("-7", Some("true")).map(|(c, _)| c), Some(NS));
    assert_eq!(
        refusal("99999999999", Some("true")).map(|(c, _)| c),
        Some(NS),
        "not an int"
    );
    assert_eq!(
        refusal("4243", Some("true")).map(|(c, _)| c),
        Some(NS),
        "another process"
    );
    assert_eq!(
        refusal("4243", None).map(|(c, _)| c),
        Some(NS),
        "another process"
    );
}

/// Interpreter round i2 wave 59, lane L1: under
/// `-XX:+DisableAttachMechanism` this process and pid 0 answer HotSpot's
/// `AttachNotSupportedException` whatever `jdk.attach.allowAttachSelf`
/// says, after the id's own check (measured, `L1W59DisableAttachMechanism`).
#[test]
fn a_disabled_attach_mechanism_refuses_as_hotspot_does() {
    const NS: &str = "com/sun/tools/attach/AttachNotSupportedException";
    let disabled = Some((
        NS,
        "The VM does not support the attach mechanism".to_string(),
    ));
    for allow in [None, Some("true"), Some("false")] {
        assert_eq!(
            attach_refusal("4242", 4242, allow, true),
            disabled,
            "{allow:?}"
        );
        assert_eq!(
            attach_refusal("0", 4242, allow, true),
            disabled,
            "{allow:?}"
        );
        assert_eq!(
            attach_refusal("abc", 4242, allow, true),
            Some((NS, "Invalid process identifier: abc".to_string()))
        );
    }
    assert_eq!(attach_refusal("4242", 4242, Some("true"), false), None);
}

/// Interpreter round i3 wave 81, lane L1: a `load` reply body read as
/// JDK 25's `HotSpotVirtualMachine.loadAgent` reads it, and the JPLIS
/// option string it sends.
#[test]
fn a_jar_load_reply_maps_to_hotspots_client_exceptions() {
    const LOAD: &str = "com/sun/tools/attach/AgentLoadException";
    const INIT: &str = "com/sun/tools/attach/AgentInitializationException";
    assert_eq!(jar_load_reply_exception("return code: 0\n"), None);
    assert_eq!(jar_load_reply_exception("return code: 0"), None);
    assert_eq!(
        jar_load_reply_exception("return code: 100\n"),
        Some((
            LOAD,
            "Agent JAR not found or no Agent-Class attribute".to_string()
        ))
    );
    assert_eq!(
        jar_load_reply_exception("return code: 102\n"),
        Some((
            INIT,
            "Agent JAR loaded but agent failed to initialize".to_string()
        ))
    );
    assert_eq!(
        jar_load_reply_exception("return code: 7\n"),
        Some((LOAD, "Failed to load agent - unknown reason: 7".to_string()))
    );
    assert_eq!(
        jar_load_reply_exception(""),
        Some((LOAD, "Target VM did not respond".to_string()))
    );
    assert_eq!(
        jar_load_reply_exception("libx was not loaded.\nno such file\n"),
        Some((
            LOAD,
            "Failed to load agent library: libx was not loaded. no such file".to_string()
        ))
    );
    assert_eq!(jplis_attach_options("/d/a.jar", None), "/d/a.jar");
    assert_eq!(jplis_attach_options("/d/a.jar", Some("")), "/d/a.jar=");
    assert_eq!(
        jplis_attach_options("/d/a.jar", Some("x=y")),
        "/d/a.jar=x=y"
    );
    assert!(DYNAMIC_AGENT_LOADING_DISABLED.starts_with("Dynamic agent loading is not enabled."));
    assert_eq!(JPLIS_START_FAILURE_LINES[3], "Agent failed to start!");
}

fn fake_objref(addr: usize) -> ObjectRef {
    // Build a dummy ObjectRef from a raw addr. Only used inside
    // unit tests where we never dereference it.
    unsafe { std::mem::transmute::<usize, ObjectRef>(addr) }
}

// ---- GC-safety of the two agent-array loops (2026-09-06) ----

/// The body of `fn <name>(` in THIS file, from the declaration to the
/// first `\n}` in column 0 after it.
///
/// Line endings are normalised first: `include_str!` embeds the file's raw
/// bytes and this repository is checked out with CRLF on Windows, where
/// every `contains` below would still work but the byte offsets used to
/// order two needles would not.
fn fn_body(name: &str) -> String {
    let src = include_str!("../instrument.rs").replace("\r\n", "\n");
    let start = src
        .find(&format!("fn {name}("))
        .unwrap_or_else(|| panic!("{name} not found in instrument.rs"));
    let end = src[start..]
        .find("\n}")
        .map_or(src.len(), |i| start + i + 2);
    src[start..end].to_string()
}

/// Both loops walk an agent-supplied array while running agent bytecode
/// and defining classes — that allocates, so a moving collector relocates
/// the array mid-loop. Measured 2026-09-06: it moved before `i=1` in
/// **60 of 60** processes, and roughly one in eight read a zero word out
/// of the vacated address, silently dropped the rest of the hierarchy, and
/// handed Mockito a mock that did not intercept.
///
/// A source witness rather than a behavioural test because reproducing it
/// needs a real agent, a real GC and luck: every in-process instrument
/// that tried to observe it changed the allocation pattern and hid it. So
/// this asserts the SHAPE that makes it impossible — pin, then re-read
/// before every element access — and it fails if an edit drops either.
#[test]
fn the_agent_array_loops_pin_and_refresh_before_every_element_read() {
    for name in ["native_retransform_classes0", "native_redefine_classes0"] {
        let body = fn_body(name);
        assert!(
            body.contains("pin_native_root(arr)"),
            "{name} must pin the agent array before the loop"
        );
        assert!(
            body.contains("unpin_native_roots(arr_pin)"),
            "{name} must release the array pin"
        );
        let refresh = body
            .find("read_native_pin(arr_pin")
            .unwrap_or_else(|| panic!("{name} never re-reads the array from its pin"));
        let read = body
            .find("get_array_element(arr, i)")
            .unwrap_or_else(|| panic!("{name} no longer reads the array by index"));
        assert!(
            refresh < read,
            "{name} reads element i BEFORE refreshing `arr` from its pin — that is \
             exactly the stale read this pin exists to stop"
        );
    }
}

/// The class reference read out of that array is itself carried across
/// the transformer chain, so it needs the same treatment. Asserted
/// separately because the array pin alone does not cover it: a relocation
/// between the element read and the chain call moves the mirror, not the
/// array.
///
/// Both loops, because they are twins and the twin is where a fix stops
/// being applied — `native_redefine_classes0` reaches the same chain with
/// the same shape and had the same hazard.
#[test]
fn both_loops_refresh_the_class_reference_before_running_the_chain() {
    for (name, pin_call, refresh_call) in [
        (
            "native_retransform_classes0",
            "pin_native_root(mirror)",
            "read_native_pin(mirror_pin, mirror)",
        ),
        (
            "native_redefine_classes0",
            "pin_native_root(target_class)",
            "read_native_pin(class_pin, target_class)",
        ),
    ] {
        let body = fn_body(name);
        let pin = body
            .find(pin_call)
            .unwrap_or_else(|| panic!("{name}: the class reference must be pinned"));
        let refresh = body.find(refresh_call).unwrap_or_else(|| {
            panic!("{name}: the class reference must be re-read from its pin")
        });
        let chain = body
            .find("run_transformer_chain(")
            .unwrap_or_else(|| panic!("{name}: the transformer chain call is gone"));
        assert!(
            pin < refresh && refresh < chain,
            "{name}: pin, then refresh, then hand to the chain \
             (pin={pin} refresh={refresh} chain={chain})"
        );
    }
}

/// The chain walk holds a SNAPSHOT of the transformer list across every
/// `transform` call in it, and each of those runs Java. The canonical
/// chain is a GC root and is remapped; the snapshot is not — see
/// `a_snapshot_taken_before_a_relocation_keeps_the_old_address`. So each
/// ref must be pinned and read back, and the `invoke_virtual` must use the
/// value that came out of the pin rather than the one in the snapshot.
///
/// One registered transformer hides this completely, which is why it has
/// to be a witness: the common case (Mockito alone) never has a second
/// entry to dispatch on.
#[test]
fn the_chain_walk_dispatches_on_a_pinned_transformer_not_the_snapshot() {
    let body = fn_body("run_chain_over_bytes");
    let pin = body
        .find("pin_native_root(entry.transformer_ref)")
        .expect("every transformer ref in the snapshot must be pinned");
    let read = body
        .find("read_native_pin(transformer_pins[slot], entry.transformer_ref)")
        .expect("each transformer ref must be read back through its pin");
    let call = body
        .find("invoke_virtual(transformer,")
        .expect("the dispatch must use the value read out of the pin");
    assert!(
        pin < read && read < call,
        "pin, then read back, then dispatch (pin={pin} read={read} call={call})"
    );
    assert!(
        !body.contains("invoke_virtual(entry.transformer_ref"),
        "the dispatch must not use the snapshot's raw ref"
    );
}

/// JVMTI `RetransformClasses` redefines each class ONCE, with the chain's
/// output; the class keeps its current version while the transformers run
/// on the base bytes. The swap back to the base that preceded the chain
/// until interpreter round i1 wave 26 made every retransform two
/// redefinitions (probe `L3/L3W25RetransformRunsTheBase`; see
/// `interpreter-L3-retransform-redefines-each-class-twice-FIXED-20260928.md`
/// before bringing it back).
#[test]
fn a_retransform_redefines_each_class_once() {
    let body = fn_body("native_retransform_classes0");
    // Since wave 27 the classes are installed together, after every
    // chain ran and every class was checked (`install_redefinitions`).
    assert!(
        !body.contains("retransform_class(") && !body.contains("redefine_class("),
        "the native redefines only through install_redefinitions"
    );
    assert_eq!(
        body.matches("install_redefinitions(").count(),
        1,
        "one install per retransform"
    );
    let chain = body
        .find("run_transformer_chain(")
        .expect("the transformer chain call is gone");
    let install = body
        .find("install_redefinitions(")
        .expect("the install call is gone");
    assert!(chain < install, "the chain runs before the only install");
    let install_body = fn_body("install_redefinitions");
    assert_eq!(
        install_body
            .matches("redefine_class_as_instrument(")
            .count(),
        1,
        "one redefinition per class"
    );
}

/// A refused redefinition throws what HotSpot throws (interpreter round
/// i1 wave 27, lane L3; `L3/L3W27RedefinitionRefusals`): every class is
/// checked before the first is redefined, and the throwable is built only
/// after the batch closed and the array pin was released, so nothing the
/// batch's handshake could move is held across it.
#[test]
fn both_natives_check_every_class_first_and_throw_the_refusal() {
    let install = fn_body("install_redefinitions");
    let check = install
        .find("check_class_redefinition(")
        .expect("install_redefinitions must check every class first");
    let redefine = install
        .find("redefine_class_as_instrument(")
        .expect("install_redefinitions must redefine");
    assert!(check < redefine, "check every class, then redefine");
    for name in ["native_retransform_classes0", "native_redefine_classes0"] {
        let body = fn_body(name);
        let end = body
            .find("end_redefinition_batch(batch)")
            .unwrap_or_else(|| panic!("{name}: the batch is never closed"));
        let unpin = body
            .find("unpin_native_roots(arr_pin)")
            .unwrap_or_else(|| panic!("{name}: the array pin is never released"));
        // The LAST throw: the first is the unmodifiable-class refusal,
        // taken before anything is pinned or opened.
        let throw = body
            .rfind("throw_redefinition_refusal(")
            .unwrap_or_else(|| panic!("{name}: a refusal is not thrown"));
        let unmodifiable = body
            .find("first_unmodifiable_class(")
            .unwrap_or_else(|| panic!("{name}: unmodifiable classes are not refused"));
        let pin = body
            .find("pin_native_root(arr)")
            .unwrap_or_else(|| panic!("{name}: the array is never pinned"));
        assert!(
            unmodifiable < pin,
            "{name}: refuse an unmodifiable class before pinning or running any chain"
        );
        assert!(
            end < throw && unpin < throw,
            "{name}: build the throwable after closing the batch and unpinning"
        );
        assert!(
            !body.contains("tracing::warn!(\"retransformClasses0: {msg}\")")
                && !body.contains("tracing::warn!(\"redefineClasses0: {msg}\")"),
            "{name}: a refusal is thrown, not only logged"
        );
    }
}

/// gci i19/rv (`gci-i19rv-i18-default-path-review-findings-FIXED-20261014`
/// finding 1): a heap-exhaustion unwind out of the install loop (the JVMTI
/// contexts run under `native_oom::catch_alloc_oom_on` since gci i18/na,
/// the natives under the funnel's scope) still lowers the install fence
/// and releases the publication hold, and the JVMTI redefinition still
/// closes its batch; the payload is re-raised afterwards.
#[test]
fn gci_i19rv_the_install_fence_and_the_batch_close_on_an_unwind() {
    let install = fn_body("install_redefinitions");
    let raised = install
        .find("let held = ctx.begin_redefinition_install(&classes);")
        .expect("the fence is raised once");
    let caught = install
        .find("std::panic::catch_unwind(std::panic::AssertUnwindSafe(||")
        .expect("the install loop runs under a catch");
    let redefine = install
        .find("redefine_class_as_instrument(")
        .expect("the install loop redefines");
    let lowered = install
        .find("ctx.end_redefinition_install(held);")
        .expect("the fence is lowered");
    let resumed = install
        .find("std::panic::resume_unwind(payload)")
        .expect("the unwind is re-raised");
    assert!(raised < caught && caught < redefine && redefine < lowered && lowered < resumed);
    assert_eq!(install.matches("end_redefinition_install(").count(), 1);

    for name in ["redefine_classes_for_jvmti", "retransform_classes_for_jvmti"] {
        let jvmti = fn_body(name);
        let opened = jvmti
            .find("let batch = ctx.begin_redefinition_batch();")
            .unwrap_or_else(|| panic!("{name}: the batch is opened"));
        let caught = jvmti
            .find("std::panic::catch_unwind(std::panic::AssertUnwindSafe(||")
            .unwrap_or_else(|| panic!("{name}: the chains and the install run under a catch"));
        let chain = jvmti
            .find("run_transformer_chain(")
            .unwrap_or_else(|| panic!("{name}: the chain runs"));
        let installs = jvmti
            .find("install_redefinitions(")
            .unwrap_or_else(|| panic!("{name}: the install runs"));
        let closed = jvmti
            .find("ctx.end_redefinition_batch(batch);")
            .unwrap_or_else(|| panic!("{name}: the batch is closed"));
        let resumed = jvmti
            .find("std::panic::resume_unwind(payload)")
            .unwrap_or_else(|| panic!("{name}: the unwind is re-raised"));
        assert!(opened < caught && caught < chain && chain < installs, "{name}");
        assert!(installs < closed && closed < resumed, "{name}");
    }
}

/// gci i19/rv finding 3: the two Java natives close their batch and
/// release the array pin on an unwind out of a chain or the install too,
/// then re-raise it.
#[test]
fn gci_i19rv_the_java_natives_close_their_batch_on_an_unwind() {
    for name in ["native_redefine_classes0", "native_retransform_classes0"] {
        let body = fn_body(name);
        let opened = body
            .find("let batch = ctx.begin_redefinition_batch();")
            .unwrap_or_else(|| panic!("{name}: the batch is opened"));
        let caught = body
            .find("std::panic::catch_unwind(std::panic::AssertUnwindSafe(||")
            .unwrap_or_else(|| panic!("{name}: the chains and the install run under a catch"));
        let chain = body
            .find("run_transformer_chain(")
            .unwrap_or_else(|| panic!("{name}: the chain runs"));
        let installs = body
            .find("install_redefinitions(")
            .unwrap_or_else(|| panic!("{name}: the install runs"));
        let closed = body
            .find("ctx.end_redefinition_batch(batch);")
            .unwrap_or_else(|| panic!("{name}: the batch is closed"));
        let unpinned = body
            .find("ctx.unpin_native_roots(arr_pin);")
            .unwrap_or_else(|| panic!("{name}: the array pin is released"));
        let resumed = body
            .find("std::panic::resume_unwind(payload)")
            .unwrap_or_else(|| panic!("{name}: the unwind is re-raised"));
        assert!(opened < caught && caught < chain && chain < installs, "{name}");
        assert!(installs < closed && closed < unpinned && unpinned < resumed, "{name}");
    }
}

// ---- Observability audit (2026-07-26): retransform seed validation ----

/// Build a minimal, well-formed class file whose only constant-pool
/// entries are the `this_class` Class entry and its Utf8 name.
fn minimal_class_file(name: &str) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]); // magic
    b.extend_from_slice(&0u16.to_be_bytes()); // minor
    b.extend_from_slice(&52u16.to_be_bytes()); // major
    b.extend_from_slice(&3u16.to_be_bytes()); // cp_count (indices 1..2)
                                              // #1: CONSTANT_Class -> name_index 2
    b.push(7);
    b.extend_from_slice(&2u16.to_be_bytes());
    // #2: CONSTANT_Utf8
    b.push(1);
    b.extend_from_slice(&(name.len() as u16).to_be_bytes());
    b.extend_from_slice(name.as_bytes());
    // access_flags, this_class
    b.extend_from_slice(&0x0021u16.to_be_bytes());
    b.extend_from_slice(&1u16.to_be_bytes());
    b
}

#[test]
fn obsaudit_this_class_extracted_from_minimal_class_file() {
    let bytes = minimal_class_file("com/foo/Bar");
    assert_eq!(
        class_file_this_class(&bytes).as_deref(),
        Some("com/foo/Bar")
    );
}

/// Long/Double constant-pool entries occupy two indices. If the walker
/// got that wrong every subsequent offset would shift and `this_class`
/// would resolve to the wrong entry — silently, since the result is still
/// a plausible string.
#[test]
fn obsaudit_this_class_handles_long_double_double_slots() {
    let name = "a/B";
    let mut b = Vec::new();
    b.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&52u16.to_be_bytes());
    // indices: 1 = Long (eats 1 and 2), 3 = Class, 4 = Utf8 => cp_count 5
    b.extend_from_slice(&5u16.to_be_bytes());
    b.push(5); // CONSTANT_Long
    b.extend_from_slice(&1234i64.to_be_bytes());
    b.push(7); // #3 CONSTANT_Class -> name_index 4
    b.extend_from_slice(&4u16.to_be_bytes());
    b.push(1); // #4 CONSTANT_Utf8
    b.extend_from_slice(&(name.len() as u16).to_be_bytes());
    b.extend_from_slice(name.as_bytes());
    b.extend_from_slice(&0x0021u16.to_be_bytes()); // access_flags
    b.extend_from_slice(&3u16.to_be_bytes()); // this_class = #3

    assert_eq!(class_file_this_class(&b).as_deref(), Some("a/B"));
}

#[test]
fn obsaudit_this_class_rejects_non_class_files() {
    assert_eq!(class_file_this_class(&[]), None);
    assert_eq!(class_file_this_class(b"PK\x03\x04not a class"), None);
    // Right magic, truncated before the constant pool.
    assert_eq!(
        class_file_this_class(&[0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52]),
        None
    );
    // Truncated mid-Utf8: length claims more bytes than are present.
    let mut short = minimal_class_file("com/foo/Bar");
    short.truncate(short.len() - 6);
    assert_eq!(class_file_this_class(&short), None);
}

// ---- load-time transform: supertype pre-stage walk ----

/// A class file with `this_class`, `super_class` and `interfaces`, built
/// from the same constant-pool shape `minimal_class_file` uses.
///
/// Each name gets a `CONSTANT_Class` + `CONSTANT_Utf8` pair, in order, so
/// the pool indices are `1,2` for the first name, `3,4` for the second, and
/// so on.
fn class_file_with_hierarchy(this: &str, super_name: &str, interfaces: &[&str]) -> Vec<u8> {
    let names: Vec<&str> = std::iter::once(this)
        .chain(std::iter::once(super_name))
        .chain(interfaces.iter().copied())
        .collect();
    let mut b = Vec::new();
    b.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&65u16.to_be_bytes());
    // cp_count is one past the last used index; two entries per name.
    b.extend_from_slice(&((names.len() as u16) * 2 + 1).to_be_bytes());
    for (i, name) in names.iter().enumerate() {
        // CONSTANT_Class at index 2i+1, pointing at the CONSTANT_Utf8 that
        // immediately follows it at index 2i+2.
        b.push(7);
        b.extend_from_slice(&((i as u16) * 2 + 2).to_be_bytes());
        b.push(1);
        b.extend_from_slice(&(name.len() as u16).to_be_bytes());
        b.extend_from_slice(name.as_bytes());
    }
    b.extend_from_slice(&0x0021u16.to_be_bytes()); // access_flags
    b.extend_from_slice(&1u16.to_be_bytes()); // this_class  = #1
    b.extend_from_slice(&3u16.to_be_bytes()); // super_class = #3
    b.extend_from_slice(&(interfaces.len() as u16).to_be_bytes());
    for i in 0..interfaces.len() {
        b.extend_from_slice(&((i as u16) * 2 + 5).to_be_bytes());
    }
    b
}

/// The supertype walk is what lets a transformer see the abstract base of a
/// class it is offered: supertypes are loaded from *inside*
/// `define_class_shared_with_options`, under the class-manager write lock,
/// where no Java transformer can run.
#[test]
fn supertypes_reports_superclass_and_every_interface() {
    let bytes = class_file_with_hierarchy(
        "com/app/Impl",
        "com/app/AbstractBase",
        &["com/app/Api", "java/io/Serializable"],
    );
    assert_eq!(
        class_file_this_class(&bytes).as_deref(),
        Some("com/app/Impl")
    );
    assert_eq!(
        class_file_supertypes(&bytes),
        vec![
            "com/app/AbstractBase".to_string(),
            "com/app/Api".to_string(),
            "java/io/Serializable".to_string(),
        ]
    );
}

/// `super_class == 0` is legal — it is what `java/lang/Object`'s own class
/// file carries — and must not be reported as a supertype named "".
#[test]
fn supertypes_treats_a_zero_super_class_as_no_supertype() {
    let mut bytes = class_file_with_hierarchy("java/lang/Object", "unused/Placeholder", &[]);
    // Overwrite super_class (the u2 after magic..cp, access_flags,
    // this_class) with 0. Locate it by walking back from the tail: the
    // trailer is access_flags(2) + this_class(2) + super_class(2) +
    // interfaces_count(2) with no interfaces.
    let len = bytes.len();
    bytes[len - 4..len - 2].copy_from_slice(&0u16.to_be_bytes());
    assert!(class_file_supertypes(&bytes).is_empty());
}

/// Anything unparseable means "no supertypes to pre-stage" — a coverage
/// limit, never a failure that could break a class load.
#[test]
fn supertypes_of_garbage_is_empty_not_a_panic() {
    assert!(class_file_supertypes(&[]).is_empty());
    assert!(class_file_supertypes(b"PK\x03\x04not a class").is_empty());
    let mut truncated = class_file_with_hierarchy("a/B", "a/C", &[]);
    truncated.truncate(truncated.len() - 3);
    // Truncated inside the trailer: the walk must decline, not index past
    // the end.
    let _ = class_file_supertypes(&truncated);
}

/// The point of the extractor: a classpath resource that defines a
/// *different* class must be distinguishable from the right one, so
/// `original_class_bytes` can refuse to seed a retransform with it. The
/// scenario is a shaded jar (or a second class loader) carrying its own
/// `com/foo/Bar.class` while the live `com/foo/Bar` came from elsewhere
/// and has since been evicted from the 16 MiB class_bytes_cache.
#[test]
fn obsaudit_this_class_distinguishes_shadowed_resource() {
    let wanted = "com/foo/Bar";
    let shadowed = minimal_class_file("shaded/com/foo/Bar");
    let found = class_file_this_class(&shadowed).expect("parseable");
    assert_ne!(
        found, wanted,
        "a shadowed resource must not be mistaken for the requested class"
    );
}

// ---- Retransformation base adjudication --------------------------------
//
// `adjudicate_retransform_base` is the whole policy `original_class_bytes`
// applies once it has gathered its four inputs. Driving it directly is the
// only way to test the version-skew arm: producing the state for real needs
// two builds of one class, a user-defined loader, and 16 MiB of class bytes
// loaded in between to evict the cache.

/// A class file that is a *different build of the same class*: same
/// `this_class`, different bytes. Achieved by appending a trailing
/// attribute-shaped tail, which `class_file_this_class` (a header-only
/// walker) neither reads nor cares about — which is precisely why the name
/// check cannot tell the two apart.
fn other_build_of(name: &str) -> Vec<u8> {
    let mut b = minimal_class_file(name);
    b.extend_from_slice(&[0u8; 32]);
    b
}

#[test]
fn retransform_base_prefers_the_cached_bytes() {
    let cached = minimal_class_file("com/foo/Bar");
    let got = adjudicate_retransform_base(
        "com/foo/Bar",
        Some(cached.clone()),
        // A fallback is never even consulted when the cache hit — including
        // its digest verdict, which here says "wrong".
        Some(other_build_of("com/foo/Bar")),
        Some(false),
    );
    assert_eq!(got.as_deref(), Ok(&cached[..]));
}

#[test]
fn retransform_base_accepts_a_matching_classpath_fallback() {
    let bytes = minimal_class_file("com/foo/Bar");
    let got = adjudicate_retransform_base("com/foo/Bar", None, Some(bytes.clone()), Some(true));
    assert_eq!(got.as_deref(), Ok(&bytes[..]));
}

#[test]
fn retransform_base_accepts_a_fallback_the_vm_cannot_adjudicate() {
    // `None` = no fingerprint recorded (synthetic stub). Unchanged from the
    // pre-fingerprint behaviour: the name check is all we have, so use it.
    let bytes = minimal_class_file("com/foo/Bar");
    let got = adjudicate_retransform_base("com/foo/Bar", None, Some(bytes.clone()), None);
    assert_eq!(got.as_deref(), Ok(&bytes[..]));
}

#[test]
fn retransform_base_refuses_a_different_build_of_the_same_class() {
    // THE REGRESSION. Before the fingerprint check these bytes were
    // accepted — `this_class` says `com/foo/Bar` and that was the entire
    // test — and the transformer wove a class body that was then installed
    // over a live class it did not come from. The state arises whenever a
    // class is defined through a loader that does not resolve to the
    // application classpath (Spring Boot's `ModifiedClassPathClassLoader`,
    // `FilteredClassLoader`, per-test `URLClassLoader`s) while a different
    // build of the same coordinate sits on that classpath, and the 16 MiB
    // class-bytes cache has since evicted the real base.
    let got = adjudicate_retransform_base(
        "com/foo/Bar",
        None,
        Some(other_build_of("com/foo/Bar")),
        Some(false),
    );
    assert_eq!(
        got,
        Err(RetransformBaseRefusal::DifferentBuild(
            other_build_of("com/foo/Bar").len()
        )),
        "a same-named class file from a different build must not seed a retransform"
    );
}

#[test]
fn retransform_base_refuses_a_resource_defining_another_class() {
    let got = adjudicate_retransform_base(
        "com/foo/Bar",
        None,
        Some(minimal_class_file("shaded/com/foo/Bar")),
        Some(true),
    );
    assert_eq!(
        got,
        Err(RetransformBaseRefusal::DefinesOtherClass(
            "shaded/com/foo/Bar".to_string()
        ))
    );
}

#[test]
fn retransform_base_refuses_an_unparseable_resource() {
    let got = adjudicate_retransform_base(
        "com/foo/Bar",
        None,
        Some(b"PK\x03\x04 this is a jar, not a class".to_vec()),
        None,
    );
    assert_eq!(got, Err(RetransformBaseRefusal::Unparseable));
}

#[test]
fn retransform_base_refuses_when_nothing_was_found() {
    // The arm that used to be a silent `Vec::new()` the CALLER then handed
    // to every registered transformer as a zero-length `byte[]`. Nothing
    // here can prove the caller now skips — `run_transformer_chain`'s own
    // empty-buffer guard does that — but the refusal must at least be
    // distinguishable from "here are your bytes".
    let got = adjudicate_retransform_base("com/foo/Bar", None, None, None);
    assert_eq!(got, Err(RetransformBaseRefusal::NotFound));
}

// -----------------------------------------------------------------------
// Transformer-chain tests
//
// Every test below uses its OWN `vm_identity`, so they are genuinely
// isolated from one another under `cargo test`'s parallel harness. They
// used to share one process-global chain and lean on
// `reset_transformer_chain()` at the top of each test for isolation, which
// only ever worked because no two of them happened to interleave.
// `unique_vm()` hands out a fresh identity per call; `fake_objref` refs are
// synthetic non-heap addresses, never dereferenced.
// -----------------------------------------------------------------------

/// A `vm_identity` no other test and no real VM can collide with.
///
/// Real identities come from `NEXT_VM_IDENTITY` (`vm/src/vm/vm_init.rs`),
/// a counter starting at 1 -- not `SharedVm` addresses. This counter used
/// to start at 1 too, so in this binary, where other test modules build
/// and drop real `SharedVm`s in parallel, test identity N WAS real VM N:
/// that VM's teardown (`release_vm_native_state` ->
/// `forget_vm_transformers(N)`) wiped this test's chain mid-assertion, and
/// while both lived that VM's class loads and root scans saw this test's
/// fake `0x1000`-style transformer refs (gc-common w25-c). The base is
/// far above any plausible allocation and distinct from the jvmti
/// modules' bases (`0x7000_0000`, `0x7100_0000`, `0x7200_0000`). gcg g2/k:
/// it now comes from the real counter itself (`vm_init::test_vm_identity`),
/// which no real VM is ever handed again, so no base is needed.
fn unique_vm() -> usize {
    crate::vm::vm_init::test_vm_identity()
}

#[test]
fn add_then_remove_transformer_roundtrip() {
    let vm = unique_vm();
    let t1 = fake_objref(0x1000);
    add_transformer_entry(
        vm,
        TransformerEntry {
            transformer_ref: t1,
            can_retransform: false,
            native_method_prefix: None,
        },
    );
    assert_eq!(transformer_count(vm), 1);
    let removed = remove_transformer_entry(vm, t1);
    assert!(removed);
    assert_eq!(transformer_count(vm), 0);
    forget_vm_transformers(vm);
}

#[test]
fn remove_unknown_transformer_returns_false() {
    let vm = unique_vm();
    let t = fake_objref(0x2000);
    assert!(!remove_transformer_entry(vm, t));
    forget_vm_transformers(vm);
}

#[test]
fn snapshot_preserves_order() {
    let vm = unique_vm();
    let t1 = fake_objref(0x1000);
    let t2 = fake_objref(0x2000);
    let t3 = fake_objref(0x3000);
    add_transformer_entry(
        vm,
        TransformerEntry {
            transformer_ref: t1,
            can_retransform: true,
            native_method_prefix: None,
        },
    );
    add_transformer_entry(
        vm,
        TransformerEntry {
            transformer_ref: t2,
            can_retransform: false,
            native_method_prefix: Some("$pfx".into()),
        },
    );
    add_transformer_entry(
        vm,
        TransformerEntry {
            transformer_ref: t3,
            can_retransform: true,
            native_method_prefix: None,
        },
    );
    let snap = snapshot_transformer_chain(vm);
    assert_eq!(snap.len(), 3);
    assert_eq!(snap[0].transformer_ref.as_ptr() as usize, 0x1000);
    assert_eq!(snap[1].transformer_ref.as_ptr() as usize, 0x2000);
    assert_eq!(snap[1].native_method_prefix.as_deref(), Some("$pfx"));
    assert_eq!(snap[2].transformer_ref.as_ptr() as usize, 0x3000);
    forget_vm_transformers(vm);
}

/// Interpreter round i7 wave 136, lane L3: a JDK-managed group entry holds
/// its group's whole prefix list, and the prefixes link in the order
/// non-retransformable first, each entry's list in its own order.
#[test]
fn a_group_entry_holds_its_whole_prefix_list() {
    assert_eq!(join_group_prefixes(&[]), None);
    assert_eq!(join_group_prefixes(&[None, Some(String::new())]), None);
    let vm = unique_vm();
    add_transformer_entry(
        vm,
        TransformerEntry {
            transformer_ref: fake_objref(0x1000),
            can_retransform: true,
            native_method_prefix: join_group_prefixes(&[
                Some("r1_".into()),
                None,
                Some("r2_".into()),
            ]),
        },
    );
    add_transformer_entry(
        vm,
        TransformerEntry {
            transformer_ref: fake_objref(0x2000),
            can_retransform: false,
            native_method_prefix: Some("n_".into()),
        },
    );
    assert_eq!(native_method_prefixes(vm), ["n_", "r1_", "r2_"]);
    forget_vm_transformers(vm);
}

/// Interpreter round i7 wave 138, lane L3: a removed VM-chain
/// transformer's prefix stays in its group's list until that group's next
/// `setNativeMethodPrefix`; the other group's removed prefixes stay.
#[test]
fn i138_l3_a_removed_transformers_prefix_outlives_it_until_the_next_set() {
    let vm = unique_vm();
    let plain = fake_objref(0x1000);
    let retransform = fake_objref(0x2000);
    add_transformer_entry(
        vm,
        TransformerEntry {
            transformer_ref: plain,
            can_retransform: false,
            native_method_prefix: Some("n_".into()),
        },
    );
    add_transformer_entry(
        vm,
        TransformerEntry {
            transformer_ref: retransform,
            can_retransform: true,
            native_method_prefix: Some("r_".into()),
        },
    );
    assert!(remove_transformer_entry(vm, plain));
    assert!(remove_transformer_entry(vm, retransform));
    assert_eq!(transformer_count(vm), 0);
    assert_eq!(native_method_prefixes(vm), ["n_", "r_"]);
    forget_removed_prefixes(vm, false);
    assert_eq!(native_method_prefixes(vm), ["r_"]);
    forget_vm_transformers(vm);
    assert!(native_method_prefixes(vm).is_empty());
}

#[test]
fn is_modifiable_rejects_primitive_array_hidden() {
    assert!(is_modifiable_class(false, false, false));
    assert!(!is_modifiable_class(true, false, false));
    assert!(!is_modifiable_class(false, true, false));
    assert!(!is_modifiable_class(false, false, true));
    assert!(!is_modifiable_class(true, true, true));
}

#[test]
fn reset_clears_transformer_chain() {
    let vm = unique_vm();
    add_transformer_entry(
        vm,
        TransformerEntry {
            transformer_ref: fake_objref(0x100),
            can_retransform: true,
            native_method_prefix: None,
        },
    );
    add_transformer_entry(
        vm,
        TransformerEntry {
            transformer_ref: fake_objref(0x200),
            can_retransform: false,
            native_method_prefix: None,
        },
    );
    assert_eq!(transformer_count(vm), 2);
    reset_transformer_chain(vm);
    assert_eq!(transformer_count(vm), 0);
    forget_vm_transformers(vm);
}

#[test]
fn scan_transformer_roots_yields_every_ref() {
    let vm = unique_vm();
    add_transformer_entry(
        vm,
        TransformerEntry {
            transformer_ref: fake_objref(0x1000),
            can_retransform: true,
            native_method_prefix: None,
        },
    );
    add_transformer_entry(
        vm,
        TransformerEntry {
            transformer_ref: fake_objref(0x2000),
            can_retransform: false,
            native_method_prefix: None,
        },
    );
    let mut roots = Vec::new();
    scan_transformer_roots(vm, &mut roots);
    let addrs: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
    assert_eq!(addrs, vec![0x1000, 0x2000]);
    forget_vm_transformers(vm);
}

#[test]
fn scan_transformer_roots_empty_chain_pushes_nothing() {
    let vm = unique_vm();
    let mut roots = Vec::new();
    scan_transformer_roots(vm, &mut roots);
    assert!(roots.is_empty());
}

#[test]
fn remap_transformer_refs_rewrites_relocated_entries() {
    let vm = unique_vm();
    add_transformer_entry(
        vm,
        TransformerEntry {
            transformer_ref: fake_objref(0x1000),
            can_retransform: true,
            native_method_prefix: None,
        },
    );
    add_transformer_entry(
        vm,
        TransformerEntry {
            transformer_ref: fake_objref(0x2000),
            can_retransform: false,
            native_method_prefix: None,
        },
    );
    // Relocate only the first entry; the second is absent from the map
    // and must be left untouched.
    let mut map: cratonvm_types::PointerMap = cratonvm_types::PointerMap::default();
    map.insert(0x1000, 0x9000);
    remap_transformer_refs(vm, &map);
    let snap = snapshot_transformer_chain(vm);
    assert_eq!(snap[0].transformer_ref.as_ptr() as usize, 0x9000);
    assert_eq!(snap[1].transformer_ref.as_ptr() as usize, 0x2000);
    forget_vm_transformers(vm);
}

/// A snapshot taken BEFORE a collection is not repaired by it — which is
/// the whole reason `run_chain_over_bytes` pins the refs it walks.
///
/// `remap_transformer_refs` rewrites the CANONICAL chain; a
/// `snapshot_transformer_chain` result is a `to_vec()` copy the collector
/// has never heard of. The chain walk holds such a copy across every
/// transformer's `transform` call, each of which runs Java and allocates,
/// so without a pin the second and later entries would be dispatched on a
/// dead address. That is the failure `scan_transformer_roots`'s own doc
/// comment describes and that the canonical remap alone does not prevent.
///
/// This asserts the staleness rather than the fix, deliberately: if the
/// snapshot ever DOES get remapped this test fails and whoever made that
/// true can retire the pins with evidence.
#[test]
fn a_snapshot_taken_before_a_relocation_keeps_the_old_address() {
    let vm = unique_vm();
    add_transformer_entry(
        vm,
        TransformerEntry {
            transformer_ref: fake_objref(0x4000),
            can_retransform: true,
            native_method_prefix: None,
        },
    );
    let stale = snapshot_transformer_chain(vm);

    let mut map: cratonvm_types::PointerMap = cratonvm_types::PointerMap::default();
    map.insert(0x4000, 0xA000);
    remap_transformer_refs(vm, &map);

    assert_eq!(
        snapshot_transformer_chain(vm)[0].transformer_ref.as_ptr() as usize,
        0xA000,
        "the canonical chain must follow the relocation"
    );
    assert_eq!(
        stale[0].transformer_ref.as_ptr() as usize,
        0x4000,
        "a snapshot taken earlier is NOT rewritten — so a walk over one must \
         read its refs back through a pin, not use them raw"
    );
    forget_vm_transformers(vm);
}

#[test]
fn remap_transformer_refs_empty_map_is_noop() {
    let vm = unique_vm();
    add_transformer_entry(
        vm,
        TransformerEntry {
            transformer_ref: fake_objref(0x3000),
            can_retransform: true,
            native_method_prefix: None,
        },
    );
    remap_transformer_refs(vm, &cratonvm_types::PointerMap::default());
    let snap = snapshot_transformer_chain(vm);
    assert_eq!(snap[0].transformer_ref.as_ptr() as usize, 0x3000);
    forget_vm_transformers(vm);
}

#[test]
fn add_premain_transformer_appends_to_chain() {
    let vm = unique_vm();
    let t = fake_objref(0x4000);
    add_premain_transformer(vm, t, true, Some("__".into()));
    let snap = snapshot_transformer_chain(vm);
    assert_eq!(snap.len(), 1);
    assert!(snap[0].can_retransform);
    assert_eq!(snap[0].native_method_prefix.as_deref(), Some("__"));
    forget_vm_transformers(vm);
}

// ----- per-VM isolation (PROCESS-GLOBAL-STATE ROUND 2) ------------------

/// The core isolation property: two VMs' chains are disjoint. Under the
/// old single process-global `Vec`, `vm_b`'s chain contained `vm_a`'s
/// transformer and both counts read 2.
#[test]
fn chains_are_per_vm() {
    let vm_a = unique_vm();
    let vm_b = unique_vm();
    let ta = fake_objref(0xA000);
    let tb = fake_objref(0xB000);
    add_transformer_entry(
        vm_a,
        TransformerEntry {
            transformer_ref: ta,
            can_retransform: true,
            native_method_prefix: None,
        },
    );
    add_transformer_entry(
        vm_b,
        TransformerEntry {
            transformer_ref: tb,
            can_retransform: false,
            native_method_prefix: None,
        },
    );
    assert_eq!(transformer_count(vm_a), 1);
    assert_eq!(transformer_count(vm_b), 1);
    assert_eq!(
        snapshot_transformer_chain(vm_a)[0].transformer_ref.as_ptr() as usize,
        0xA000
    );
    assert_eq!(
        snapshot_transformer_chain(vm_b)[0].transformer_ref.as_ptr() as usize,
        0xB000
    );
    // A remove aimed at B's transformer must not touch A's chain, and
    // vice versa.
    assert!(!remove_transformer_entry(vm_a, tb));
    assert_eq!(transformer_count(vm_a), 1);
    // Resetting one VM must leave the other alone.
    reset_transformer_chain(vm_a);
    assert_eq!(transformer_count(vm_a), 0);
    assert_eq!(transformer_count(vm_b), 1);
    forget_vm_transformers(vm_a);
    forget_vm_transformers(vm_b);
}

/// The GC halves are the load-bearing ones: VM B's collection must neither
/// report VM A's addresses as roots (a pointer into a heap B does not own)
/// nor rewrite A's entries through B's relocation map.
#[test]
fn gc_halves_only_touch_the_owning_vm() {
    let vm_a = unique_vm();
    let vm_b = unique_vm();
    add_transformer_entry(
        vm_a,
        TransformerEntry {
            transformer_ref: fake_objref(0xA000),
            can_retransform: true,
            native_method_prefix: None,
        },
    );
    add_transformer_entry(
        vm_b,
        TransformerEntry {
            transformer_ref: fake_objref(0xB000),
            can_retransform: true,
            native_method_prefix: None,
        },
    );

    let mut roots_b = Vec::new();
    scan_transformer_roots(vm_b, &mut roots_b);
    let addrs: Vec<usize> = roots_b.iter().map(|r| r.as_ptr() as usize).collect();
    assert_eq!(
        addrs,
        vec![0xB000],
        "VM B's scan must not report VM A's transformer address"
    );

    // B relocates the address A's entry happens to live at. A must be
    // untouched.
    let mut map: cratonvm_types::PointerMap = cratonvm_types::PointerMap::default();
    map.insert(0xA000, 0xDEAD_0000);
    remap_transformer_refs(vm_b, &map);
    assert_eq!(
        snapshot_transformer_chain(vm_a)[0].transformer_ref.as_ptr() as usize,
        0xA000,
        "VM B's post-move fixup must not rewrite VM A's entries"
    );
    forget_vm_transformers(vm_a);
    forget_vm_transformers(vm_b);
}

/// VM teardown drops the row. Without it, a long-lived host process that
/// creates and disposes of VMs accumulates chains full of addresses into
/// heaps that no longer exist, and a later VM that reuses the identity
/// inherits them as roots.
#[test]
fn forget_vm_transformers_drops_the_row_and_is_idempotent() {
    let vm = unique_vm();
    add_transformer_entry(
        vm,
        TransformerEntry {
            transformer_ref: fake_objref(0xC000),
            can_retransform: true,
            native_method_prefix: None,
        },
    );
    assert_eq!(transformer_count(vm), 1);
    forget_vm_transformers(vm);
    assert_eq!(transformer_count(vm), 0);
    let mut roots = Vec::new();
    scan_transformer_roots(vm, &mut roots);
    assert!(
        roots.is_empty(),
        "a released VM's transformers must not be reported as roots"
    );
    // Second call is a no-op, not a panic.
    forget_vm_transformers(vm);
    assert_eq!(transformer_count(vm), 0);
}

/// Reading an unknown VM must not create a row — otherwise the map grows
/// without bound on every `run_transformer_chain` in a VM that never
/// installed a transformer.
#[test]
fn reading_an_unknown_vm_does_not_create_a_row() {
    let vm = unique_vm();
    assert_eq!(transformer_count(vm), 0);
    assert!(snapshot_transformer_chain(vm).is_empty());
    let chains = transformer_chains()
        .read()
        .unwrap_or_else(PoisonError::into_inner);
    assert!(
        !chains.contains_key(&vm),
        "read-only access must not allocate a chain for the VM"
    );
}

/// gc-common w25-c: a test identity never names a real VM, so a real
/// VM's teardown -- which every other test module in this binary runs,
/// in parallel, on `NEXT_VM_IDENTITY` values counting from 1 -- cannot
/// reach a test's chain. `unique_vm()` used to count from 1 as well.
#[test]
fn a_real_vms_teardown_never_reaches_a_test_identitys_chain() {
    let real = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let vm = unique_vm();
    assert_ne!(vm, real.vm_identity);
    assert!(
        vm > real.vm_identity,
        "test identities ({vm:#x}) come from the real counter, after {:#x}",
        real.vm_identity
    );
    add_transformer_entry(
        vm,
        TransformerEntry {
            transformer_ref: fake_objref(0xD000),
            can_retransform: true,
            native_method_prefix: None,
        },
    );
    // `Drop for SharedVm` runs `release_vm_native_state`, which calls
    // `forget_vm_transformers` for the real VM's identity only.
    drop(real);
    assert_eq!(transformer_count(vm), 1, "the test's row survives");
    forget_vm_transformers(vm);
}

#[test]
fn approximate_object_size_object() {
    // header + 5 legacy Value slots * 16. Derived, because these four
    // sizes moved together by exactly the 8 bytes the 2026-08-06 header
    // shrink returned, and a literal only records which day it was written.
    let sz = approximate_object_size(ObjectKind::Object, ArrayElementType::Reference, 0, 5);
    assert_eq!(
        sz as usize,
        cratonvm_types::HEADER_SIZE + 5 * cratonvm_types::SLOT_SIZE
    );
}

#[test]
fn approximate_object_size_byte_array() {
    // Header + 10 bytes aligned up to 8.
    let sz = approximate_object_size(ObjectKind::Array, ArrayElementType::Byte, 10, 0);
    assert_eq!(sz as usize, cratonvm_types::HEADER_SIZE + 16);
}

#[test]
fn approximate_object_size_long_array() {
    // Header + 4 * 8.
    let sz = approximate_object_size(ObjectKind::Array, ArrayElementType::Long, 4, 0);
    assert_eq!(sz as usize, cratonvm_types::HEADER_SIZE + 32);
}

#[test]
fn approximate_object_size_ref_array() {
    // Header + 3 refs * 8, rounded to the 8-byte grid.
    let sz = approximate_object_size(ObjectKind::Array, ArrayElementType::Reference, 3, 0);
    assert_eq!(sz as usize, cratonvm_types::HEADER_SIZE + 24);
}

#[test]
fn approximate_object_size_empty_array_is_positive() {
    let sz = approximate_object_size(ObjectKind::Array, ArrayElementType::Int, 0, 0);
    assert!(sz > 0);
}

/// gc-common w5-e (`common-w4e-object-size-reports-legacy-footprint`): a
/// footprint read from the object's header wins over the shape estimate.
/// A compact class of eight `boolean`s is a 16-byte header plus an
/// 8-byte packed body, not `16 + 8 * 16`.
#[test]
fn object_size_prefers_the_heap_footprint() {
    let compact = cratonvm_types::HEADER_SIZE + 8;
    assert_eq!(size_from_footprint(Some(compact)), Some(compact as i64));
    assert!(
        (compact as i64)
            < approximate_object_size(ObjectKind::Object, ArrayElementType::Reference, 0, 8),
        "the legacy formula over-sizes the compact object"
    );
    assert_eq!(size_from_footprint(Some(0)), Some(1), "never zero");
    assert_eq!(size_from_footprint(Some(usize::MAX)), Some(i64::MAX));
    assert_eq!(size_from_footprint(None), None);
}

/// A context that cannot read a header (the mock's default `None`) keeps
/// the legacy shape estimate, byte for byte.
#[test]
fn object_size_falls_back_to_the_shape_without_a_header() {
    let mut ctx = cratonvm_native_api::test_mock::MockNativeContext::new();
    let obj = cratonvm_native_api::NativeHeapAccess::alloc_object(&mut ctx, ClassId::new(7), 5);
    assert_eq!(
        object_size_of(&ctx, obj),
        approximate_object_size(ObjectKind::Object, ArrayElementType::Reference, 0, 5)
    );
}

/// The registrar must cover every `InstrumentationImpl` native the JDK
/// declares — **at the descriptor the JDK declares it with**.
///
/// Until 2026-08-11 this asserted thirteen spellings with no leading
/// `long`: `redefineClasses0([ClassDefinition;)V`,
/// `getAllLoadedClasses0()[Class;`, `isModifiableClass0(Class;)Z` and the
/// rest. **Every native on this class takes `long jvmtienv` first**, on
/// both Temurin 21.0.12+8 and 25.0.4+7 (`javap -p -s --module
/// java.instrument`), so those thirteen could never bind and the test
/// agreed with thirteen registrations that could never be reached. Both
/// sides were wrong together, which is why it stayed green for so long.
///
/// `dc55e8057` deleted the unbindable registrations as dead — correctly —
/// and this test went red. The fix is the real descriptors, not the
/// registrations back.
///
/// Three of the old assertions have no JDK counterpart at all and are gone
/// rather than corrected: `isRedefineClassesSupported0`,
/// `isNativeMethodPrefixSupported0` and `setNativeMethodPrefix0` are not
/// native on any supported image, and `appendTo{Bootstrap,System}
/// ClassLoaderSearch0` is one JDK method, `appendToClassLoaderSearch0`,
/// taking `(long, String, boolean)`.
#[test]
fn register_natives_includes_required_methods() {
    let mut r = NativeMethodRegistry::new();
    register_instrumentation_natives(&mut r);
    let impl_class = "sun/instrument/InstrumentationImpl";
    // (method, descriptor) — each one verified present and ACC_NATIVE on
    // BOTH supported images. Keep this list and the image in step: a
    // descriptor here that the JDK does not declare is a registration that
    // binds to nothing, and this assertion would hide it.
    for (name, descriptor) in [
        (
            "redefineClasses0",
            "(J[Ljava/lang/instrument/ClassDefinition;)V",
        ),
        ("retransformClasses0", "(J[Ljava/lang/Class;)V"),
        ("getAllLoadedClasses0", "(J)[Ljava/lang/Class;"),
        (
            "getInitiatedClasses0",
            "(JLjava/lang/ClassLoader;)[Ljava/lang/Class;",
        ),
        ("isModifiableClass0", "(JLjava/lang/Class;)Z"),
        ("getObjectSize0", "(JLjava/lang/Object;)J"),
        ("isRetransformClassesSupported0", "(J)Z"),
        ("appendToClassLoaderSearch0", "(JLjava/lang/String;Z)V"),
        ("setHasRetransformableTransformers", "(JZ)V"),
        // Interpreter round i6 wave 119, lane L3.
        ("setHasTransformers", "(JZ)V"),
        ("setNativeMethodPrefixes", "(J[Ljava/lang/String;Z)V"),
    ] {
        assert!(
            r.find(impl_class, name, descriptor).is_some(),
            "{impl_class}.{name}{descriptor} is declared native by JDK 21 \
             and 25 and must be registered"
        );
    }
    // The no-`jvmtienv` spellings must NOT come back. Re-adding one makes
    // the assertions above pass while binding nothing, which is exactly the
    // state this test was in before 2026-08-11.
    for (name, descriptor) in [
        (
            "redefineClasses0",
            "([Ljava/lang/instrument/ClassDefinition;)V",
        ),
        ("retransformClasses0", "([Ljava/lang/Class;)V"),
        ("getAllLoadedClasses0", "()[Ljava/lang/Class;"),
        ("isModifiableClass0", "(Ljava/lang/Class;)Z"),
        ("getObjectSize0", "(Ljava/lang/Object;)J"),
    ] {
        assert!(
            r.find(impl_class, name, descriptor).is_none(),
            "{impl_class}.{name}{descriptor} has no leading `long jvmtienv` \
             and is declared by no supported JDK image; registering it \
             binds nothing and hides the arity that does"
        );
    }
    // CratonVM's own convenience surface on the same class: no-`0`, no
    // `jvmtienv`. These are ours, not the JDK's, and the census scores them
    // `method-nowhere` for that reason.
    for (name, descriptor) in [
        (
            "removeTransformer",
            "(Ljava/lang/instrument/ClassFileTransformer;)Z",
        ),
        (
            "addTransformer",
            "(Ljava/lang/instrument/ClassFileTransformer;)V",
        ),
        (
            "addTransformer",
            "(Ljava/lang/instrument/ClassFileTransformer;Z)V",
        ),
    ] {
        assert!(
            r.find(impl_class, name, descriptor).is_some(),
            "{impl_class}.{name}{descriptor} is CratonVM's own entry point \
             and must stay registered"
        );
    }
    // Bridge surface for the in-process probe.
    let bridge = "cratonvm/Instrument";
    assert!(r
        .find(bridge, "addTransformer", "(Ljava/lang/Object;)V")
        .is_some());
    assert!(r
        .find(bridge, "removeTransformer", "(Ljava/lang/Object;)Z")
        .is_some());
    assert!(r.find(bridge, "getTransformerCount", "()I").is_some());
    assert!(r
        .find(bridge, "getAllLoadedClasses", "()[Ljava/lang/Class;")
        .is_some());
    assert!(r
        .find(bridge, "isModifiableClass", "(Ljava/lang/Class;)Z")
        .is_some());
    assert!(r
        .find(bridge, "getObjectSize", "(Ljava/lang/Object;)J")
        .is_some());
    assert!(r
        .find(bridge, "redefineClass", "(Ljava/lang/Class;[B)Z")
        .is_some());
}

// ---- Load-time transform: one offer per name per VM ----------------
//
// The bug these pin: `pre_transform_for_load` sits on the constant-pool
// resolution path, so a name whose "already defined" early-out cannot fire
// (a user-loader class the delegation chain also answers) was re-offered —
// class file re-read, supertypes re-walked, Java re-entered — on EVERY
// resolution. See [`LoadTimeOffered`].
//
// Identities come from the real counter (`unique_vm`, gcg g2/k) so these
// never collide with a real VM identity or with each other under the test
// harness's shared process; they were literals (`0x10ad_0001..5`).

#[test]
fn load_time_offer_is_claimed_exactly_once_per_name() {
    let vm = unique_vm();
    assert!(
        claim_load_time_offer(vm, "com/foo/Bar"),
        "the first resolution must do the work"
    );
    for _ in 0..1000 {
        assert!(
            !claim_load_time_offer(vm, "com/foo/Bar"),
            "every later resolution of the same name must be a no-op"
        );
    }
    // A different name is still its own first offer.
    assert!(claim_load_time_offer(vm, "com/foo/Baz"));
    forget_vm_load_time_offers(vm);
}

#[test]
fn load_time_offer_memo_is_per_vm() {
    let a = unique_vm();
    let b = unique_vm();
    assert!(claim_load_time_offer(a, "com/foo/Bar"));
    assert!(
        claim_load_time_offer(b, "com/foo/Bar"),
        "a name offered in VM A says nothing about VM B — its heap, its \
         transformer chain, its class file"
    );
    assert!(!claim_load_time_offer(a, "com/foo/Bar"));
    forget_vm_load_time_offers(a);
    forget_vm_load_time_offers(b);
}

#[test]
fn forgetting_a_vm_drops_its_offer_memo() {
    let vm = unique_vm();
    assert!(claim_load_time_offer(vm, "com/foo/Bar"));
    assert!(!claim_load_time_offer(vm, "com/foo/Bar"));
    // An identity a later VM reuses must not inherit the previous VM's
    // already-offered set, or that VM's agent never sees a class load.
    forget_vm_transformers(vm);
    assert!(claim_load_time_offer(vm, "com/foo/Bar"));
    forget_vm_load_time_offers(vm);
}

#[test]
fn resetting_the_chain_drops_the_offer_memo() {
    let vm = unique_vm();
    assert!(claim_load_time_offer(vm, "com/foo/Bar"));
    assert!(!claim_load_time_offer(vm, "com/foo/Bar"));
    reset_transformer_chain(vm);
    assert!(
        claim_load_time_offer(vm, "com/foo/Bar"),
        "\"start over\" has to mean the next transformer sees class loads"
    );
    forget_vm_load_time_offers(vm);
}
