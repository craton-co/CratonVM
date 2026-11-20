//! Source witnesses for the four nominations this lane closed in
//! `lib.rs`. Each of them is a small edit inside a large registrar, and
//! each was reported by ANOTHER lane precisely because a large registrar
//! is where an edit goes quiet — so what is checked here is not the
//! behaviour (the crate's tests cannot start a VM) but the thing that
//! actually went wrong before: **an edit that is present at some of its
//! sites and missing at the rest**.
//!
//! HANDOFF-20260814 §5 names both failure modes these witnesses cover:
//! "a scripted edit matched zero sites" and "scripted edits land on the
//! wrong twin". A nine-site change with eight sites done looks exactly
//! like a nine-site change with nine.
//!
//! Written against the FILE TEXT rather than against a mock context on
//! purpose. `alloc_synth_timezone` and the nine `Thread.<init>` bodies are
//! closures inside `register_essential_natives_with_shims`; nothing in
//! this crate can name them, and a mock-context test would have to
//! re-implement the registrar to reach them.

fn lib_rs() -> String {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib_parts");
    ["dbg_toarray_enabled.rs", "essential_dur_parse.rs", "register_essential_natives_with.rs", "essential_natives_shims_a.rs", "surefire_channel_can_use.rs",
     "url_path_or_file_field.rs", "native_set_accessible_write_override.rs", "essential_natives_shims_e.rs", "formatter_alloc_sink.rs", "arrays_range_or_throw.rs",
     "native_printwriter_flush.rs", "matcher_captures.rs", "charset_concrete_or_synthetic.rs", "lock_key_for.rs", "native_invocation_handler_invoke.rs"]
        .iter()
        .map(|part| std::fs::read_to_string(root.join(part)).expect("registrar source is readable"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `register_essential_natives_with_shims`, and ONLY that function.
///
/// This scoping is load-bearing, and it is the hazard the sibling lanes
/// warned about (`http_url_connection.rs` / `net_phase_e.rs`: one
/// registrar silently overwriting five of six accessors under the same
/// function name). `java/lang/Thread.<init>` is registered by FOUR
/// families in this file: this one at ~13705, one inside
/// `alloc_carrier_thread_mirror` (~6921), and twenty-two more inside
/// `register_synthetic_overrides` (~22196 onward). `register()` is
/// last-write-wins with no unregister API, so which family owns the slot
/// is a question about CALL ORDER and cannot be read off the file.
///
/// It was settled by measurement, not by reading: `--dump-native-registry`
/// on the d2e127930 binary reports all nine essential-registrar rows with
/// `owns_slot=true` and `overwrote=null`, in `--real-jdk` AND in
/// Compatible. The `register_synthetic_overrides` copies do not run in
/// either configuration. So this witness checks the nine that answer, and
/// a scan over the whole file would have checked thirty-two bodies of
/// which twenty-three are unreachable.
fn essential_registrar(src: &str) -> &str {
    let start = src
        .find("\nfn register_essential_natives_with_shims(")
        .or_else(|| src.find("\npub fn register_essential_natives_with_shims("))
        .expect("register_essential_natives_with_shims is still in lib.rs");
    let rest = &src[start + 1..];
    for part in ['a', 'b', 'c', 'd', 'e'] {
        assert!(rest.contains(&format!("register_essential_natives_with_shims_{part}(registry, shim_selection)")));
    }
    let stop = rest
        .find("\nfn register_synthetic_overrides(")
        .or_else(|| rest.find("\npub fn register_synthetic_overrides("))
        .expect(
            "register_synthetic_overrides is still in lib.rs, after the essential registrar",
        );
    &rest[..stop]
}

/// The bodies of every `registry.register("java/lang/Thread", "<init>",
/// ...)` in the given slice, one string per registration, cut at the next
/// registration or the end of the slice.
fn thread_ctor_bodies(src: &str) -> Vec<String> {
    // Assembled rather than written as one literal so this test's own
    // text is never what the scan finds.
    let ctor = format!("\"{}\"", "<init>");
    let multiline = format!("\"java/lang/Thread\",\n        {ctor},");
    let inline = format!("register(\"java/lang/Thread\", {ctor}, \"()V\"");
    let mut out = Vec::new();
    for pat in [multiline.as_str(), inline.as_str()] {
        let mut from = 0usize;
        while let Some(i) = src[from..].find(pat) {
            let start = from + i;
            let rest = &src[start..];
            // A registration body ends where the next one begins.
            let stop = rest[1..]
                .find("registry.register")
                .map(|k| k + 1)
                .unwrap_or(rest.len());
            out.push(rest[..stop].to_string());
            from = start + 1;
        }
    }
    out
}

/// G5-1 N2, and the reason it is a witness rather than a comment: the
/// change is ONE line repeated across NINE constructor bodies that differ
/// from each other in arity, in layout arm and in argument order. Eight of
/// nine compiles, runs, and closes eight of the fifteen measured rows —
/// silently.
///
/// MEASURED with `--dump-native-registry` on the d2e127930 binary
/// (`ItlProbe`, both modes): all nine own their slot, `kind=bridge`, and
/// seven of the nine record non-zero `invocations` on a ten-case probe.
/// They are live under `--real-jdk` as well as in Compatible — which
/// falsifies G5-1 N2's own prediction that they would be inert there.
#[test]
fn every_thread_constructor_captures_inheritable_thread_locals() {
    let src = lib_rs();
    let bodies = thread_ctor_bodies(essential_registrar(&src));
    assert_eq!(
        bodies.len(),
        9,
        "expected exactly 9 java/lang/Thread constructor registrations, found \
         {} — the scan is broken or a constructor was added or removed; either \
         way the per-body check below would be checking the wrong population",
        bodies.len()
    );
    // `&mut this` since `WORKER-5-NOTE-13`: the funnel allocates, so it
    // hands the refreshed receiver back rather than stranding the caller
    // with a pre-move address. Kept EXACT rather than loosened to a name
    // match -- the point of this witness is that each body passes its own
    // `this`, and a prefix match would stop checking that.
    let call = format!(
        "capture_inheritable_tl_at_{}(ctx, &mut this)",
        "construction"
    );
    let queue = format!("queue_inherited_tl_for_{}(ctx, this", "child");
    // gen r4w3/rooting: the shims now reach the capture through
    // `thread_init_capture_itl_rooted`, which also keeps the shim's
    // group/target/name arguments rooted across the `childValue` upcall.
    // Accepted only as the same exact `&mut this` form, and only while the
    // wrapper itself still makes the capture call checked above.
    let rooted = format!("thread_init_capture_itl_{}(ctx, &mut this, args)", "rooted");
    let wrapper_start = src
        .find("fn thread_init_capture_itl_rooted(")
        .expect("the rooted capture wrapper exists");
    let wrapper = &src[wrapper_start..];
    let wrapper = &wrapper[..wrapper.find("\n}\n").unwrap_or(wrapper.len())];
    assert!(
        wrapper.contains("capture_inheritable_tl_at_construction(ctx, this)"),
        "thread_init_capture_itl_rooted no longer makes the capture call"
    );
    for (n, body) in bodies.iter().enumerate() {
        assert!(
            body.contains(&call) || body.contains(&queue) || body.contains(&rooted),
            "Thread constructor registration #{n} does not capture inheritable \
             ThreadLocals at construction. HotSpot captures in the master \
             constructor that ALL eight public forms forward to (MEASURED, \
             G5-1 section 2, rows 1/3/9/12/15), so a constructor that skips \
             the capture lets Thread.start() re-snapshot the parent's LATER \
             values. Body began:\n{}",
            &body[..body.len().min(300)]
        );
    }
}

/// The opt-out arm, which is the one of the nine that must NOT take the
/// plain capture. `new Thread(g, r, n, 0, false)` sets
/// `characteristics |= NO_INHERIT_THREAD_LOCALS` and HotSpot's master
/// constructor then skips the copy entirely — MEASURED, the child reads
/// `null` where the parent had a value set.
///
/// It still has to queue an EMPTY capture, because an ABSENT queue entry
/// sends `native_thread_start0` back to snapshotting the parent's CURRENT
/// map — i.e. opting out would inherit MORE than opting in.
#[test]
fn the_opt_out_constructor_queues_an_empty_capture_not_the_parents_values() {
    let src = lib_rs();
    let marker = "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;JZ)V";
    let i = src
        .find(marker)
        .unwrap_or_else(|| panic!("the 5-argument Thread constructor {marker} is gone"));
    let body = &src[i..(i + 3000).min(src.len())];
    assert!(
        body.contains("args.get(5)"),
        "the opt-out constructor must read the inheritThreadLocals flag at \
         argument slot 5 — a `J` occupies ONE slot in this args vec, not two \
         (SOURCE-VERIFIED against Unsafe.putBoolean(Object,long,boolean), whose \
         body reads the receiver at 1, the offset at 2 and the boolean at 3)"
    );
    assert!(
        body.contains("Default::default()"),
        "opting out must QUEUE AN EMPTY capture, not skip the capture: an \
         absent queue entry is indistinguishable from 'nobody asked', and \
         native_thread_start0 then snapshots the parent at start time"
    );
}

/// The rawOffset FIELD and `getRawOffset()` are two producers of one
/// quantity, and the retired one was still writing: MEASURED on the
/// d2e127930 binary, `getRawOffset()` is right on all 632 ids
/// `TimeZone.getAvailableIDs()` returns, while the field was seeded from a
/// roughly 50-entry hand table whose own successor note calls it
/// superseded.
#[test]
fn the_synthetic_timezone_seeds_raw_offset_from_tzdb_not_the_hand_table() {
    let src = lib_rs();
    let i = src
        .find("fn alloc_synth_timezone")
        .expect("alloc_synth_timezone is still in lib.rs");
    let body = &src[i..(i + 7000).min(src.len())];
    let tzdb_call = format!("crate::tzdb::raw_offset_{}(ctx, id_str)", "seconds");
    assert!(
        body.contains(&tzdb_call),
        "alloc_synth_timezone must seed the rawOffset FIELD from the same \
         source getRawOffset() answers from. The hand table is the FALLBACK — \
         for the deprecated three-letter aliases and the GMT+HH:MM spellings \
         ZoneId.of cannot resolve — and not the primary."
    );
    // The comment this replaced claimed getOffset(long) reads the field. It
    // does not: it is a registered native (register_tzdb_offset_natives_for)
    // that answers from tzdb regardless of what the field holds. A comment
    // saying otherwise sends the next reader to the wrong producer.
    assert!(
        !body.contains("ZoneInfo.getOffset(long) returns this rawOffset"),
        "the retired claim that getOffset(long) reads the rawOffset field is \
         back in alloc_synth_timezone. It is a registered native and it reads tzdb."
    );
}

/// `java.util.logging` punishes generalisation, and this is the row it
/// punishes: on `Logger`, `setParent(null)` and `addHandler(null)` THROW
/// while `setLevel(null)`, `setFilter(null)` and `removeHandler(null)`
/// RETURN — and `Handler.setLevel(null)`, the same-named method one type
/// over, throws. All MEASURED on HotSpot 25.0.3+9 (G15-1 section 1;
/// re-measured 2026-08-17 by this lane).
///
/// So this witness is deliberately NEGATIVE as well as positive: it fails
/// if a later edit widens the refusal onto `Logger.setLevel`.
#[test]
fn only_set_parent_refuses_null_among_loggers_one_argument_setters() {
    let src = lib_rs();
    let i = src
        .find("\"setParent\",\n        \"(Ljava/util/logging/Logger;)V\"")
        .expect("Logger.setParent registration is still in lib.rs");
    let body = &src[i..(i + 4000).min(src.len())];
    assert!(
        body.contains("NullPointerException { message: None }"),
        "Logger.setParent(null) must raise a BARE NullPointerException — its \
         getMessage() is null, not a helpful-NPE text (MEASURED)"
    );

    let level = src
        .find("\"setLevel\",\n        \"(Ljava/util/logging/Level;)V\"")
        .expect("Logger.setLevel registration is still in lib.rs");
    let level_body = &src[level..(level + 1200).min(src.len())];
    assert!(
        !level_body.contains("NullPointerException"),
        "Logger.setLevel(null) RETURNS NORMALLY on HotSpot (MEASURED) — the \
         OPPOSITE verdict from Handler.setLevel(null), which throws. A blanket \
         null rule across this family breaks 22 measured rows."
    );
}

/// `new LogRecord(null, "m")` throws and `new LogRecord(INFO, null)` is
/// LEGAL — one constructor, two arguments, opposite verdicts. MEASURED on
/// HotSpot; the JDK body is
/// `this.level = Objects.requireNonNull(level); this.message = msg;`.
#[test]
fn the_log_record_constructor_requires_the_level_and_permits_a_null_message() {
    let src = lib_rs();
    let ctor = format!("\"{}\"", "<init>");
    let marker = format!("\"java/util/logging/LogRecord\",\n        {ctor}");
    let i = src
        .find(&marker)
        .expect("the LogRecord(Level,String) registration is still in lib.rs");
    let body = &src[i..(i + 2600).min(src.len())];
    assert!(
        body.contains("matches!(args.get(1), None | Some(Value::Object(None)))"),
        "the LogRecord constructor must refuse a null LEVEL (argument slot 1)"
    );
    assert!(
        !body.contains("args.get(2), None | Some(Value::Object(None))"),
        "the LogRecord constructor must NOT refuse a null MESSAGE (slot 2): \
         `new LogRecord(Level.INFO, null)` returns normally on HotSpot and its \
         getMessage() is null. Nine of LogRecord's eleven setters likewise \
         return on null; only setLevel and setInstant throw."
    );
}
