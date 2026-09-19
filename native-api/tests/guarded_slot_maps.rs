//! W7-77-guarded-slot-maps.md — gates for the four slot maps that disagree with
//! the real JDK 25 layout and are kept safe by a guard rather than by being
//! right.
//!
//! Each of the four rows below was re-derived with `javap -p` against Eclipse
//! Adoptium 25.0.3.9 before anything here was written; the derivations live in
//! the record and in each map's own header comment. None of the four is
//! renumbered, and the reasoning per row is in the record. What these tests
//! protect is the thing a renumber would not fix and a comment cannot enforce:
//! **the guard**.
//!
//! Why guards and not behaviour. Every one of these four rows is unobservable
//! from Java today, because the guard holds on every receiver the tree can
//! currently produce. A probe asserting that `StringJoiner` renders correctly
//! passes on an unmutated tree, passes with the prefix/delimiter map applied to
//! the fabricated stub (where it is the correct map), and would only fail after
//! somebody deletes the guard — which is precisely the event these tests exist
//! to catch, and they catch it directly rather than through three layers of
//! runtime. `W7-69-read-side-alias-instrument.md` §5.1's rule applies: a gate
//! never seen to fail is the most common wasted effort, so every predicate here
//! was run against the tree and against a mutation of the thing it guards.
//!
//! These are text scans, like every gate in `read_alias_coverage.rs`, for the
//! same reason: the alternative is booting a VM per assertion.
//!
//! **A fifth row joined on 2026-08-12** — `SSC_P58_SLOT_MAP`, from
//! W7-88-net-channels-dead-registration.md. It is a different species from the
//! four above and the difference is worth stating: the other four are guarded
//! at RUNTIME by a class-side witness, so their gate is "the witness is still
//! consulted". This one has no witness and needs none, because the bodies
//! holding the belief are never dispatched to at all — every triple in
//! `register_p58_nio_channels` is re-registered later by `native-io`, and the
//! registrar itself sits behind `#[cfg(feature = "synthetic-jdk")]`. Its gate is
//! therefore "that is still true", which is the only thing keeping the map from
//! becoming live code.

use std::fs;
use std::path::{Path, PathBuf};

/// Every `.rs` file under `dir`, recursively. Copied from
/// `read_alias_coverage.rs`, which is the file this one already borrows its
/// text-scan approach from.
fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(rust_sources(&path));
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    out
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("native-api sits directly under the workspace root")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let p = workspace_root().join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// The body of the first top-level `fn NAME` in `src`, by brace depth.
///
/// Depth-counted rather than "the next `\n}`", because a column-0 `}` inside a
/// string or a nested item closes the wrong thing — three lanes were burned by
/// brace-scanning on 2026-08-12 and the one that got it right validated its
/// scanner before trusting it. `fn_body_is_delimited_by_braces_not_by_column`
/// below is that validation.
fn fn_body(src: &str, name: &str) -> String {
    let sig = format!("fn {name}(");
    let start = src
        .find(&sig)
        .unwrap_or_else(|| panic!("no `{sig}` in this file — the gate is naming a fn that moved"));
    let open = start
        + src[start..]
            .find('{')
            .unwrap_or_else(|| panic!("`{sig}` has no body"));
    let bytes = src.as_bytes();
    let mut depth = 0usize;
    for (i, b) in bytes.iter().enumerate().skip(open) {
        match *b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return src[open..=i].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces walking `{sig}`");
}

// ---------------------------------------------------------------------------
// 0. The scanner this file depends on
// ---------------------------------------------------------------------------

/// Ground truth for `fn_body` before any gate below relies on it.
///
/// Uses a fn whose body demonstrably contains a nested block and whose end is
/// known independently: `month_slot0_is_synthetic` is a one-expression fn, and
/// `month_alloc` immediately follows `month_set_value`. If `fn_body` closed on
/// the first column-0 `}` it would return a truncated `month_set_value` that
/// does not contain its own trailing `ctx.set_field`.
#[test]
fn fn_body_is_delimited_by_braces_not_by_column() {
    let src = read("native-builtins/src/phases_early.rs");
    let body = fn_body(&src, "month_set_value");
    assert!(
        body.starts_with('{') && body.ends_with('}'),
        "fn_body must return a brace-delimited body"
    );
    assert!(
        body.contains("if !month_slot0_is_synthetic"),
        "fn_body truncated before the guard"
    );
    assert!(
        body.contains("ctx.set_field(obj, MONTH_FIELD_VALUE"),
        "fn_body truncated before the write that follows the guard's early return \
         — it is closing on the wrong brace"
    );
    assert!(
        !body.contains("fn month_alloc"),
        "fn_body ran past the end of month_set_value into the next item"
    );
}

// ---------------------------------------------------------------------------
// 1. java/time/Month — synthetic-only, now with a class-side witness
// ---------------------------------------------------------------------------

/// Slot 0 is `value` only on the fabricated stub; on JDK 25 it is
/// `java.lang.Enum.name`, a String reference. Both the read and the write
/// funnel must consult the witness.
///
/// **Fails** when someone inlines a `ctx.set_field(_, MONTH_FIELD_VALUE, _)`
/// back at a call site, or drops the witness from either funnel.
#[test]
fn month_slot_zero_is_only_written_through_the_guarded_funnel() {
    let src = read("native-builtins/src/phases_early.rs");

    for funnel in ["month_set_value", "month_value"] {
        let body = fn_body(&src, funnel);
        assert!(
            body.contains("month_slot0_is_synthetic"),
            "`{funnel}` no longer consults the class-side witness — slot 0 is \
             `Enum.name` on a real java.time.Month"
        );
    }

    // The witness must stay a NAME question. A slot count cannot identify a
    // layout; that lesson is written down in four places in this tree.
    let witness = fn_body(&src, "month_slot0_is_synthetic");
    assert!(
        witness.contains("resolve_field_index_by_class_id") && witness.contains("\"name\""),
        "the Month witness must ask for a field NAME the real hierarchy has and \
         the stub does not, not for a slot count"
    );

    // Exactly three mentions of the constant outside the funnels: its own
    // declaration, the `SlotMap` entry, and nothing else. The two funnels hold
    // the rest.
    let outside: Vec<&str> = src
        .lines()
        .filter(|l| l.contains("MONTH_FIELD_VALUE"))
        .collect();
    let accesses = outside
        .iter()
        .filter(|l| l.contains("ctx.set_field") || l.contains("ctx.get_field"))
        .count();
    assert_eq!(
        accesses, 2,
        "expected exactly two field accesses on MONTH_FIELD_VALUE (one in \
         `month_set_value`, one in `month_value`); found {accesses} in:\n{outside:#?}"
    );
}

/// The registrar gate is the *outer* guard for Month, and it is the one W7-69
/// flagged: "if that registrar ever escapes `register_synthetic_overrides`".
///
/// **Fails** when `register_phase52_time_enums` or `register_t25_natives`
/// acquires a second caller, which is the escape.
#[test]
fn month_registrars_stay_synthetic_only() {
    let lib = read("native-builtins/src/lib.rs");
    for registrar in ["register_phase52_natives", "register_t25_natives"] {
        let calls = lib
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && t.contains(&format!("{registrar}(registry)"))
            })
            .count();
        assert_eq!(
            calls, 1,
            "`{registrar}` is called {calls} times from native-builtins/src/lib.rs; \
             W7-77 established it has exactly one caller, inside \
             `register_synthetic_overrides`. A second caller is the escape that \
             puts an Int into java.lang.Enum.name."
        );
    }
}

// ---------------------------------------------------------------------------
// 2. java/util/StringJoiner — prefix/delimiter swapped, class-side guard
// ---------------------------------------------------------------------------

/// Every entry point that reaches a `SJ_SYNTHETIC_SLOT_*` constant must branch
/// on `sj_real_layout` first.
///
/// **Fails** when a cleanup lifts the branch out of any of them. That is the
/// event that turns this row from "guarded" into `UriComponentsBuilder` dropping
/// path segments — the regression the guard was written for.
///
/// `sj_read_elements` is the deliberate exception and is asserted as one: it is
/// private and called only from the two legacy branches. Naming it here rather
/// than silently excluding it is the point — an unexplained exclusion rots.
#[test]
fn string_joiner_entry_points_still_branch_on_the_real_layout() {
    let src = read("native-collections/src/lib.rs");

    for entry in [
        "native_sj_init_delim",
        "native_sj_init_full",
        "native_sj_add",
        "native_sj_merge",
        "native_sj_set_empty_value",
        "sj_build_string",
    ] {
        let body = fn_body(&src, entry);
        assert!(
            body.contains("SJ_SYNTHETIC_SLOT_"),
            "`{entry}` no longer touches the synthetic slot map — if that is \
             intentional, remove it from this list and say why in \
             W7-77-guarded-slot-maps.md"
        );
        assert!(
            body.contains("sj_real_layout("),
            "`{entry}` reaches SJ_SYNTHETIC_SLOT_* without asking \
             `sj_real_layout` first. On a real java.util.StringJoiner slot 0 is \
             `prefix` and slot 1 is `delimiter` — the reverse of this map — and \
             slot 4 is the int `size`, not `emptyValue`."
        );
    }

    let helper = fn_body(&src, "sj_read_elements");
    assert!(
        !helper.contains("sj_real_layout("),
        "`sj_read_elements` is documented as the legacy-branch-only helper; if \
         it grew a guard, this gate's exception is stale"
    );
    let callers = src.matches("sj_read_elements(ctx,").count();
    assert_eq!(
        callers, 2,
        "`sj_read_elements` had exactly two callers (the legacy branches of \
         `sj_build_string` and `native_sj_merge`); a third means an unguarded \
         reader of the synthetic map"
    );
}

/// The witness must resolve ALL SEVEN real field names, so a partial rename in
/// a future JDK degrades to "use the legacy map" loudly rather than to a
/// half-real layout.
///
/// **Fails** when a name is dropped from `sj_real_layout`.
#[test]
fn the_string_joiner_witness_covers_the_whole_real_layout() {
    let src = read("native-collections/src/lib.rs");
    let body = fn_body(&src, "sj_real_layout");
    // The JDK 25.0.3.9 transitive instance layout, in order.
    for field in [
        "prefix",
        "delimiter",
        "suffix",
        "elts",
        "size",
        "len",
        "emptyValue",
    ] {
        assert!(
            body.contains(&format!("\"{field}\"")),
            "`sj_real_layout` no longer resolves `{field}`; the witness must be \
             all-or-nothing over the real class's seven instance fields"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. java/lang/reflect/Method — 11 of 12 slots, `!has_named_layout`
// ---------------------------------------------------------------------------

/// Every `METHOD_LEGACY_SLOT_*` **write** lives under `if !has_named_layout`.
///
/// **Fails** when a write escapes the block — which is what put the return type
/// into `clazz` and made Byte Buddy report `public abstract int int.value()`.
#[test]
fn every_legacy_method_slot_write_is_under_the_named_layout_gate() {
    let src = read("native-builtins/src/lang_class.rs");
    let body = fn_body(&src, "create_method_object");

    let gate = body
        .find("if !has_named_layout {")
        .expect("`create_method_object` no longer has the `!has_named_layout` gate");

    // Split on the gate rather than computing per-line byte offsets: `lines()`
    // strips a trailing `\r`, so reconstructing offsets as `len + 1` undercounts
    // by one per line on a CRLF checkout and would make this gate's verdict
    // depend on `core.autocrlf`. Comment lines are skipped — the block above the
    // gate legitimately *discusses* `METHOD_LEGACY_SLOT_*`.
    let above = &body[..gate];
    for line in above.lines() {
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with("///") {
            continue;
        }
        assert!(
            !t.contains("METHOD_LEGACY_SLOT_"),
            "a METHOD_LEGACY_SLOT_* access sits ABOVE the `!has_named_layout` \
             gate in `create_method_object`:\n  {}\nOn a real \
             java.lang.reflect.Method slot 0 is `override` and slot 2 is \
             `parameterData`, not `clazz` and `returnType`.",
            t
        );
    }

    // The witness is a class-side name question, not a slot count.
    let witness = fn_body(&src, "method_class_has_named_layout");
    assert!(
        witness.contains("resolve_field_index_by_class_id") && witness.contains("\"clazz\""),
        "the Method witness must stay a class-side field-NAME question"
    );
}

/// The legacy reads consult the same witness before falling back.
///
/// **Fails** when either `_or_legacy` helper starts returning the legacy slot
/// unconditionally — the quiet half of the same defect.
#[test]
fn the_legacy_method_readers_consult_the_same_witness() {
    let src = read("native-builtins/src/lang_class.rs");
    for reader in [
        "method_object_field_value_or_legacy",
        "method_int_field_value_or_legacy",
    ] {
        let body = fn_body(&src, reader);
        assert!(
            body.contains("method_object_has_named_layout")
                || body.contains("method_class_has_named_layout"),
            "`{reader}` falls back to the legacy slot without asking whether the \
             receiver's class has a named layout"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. java/lang/Thread — the `eetop` witness
// ---------------------------------------------------------------------------

/// The exemplar guard in this tree, and the one whose own comment records that
/// its predecessor (`num_slots() >= 5`) was wrong.
///
/// **Fails** when the read is re-gated on a slot count, or when the witness is
/// dropped. Slot 5 is `holder` on a real Thread.
#[test]
fn the_thread_virtual_slot_read_keeps_its_eetop_witness() {
    let src = read("vm/src/vm/vm_exec.rs");
    // Anchored on the binding, and delimited by braces.
    //
    // The first draft of this gate took a 1200-byte window from the first
    // mention of `SYNTHETIC_THREAD_VIRTUAL_SLOT` — which is a COMMENT 1,606
    // bytes above the witness — and was therefore RED on the untouched tree.
    // A gate that fires on an unmutated tree gets deleted, not investigated
    // (`W7-69-read-side-alias-instrument.md` §5.1 lost its gate 6 to the same
    // shape). Recorded rather than quietly corrected, because "I picked a
    // bigger window" is not the lesson; "do not guess an extent when the
    // language gives you one" is.
    let at = src.find("let is_virtual_synthetic = {").expect(
        "vm_exec no longer binds `is_virtual_synthetic` — if the virtual-thread \
                 decision moved, this gate must move with it, not be deleted",
    );
    let open = at + src[at..].find('{').expect("binding has no block");
    let mut depth = 0usize;
    let mut end = open;
    for (i, b) in src.as_bytes().iter().enumerate().skip(open) {
        match *b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    end = i;
                    break;
                }
            }
            _ => {}
        }
    }
    let window = &src[open..=end];
    assert!(
        window.contains("resolve_field_index_in_hierarchy") && window.contains("\"eetop\""),
        "the synthetic virtual-thread read lost its `eetop` class-side witness. \
         Slot 5 is `holder` on a real java.lang.Thread (19 instance fields); a \
         slot COUNT cannot identify a layout, which is why the previous \
         `num_slots() >= 5` gate was replaced."
    );
}

// ---------------------------------------------------------------------------
// 5. All the maps reach the sweep
// ---------------------------------------------------------------------------

/// Each of the rows publishes a `SlotMap`, and each publication is wired.
///
/// **Fails** when a map is declared and never handed to `declare_slot_map`,
/// which would make `verify_declared_slot_maps` sweep nothing and report clean
/// — `read_alias_coverage.rs`'s gate 6 in miniature, for these rows.
#[test]
fn every_guarded_row_publishes_its_slot_map() {
    let rows = [
        (
            "MONTH_SLOT_MAP",
            "native-builtins/src/phases_early.rs",
            "native-builtins/src/phases_early.rs",
        ),
        (
            "SJ_STUB_SLOT_MAP",
            "native-collections/src/lib.rs",
            "native-collections/src/lib.rs",
        ),
        (
            "METHOD_LEGACY_SLOT_MAP",
            "native-builtins/src/lang_class.rs",
            "native-builtins/src/lang_reflect.rs",
        ),
        (
            "SYNTHETIC_THREAD_SLOT_MAP",
            "native-builtins/src/jdk25_concurrency.rs",
            "native-builtins/src/jdk25_concurrency.rs",
        ),
        (
            "SSC_P58_SLOT_MAP",
            "native-builtins/src/phases_late/net_channels.rs",
            "native-builtins/src/phases_late/net_channels.rs",
        ),
    ];
    for (name, declared_in, wired_in) in rows {
        let d = read(declared_in);
        assert!(
            d.contains(&format!(
                "static {name}: cratonvm_native_api::read_alias::SlotMap"
            )),
            "`{name}` is not declared as a SlotMap in {declared_in}"
        );
        // The call may qualify the path (`&crate::lang_class::NAME`), so match
        // on the call and the name together rather than on one literal spelling.
        let w = read(wired_in);
        let wired = w
            .match_indices("declare_slot_map(")
            .any(|(i, _)| w[i..(i + 200).min(w.len())].contains(name));
        assert!(
            wired,
            "`{name}` is declared but never handed to `declare_slot_map` in \
             {wired_in} — the sweep would report clean because it cannot see it"
        );
    }
}

/// The published maps state the native's BELIEF, not the real JDK layout.
///
/// A map that publishes the correct answer sweeps clean and measures nothing,
/// which is the vacuous shape this whole campaign is about. Asserted on the two
/// rows whose real layout is a *permutation* of the belief, because those are
/// the ones somebody would "fix" by editing the map instead of the code.
///
/// **Fails** when someone silences the census by correcting the declaration.
#[test]
fn the_published_maps_state_the_belief_not_the_truth() {
    let sj = read("native-collections/src/lib.rs");
    assert!(
        sj.contains("(SJ_SYNTHETIC_SLOT_DELIM, \"delimiter\")")
            && sj.contains("(SJ_SYNTHETIC_SLOT_PREFIX, \"prefix\")"),
        "SJ_STUB_SLOT_MAP must keep naming slot 0 `delimiter` and slot 1 \
         `prefix` — that IS the belief, and JDK 25 has them the other way \
         round. Editing the map to agree with javap silences the row without \
         changing a line of the code that reads it."
    );

    let m = read("native-builtins/src/lang_class.rs");
    assert!(
        m.contains("(METHOD_LEGACY_SLOT_CLAZZ, \"clazz\")"),
        "METHOD_LEGACY_SLOT_MAP must keep naming slot 0 `clazz`; slot 0 is \
         `override` on a real Method and the disagreement is the finding"
    );

    let nc = read("native-builtins/src/phases_late/net_channels.rs");
    assert!(
        nc.contains("(1, \"bound\")") && nc.contains("(2, \"fd\")"),
        "SSC_P58_SLOT_MAP must keep naming slot 1 `bound` and slot 2 `fd` — that \
         IS the belief. On the real `java.nio.channels.ServerSocketChannel` slot 1 \
         is `closed` (the flag `AbstractInterruptibleChannel.isOpen()` reads) and \
         slot 2 is `interruptor` (a `sun.nio.ch.Interruptible`, not an int fd). \
         Editing the map to agree with javap silences the row without changing a \
         line of the code that holds the belief. \
         See W7-88-net-channels-dead-registration.md."
    );
}

// ---------------------------------------------------------------------------
// 6. `SSC_P58_SLOT_MAP` — the row with no runtime witness, and why it needs none
// ---------------------------------------------------------------------------

/// The dead `ServerSocketChannel.socket()` stays deleted, its winner stays
/// registered, and its registrar stays behind the `synthetic-jdk` gate.
///
/// This is the whole guard for row 5. W7-88 deleted a body that wrote seven real
/// JDK fields — five on a `java.net.ServerSocket` (`impl`, `created`, `bound`,
/// `closed`, `socketLock`) and one into
/// `AbstractInterruptibleChannel.interruptedTarget` on the channel — on the
/// measured ground that nothing could reach it. Three independent facts made
/// that true, and each is asserted here, because losing any one of them turns
/// `SSC_P58_SLOT_MAP` from a census row into live heap corruption.
///
/// **Fails** when the registration comes back, when `native-io` stops
/// registering the winner, or when a second caller appears for either link of
/// the chain that keeps the registrar synthetic-only.
///
/// Not a behavioural probe, deliberately: there is no behaviour to assert. The
/// registrar contributes nothing to `--dump-native-registry` in any runnable
/// configuration, so a Java-level probe over `socket()` passes identically
/// before and after the deletion and would be measuring `native-io`.
#[test]
fn ssc_p58_socket_stays_deleted_and_the_registrar_stays_gated() {
    // (a) The deletion holds. Matched on the descriptor AS A RUST STRING
    //     LITERAL, quotes included: the first spelling of this gate looked for
    //     the bare descriptor and went red on an unmutated tree, because the
    //     comment left in place of the deleted body names the triple it
    //     replaced. A gate that fires on the tree it ships with gets deleted
    //     rather than investigated, which is `read_alias_coverage.rs`'s own
    //     recorded near-miss. The descriptor is unique to this triple inside the
    //     registrar — the DatagramChannel `socket()` next door returns a
    //     `java.net.DatagramSocket` — so the quoted form has exactly one
    //     meaning here: a registration.
    let nc = read("native-builtins/src/phases_late/net_channels.rs");
    let p58 = fn_body(&nc, "register_p58_nio_channels");
    assert!(
        !p58.contains("\"()Ljava/net/ServerSocket;\""),
        "`register_p58_nio_channels` registers `ServerSocketChannel.socket()` \
         again. W7-88 deleted it because it wrote `java.net.ServerSocket.closed \
         := -1` and put the socket in the channel's `interruptedTarget`, which \
         `AbstractInterruptibleChannel.end(boolean)` reads on every \
         interruptible operation. If this triple is genuinely needed here, the \
         map has to be re-derived against `javap -p` first, not restored."
    );

    // (b) The winner is still there. Deleting the loser is only a no-op while
    //     something else serves the triple.
    let sc = read("native-io/src/socket_channel.rs");
    let real = fn_body(&sc, "register_socket_channel_real");
    assert!(
        real.contains(r#"r.register(c, "socket", "()Ljava/net/ServerSocket;", ssc_socket);"#),
        "`native-io`'s `register_socket_channel_real` no longer registers \
         `ServerSocketChannel.socket()`. That registration is what made W7-88's \
         deletion a no-op; without it the triple has no owner at all."
    );

    // (c) The registrar stays reachable from exactly one place, and that place
    //     stays inside `#[cfg(feature = "synthetic-jdk")] register_synthetic_overrides`.
    //     Both symbols are `pub(crate)`, so `native-builtins/src` is the whole
    //     search space. Two occurrences each: the `fn` and the single call.
    let mut sources = String::new();
    for file in rust_sources(&workspace_root().join("native-builtins").join("src")) {
        sources.push_str(&fs::read_to_string(&file).unwrap_or_default());
    }
    for symbol in ["register_p58_nio_channels(", "register_phase58_natives("] {
        let n = sources.matches(symbol).count();
        assert_eq!(
            n, 2,
            "`{symbol}` occurs {n} times in native-builtins/src; expected exactly \
             2 (its `fn` and its one call site). A second caller is the escape \
             that makes SSC_P58_SLOT_MAP's beliefs live — re-derive the whole \
             chain before changing this number. A prose mention spelled with a \
             trailing `(` also trips this; reword the comment rather than \
             loosening the gate."
        );
    }

    let lib = read("native-builtins/src/lib.rs");
    assert!(
        lib.contains("#[cfg(feature = \"synthetic-jdk\")]\npub fn register_synthetic_overrides("),
        "`register_synthetic_overrides` is no longer immediately preceded by \
         `#[cfg(feature = \"synthetic-jdk\")]`. That attribute is why phase 58 is \
         absent from every default-build census."
    );
    assert!(
        fn_body(&lib, "register_synthetic_overrides")
            .contains("register_phase58_natives(registry);"),
        "phase 58 is no longer called from `register_synthetic_overrides`. It may \
         have moved somewhere the real-JDK boot path reaches, which is exactly \
         the escape this gate exists for."
    );
}
