    use super::*;
    use crate::test_utils::{mock_ctx, MockNativeContext};
    use cratonvm_native_api::FieldMetadata;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use cratonvm_types::ClassId;

    /// THE DISCRIMINATOR that turns [`harvest_real_map`] from a union into a
    /// reconciliation, tested directly because the three answers it gives are
    /// what separate three different writers.
    ///
    /// `jdk-only/the-system-properties-real-map-is-null-and-it-blocks-the-chm-retirement-20260909-RETIRED-20260921.md`
    /// §7 left this open as "a property written into the VM store from RUST is
    /// masked by an older value in the map until the next
    /// `System.getProperties()` refills it", and expected the fix to need a
    /// generation counter. It needed the published VALUE instead, and
    /// "until the next refill" understated it: the refill's harvest runs FIRST,
    /// so the stale value was written back over the VM's and the loss was
    /// permanent. `SysPropsVmWriteProbe.java` rows 3 and 5 are the
    /// end-to-end form; this is the unit.
    ///
    /// A VM identity nothing else uses, so a parallel test cannot collide on
    /// the process-global table.
    #[test]
    fn a_published_value_echoed_back_is_not_a_receiver_write() {
        const VM: usize = 0xF00D_6AC0;
        last_published().lock().insert(
            VM,
            [
                ("user.timezone".to_string(), String::new()),
                ("java.home".to_string(), "/opt/jdk".to_string()),
            ]
            .into_iter()
            .collect(),
        );

        // Published and unchanged: the map is echoing the VM's own value, so
        // the store is at least as fresh and must win.
        assert!(
            !map_entry_is_a_receiver_write(VM, "user.timezone", ""),
            "an entry equal to what was published carries no opinion"
        );
        assert!(!map_entry_is_a_receiver_write(VM, "java.home", "/opt/jdk"));

        // Published and CHANGED: real `Properties` bytecode wrote it.
        assert!(
            map_entry_is_a_receiver_write(VM, "user.timezone", "Etc/UTC"),
            "a value different from the published one is an application write"
        );

        // Never published at all: it can only have been added through the
        // receiver, and the safe direction is to treat it as one.
        assert!(map_entry_is_a_receiver_write(VM, "probe.added", "v"));

        // An unknown VM degrades to the pre-2026-09-20 rule (map wins) rather
        // than to silently dropping an application write.
        assert!(map_entry_is_a_receiver_write(
            VM + 1,
            "java.home",
            "/opt/jdk"
        ));

        last_published().lock().remove(&VM);
    }

    // -----------------------------------------------------------------------
    // G49-1 — the second-largest coercion cluster, and why it is benign.
    //
    // MEASURED, 19 vectors under `CRATONVM_DBG_COERCION=1` on
    // `target-rel3/release/cratonvm.exe`: 337 `primitive-into-reference` events
    // at `props_defaults`, descriptor `L`, and every single one is a READ
    // (frame `VmHeap::get_field_as`) carrying `value=Int(0)`. `Int(0)` is what
    // a never-written slot decodes to under `gen_heap.rs`'s R-niche rule —
    // `java.util.Properties.defaults` on a `Properties` built without a parent.
    // The coercion answers `null`, which is what the field means, and a
    // 22-check differential against HotSpot 25.0.3+9-LTS (including the
    // non-String-own-value fall-through and a three-level chain) is identical.
    //
    // So the cluster is noise, not damage — but it is only noise while
    // `props_defaults` treats a primitive as "no defaults". These pin that,
    // and pin that it still finds a real parent when there is one, so nobody
    // can close the noise by making the function refuse.
    // -----------------------------------------------------------------------

    /// Real-JDK flat slot of `java.util.Properties.defaults`: `Hashtable`
    /// contributes `table`/`count`/`threshold`/`loadFactor`/`modCount`/
    /// `keySet`/`entrySet`/`values` first. Cross-checked against
    /// `javap -p java.util.Hashtable` on HotSpot 25.0.3+9-LTS and against
    /// `native-collections`' own `define_field(PROPERTIES_CID, "defaults", 8)`.
    const PROPERTIES_DEFAULTS_SLOT: usize = 8;

    fn properties_class(ctx: &mut MockNativeContext) -> ClassId {
        let cid = ctx
            .ensure_class_initialized("java/util/Properties")
            .expect("mock could not initialize java/util/Properties");
        ctx.set_declared_fields(
            cid,
            vec![FieldMetadata {
                name: "defaults".to_string(),
                descriptor: "Ljava/util/Properties;".to_string(),
                access_flags: 0,
                slot_index: PROPERTIES_DEFAULTS_SLOT,
                declaring_class_id: cid,
                is_static: false,
            }],
        );
        cid
    }

    /// **The 245/95-event shape.** A `Properties` with no parent: the slot was
    /// never written, so it reads back as the raw `Int(0)` the allocator left,
    /// the descriptor-aware read coerces it to `null`, and the answer is "no
    /// defaults". Correct, and the reason G49-1 files this cluster benign.
    #[test]
    fn a_never_written_defaults_slot_means_no_defaults_not_a_lost_parent() {
        let mut ctx = mock_ctx();
        let cid = properties_class(&mut ctx);
        let props = ctx.alloc_object(cid, PROPERTIES_DEFAULTS_SLOT + 2);
        assert_eq!(
            ctx.get_field(props, PROPERTIES_DEFAULTS_SLOT),
            Value::Int(0),
            "fixture must reproduce the never-initialised slot, not a null"
        );
        assert_eq!(props_defaults(&ctx, props), None);
    }

    /// The other half, so the test above cannot be satisfied by a function that
    /// always answers `None` — which would silently break
    /// `getProperty`'s fall-through chain and is exactly the failure G45-1
    /// suspected this cluster of being.
    #[test]
    fn a_real_parent_is_found_so_the_fall_through_chain_survives() {
        let mut ctx = mock_ctx();
        let cid = properties_class(&mut ctx);
        let parent = ctx.alloc_object(cid, PROPERTIES_DEFAULTS_SLOT + 2);
        let child = ctx.alloc_object(cid, PROPERTIES_DEFAULTS_SLOT + 2);
        ctx.set_field(child, PROPERTIES_DEFAULTS_SLOT, Value::Object(Some(parent)));
        assert_eq!(props_defaults(&ctx, child), Some(parent));
    }

    /// An explicit `null` parent — what a `putfield` of `null` leaves — is
    /// indistinguishable from "no parent", and must not be mistaken for one.
    #[test]
    fn an_explicitly_null_defaults_slot_is_also_no_defaults() {
        let mut ctx = mock_ctx();
        let cid = properties_class(&mut ctx);
        let props = ctx.alloc_object(cid, PROPERTIES_DEFAULTS_SLOT + 2);
        ctx.set_field(props, PROPERTIES_DEFAULTS_SLOT, Value::Object(None));
        assert_eq!(props_defaults(&ctx, props), None);
    }

    /// When the field cannot be resolved at all the function answers `None`
    /// rather than reading slot 0 — which on the real flat layout is
    /// `Hashtable.table`, an `Entry[]`, and would hand `getProperty` an array
    /// to recurse into.
    #[test]
    fn an_unresolvable_defaults_field_answers_none_rather_than_slot_zero() {
        let mut ctx = mock_ctx();
        let props = ctx.alloc_object(ClassId::new(0), PROPERTIES_DEFAULTS_SLOT + 2);
        let table = ctx.alloc_object(ClassId::new(0), 1);
        ctx.set_field(props, 0, Value::Object(Some(table)));
        assert_eq!(props_defaults(&ctx, props), None);
    }

    fn kv(pairs: &[(&str, &str)]) -> Vec<(JavaText, JavaText)> {
        pairs
            .iter()
            .map(|(k, v)| (JavaText::from(*k), JavaText::from(*v)))
            .collect()
    }

    fn names(items: &[&str]) -> Vec<JavaText> {
        items.iter().map(|s| JavaText::from(*s)).collect()
    }

    /// Rust-text view of a parse result, so the escape-grammar tests below can
    /// keep asserting plain `&str` expectations. Anything ABOUT surrogates must
    /// assert units instead — see `parse_keeps_a_lone_surrogate_escape_as_its_unit`.
    fn lossy(pairs: Vec<(JavaText, JavaText)>) -> Vec<(String, String)> {
        pairs
            .into_iter()
            .map(|(k, v)| (k.to_lossy(), v.to_lossy()))
            .collect()
    }

    /// The gh-11892 shape, with the exact key order HotSpot's
    /// `Properties`/`ConcurrentHashMap` produces for these four keys (verified
    /// against Temurin 25 on 2026-08-01). Spring Boot's `MapBinder` binds
    /// `commit.id` as a nested `Map` only if `commit.id` is enumerated AFTER at
    /// least one of its descendants — which is precisely what this ordering
    /// delivers and what a raw hash-bucket order destroyed.
    #[test]
    fn reorder_follows_the_chm_order_for_the_git_properties_shape() {
        let side = kv(&[
            ("branch", "master"),
            ("commit.id", "1b3cec34f7ca0a021244452f2cae07a80497a7c7"),
            ("commit.id.abbrev", "1b3cec3"),
            ("commit.id.full", "1b3cec34f7ca0a021244452f2cae07a80497a7c7"),
        ]);
        let order = names(&["commit.id.full", "branch", "commit.id.abbrev", "commit.id"]);
        let reordered = reorder_by(&side, &order);
        let got: Vec<String> = reordered.iter().map(|(k, _v)| k.to_lossy()).collect();
        assert_eq!(
            got,
            vec!["commit.id.full", "branch", "commit.id.abbrev", "commit.id"]
        );
        assert!(
            got.iter().position(|k| *k == "commit.id").unwrap()
                > got.iter().position(|k| *k == "commit.id.abbrev").unwrap(),
            "an ancestor key that is also a leaf value must enumerate AFTER its \
             descendants, or MapBinder binds it as a scalar and drops the nest"
        );
    }

    /// `order` permutes; it must never filter, duplicate, or invent.
    #[test]
    fn reorder_preserves_every_entry() {
        let side = kv(&[("a", "1"), ("b", "2"), ("c", "3")]);
        // `z` is in the CHM but not the side-table; `b` is not named by `order`.
        let order = names(&["z", "c", "a"]);
        let got = reorder_by(&side, &order);
        assert_eq!(got, kv(&[("c", "3"), ("a", "1"), ("b", "2")]));
        assert_eq!(got.len(), side.len());
    }

    #[test]
    fn reorder_tolerates_a_duplicated_order_key() {
        let side = kv(&[("a", "1"), ("b", "2")]);
        let order = names(&["b", "b", "a"]);
        assert_eq!(reorder_by(&side, &order), kv(&[("b", "2"), ("a", "1")]));
    }

    /// No CHM backing: the side-table's own order passes through untouched.
    #[test]
    fn reorder_with_no_chm_order_is_a_passthrough() {
        let side = kv(&[("a", "1"), ("b", "2")]);
        assert_eq!(reorder_by(&side, &[]), side);
    }

    /// The side-table is the order every enumeration native falls back to when
    /// the receiver has no CHM backing, so it must be insertion-ordered rather
    /// than hash-bucket-ordered, and a removal must not disturb the survivors.
    #[test]
    fn props_map_is_insertion_ordered_across_removal() {
        let mut m = PropsMap::default();
        for k in ["branch", "commit.id", "commit.id.abbrev", "commit.id.full"] {
            m.insert(JavaText::from(k), JavaText::from("v"));
        }
        let keys: Vec<String> = m.keys().map(|k| k.to_lossy()).collect();
        assert_eq!(
            keys,
            vec!["branch", "commit.id", "commit.id.abbrev", "commit.id.full"]
        );
        // `shift_remove` (what `remove_kv` uses) keeps the survivors in order;
        // `swap_remove` would teleport the last key into the hole.
        m.shift_remove(&JavaText::from("commit.id"));
        let keys: Vec<String> = m.keys().map(|k| k.to_lossy()).collect();
        assert_eq!(keys, vec!["branch", "commit.id.abbrev", "commit.id.full"]);
        // Re-inserting an existing key must NOT move it to the back.
        m.insert(JavaText::from("branch"), JavaText::from("other"));
        assert_eq!(
            m.keys().next().map(|k| k.to_lossy()).as_deref(),
            Some("branch")
        );
    }

    /// `reorder_by` must not depend on the side-table's own iteration order for
    /// any key the CHM names — that is the whole point of deferring to the CHM,
    /// and it is what makes the JDK-order guarantee independent of however the
    /// side-table happens to be stored. Feed the same entries in two different
    /// orders and require the same answer.
    #[test]
    fn reorder_is_independent_of_side_table_order_for_chm_named_keys() {
        let order = names(&["commit.id.full", "branch", "commit.id.abbrev", "commit.id"]);
        let a = kv(&[
            ("branch", "b"),
            ("commit.id", "i"),
            ("commit.id.abbrev", "a"),
            ("commit.id.full", "f"),
        ]);
        let b = kv(&[
            ("commit.id", "i"),
            ("commit.id.full", "f"),
            ("branch", "b"),
            ("commit.id.abbrev", "a"),
        ]);
        assert_eq!(reorder_by(&a, &order), reorder_by(&b, &order));
        assert_eq!(
            reorder_by(&a, &order),
            kv(&[
                ("commit.id.full", "f"),
                ("branch", "b"),
                ("commit.id.abbrev", "a"),
                ("commit.id", "i"),
            ])
        );
    }

    /// Source-level guard. Every native that hands an *enumeration order* to
    /// Java must read `ordered_snapshot_kv`, never the raw, insertion-ordered
    /// `snapshot_kv` — the whole gh-11892 defect was one such site reading an
    /// order the JDK would never produce. New enumeration natives are easy to
    /// add and easy to wire to the wrong helper, and no end-to-end test would
    /// notice until some binder somewhere silently picked the wrong branch. So
    /// pin the call sites: a bare `snapshot_kv(` is allowed only in the
    /// functions listed here, each of which is order-insensitive or is the
    /// ordering machinery itself.
    /// Companion witness to
    /// `only_order_insensitive_functions_read_the_unordered_snapshot`, for the
    /// hazard that ordering fix INTRODUCED.
    ///
    /// `ordered_snapshot_kv` re-enters Java (`entrySet`/`iterator`/`getKey`),
    /// so it allocates, so it is a GC point — its own doc says exactly that.
    /// It takes the receiver by `&mut` precisely so a caller cannot carry a
    /// pre-GC receiver across it without a compile error. **That `&mut` says
    /// nothing about the OTHER references the caller is holding beside the
    /// receiver**, and moving three functions from the pure side-table read
    /// `snapshot_kv` to this one silently put those refs across a collection:
    ///
    ///   * `native_properties_clone` refreshed `this` and then handed
    ///     `native_object_clone` the caller's `args`, which nothing refreshed;
    ///   * `native_properties_replace_all` pinned its `BiFunction` AFTER the
    ///     walk, registering an address the collector may already have
    ///     vacated — protection in shape only;
    ///   * `native_properties_for_each` pinned nothing at all and dispatched
    ///     `accept` on a `BiConsumer` read out of `args` N calls earlier.
    ///
    /// None of that is visible to `scripts/stale-receiver-audit.py`, whose
    /// shape is a funnel taking the RECEIVER by value: `ordered_snapshot_kv`
    /// already takes `&mut`, so the audit considers it solved and never looks
    /// at what the caller holds next to it.
    ///
    /// The rule: a local captured out of `args` before an `ordered_snapshot_kv`
    /// call and still used after it must be pinned BEFORE that call. The
    /// receiver handed to the `&mut` parameter is exempt — that is what the
    /// `&mut` is for.
    #[test]
    fn refs_held_across_the_ordered_snapshot_are_pinned_before_it() {
        let src = include_str!("../properties_sidetable.rs");
        // Function bodies, in source order.
        // Only the production half of the file: the test module below defines
        // nested helper `fn`s, and a naive split would hand this very test's
        // body to one of them.
        let src = src
            .split(
                "
mod tests {",
            )
            .next()
            .unwrap_or(src);
        let mut fns: Vec<(String, Vec<&str>)> = Vec::new();
        for line in src.lines() {
            let t = line.trim_start();
            if let Some(rest) = t
                .strip_prefix("fn ")
                .or_else(|| t.strip_prefix("pub fn "))
                .or_else(|| t.strip_prefix("pub fn "))
            {
                let name = rest
                    .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .next()
                    .unwrap_or("")
                    .to_string();
                fns.push((name, Vec::new()));
            }
            if let Some(last) = fns.last_mut() {
                last.1.push(line);
            }
        }

        fn ident_after<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
            let at = line.find(marker)? + marker.len();
            let rest = &line[at..];
            let end = rest
                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            if end == 0 {
                None
            } else {
                Some(&rest[..end])
            }
        }
        fn mentions(line: &str, ident: &str) -> bool {
            let mut idx = 0;
            while let Some(hit) = line[idx..].find(ident) {
                let at = idx + hit;
                let before_ok = at == 0
                    || !line[..at]
                        .chars()
                        .next_back()
                        .is_some_and(|c| c.is_alphanumeric() || c == '_');
                let after = at + ident.len();
                let after_ok = line[after..]
                    .chars()
                    .next()
                    .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
                if before_ok && after_ok {
                    return true;
                }
                idx = at + ident.len();
            }
            false
        }

        let mut offenders: Vec<String> = Vec::new();
        for (name, body) in &fns {
            let Some(call_at) = body.iter().position(|l| {
                l.contains("ordered_snapshot_kv(") && !l.trim_start().starts_with("//")
            }) else {
                continue;
            };
            // The receiver handed to the `&mut` parameter is exempt.
            let receiver = ident_after(body[call_at], "&mut ").unwrap_or("");
            // Locals bound out of `args` before the call, plus `args` itself.
            let mut captured: Vec<&str> = vec!["args"];
            for (i, line) in body[..call_at].iter().enumerate() {
                if !line.contains("args") {
                    continue;
                }
                let after_let = match line.split_once("let ") {
                    Some((_, rest)) => rest.strip_prefix("mut ").unwrap_or(rest),
                    None => continue,
                };
                let end = after_let
                    .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .unwrap_or(after_let.len());
                let id = &after_let[..end];
                // A plain binding: `let id =` / `let id: T =`. A destructuring
                // `let Some(Value::Object(Some(this))) = ...` yields `Some`
                // followed by `(`, and is not a name anything reuses.
                if id.is_empty()
                    || !matches!(
                        after_let[end..].trim_start().chars().next(),
                        Some('=') | Some(':')
                    )
                {
                    continue;
                }
                // ...and not a binding whose own value expression CONTAINS the
                // call (`let vals = match args.first() { .. => ordered_snapshot_kv(..) }`):
                // that names the call's RESULT, which is not a pre-GC ref. The
                // statement is still open if no line before the call ended one.
                if !body[i..call_at].iter().any(|l| l.trim_end().ends_with(';')) {
                    continue;
                }
                captured.push(id);
            }
            let pinned_before: Vec<&str> = body[..call_at]
                .iter()
                .filter_map(|l| ident_after(l, "ctx.pin_native_root("))
                .collect();
            for id in captured {
                if id.is_empty() || id == receiver || pinned_before.contains(&id) {
                    continue;
                }
                let used_after = body[call_at + 1..]
                    .iter()
                    .any(|l| !l.trim_start().starts_with("//") && mentions(l, id));
                if used_after {
                    offenders.push(format!("{name} holds `{id}`"));
                }
            }
        }
        offenders.sort();
        offenders.dedup();
        assert!(
            offenders.is_empty(),
            "these carry a pre-GC reference across `ordered_snapshot_kv`, which \
             re-enters Java and can relocate it: {offenders:?}. Pin it BEFORE the \
             call and refresh through `read_native_pin` at each use — pinning \
             afterwards registers an address the collector may already have \
             vacated."
        );
    }

    #[test]
    fn only_order_insensitive_functions_read_the_unordered_snapshot() {
        const ALLOWED: &[&str] = &[
            // The definition itself, and its `&dyn`-context public re-export
            // (surefire copies entries into a CHM — order-insensitive).
            "snapshot_kv",
            "snapshot_sidetable",
            // The ordering machinery: reads the raw snapshot, then orders it.
            "ordered_snapshot_kv",
            // A value-membership test: `Properties.contains(Object)`.
            "native_properties_contains",
        ];
        let src = include_str!("../properties_sidetable.rs");
        let mut current = String::new();
        let mut offenders: Vec<String> = Vec::new();
        for line in src.lines() {
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed
                .strip_prefix("fn ")
                .or_else(|| trimmed.strip_prefix("pub fn "))
                .or_else(|| trimmed.strip_prefix("pub fn "))
            {
                current = rest
                    .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .next()
                    .unwrap_or("")
                    .to_string();
            }
            // A bare `snapshot_kv(` — i.e. not the `ordered_` variant, and not
            // this test's own literals.
            let mut idx = 0;
            while let Some(hit) = line[idx..].find("snapshot_kv(") {
                let at = idx + hit;
                let ordered = line[..at].ends_with("ordered_");
                let quoted = line.trim_start().starts_with("//") || line.contains('"');
                if !ordered && !quoted && current != "snapshot_kv" {
                    offenders.push(current.clone());
                }
                idx = at + "snapshot_kv(".len();
            }
        }
        offenders.sort();
        offenders.dedup();
        let unexpected: Vec<&String> = offenders
            .iter()
            .filter(|f| !ALLOWED.contains(&f.as_str()))
            .collect();
        assert!(
            unexpected.is_empty(),
            "these functions read the UNORDERED side-table snapshot: {unexpected:?}. \
             If the result is handed to Java as an iteration order, call \
             `ordered_snapshot_kv` instead; if it genuinely cannot be, add it to ALLOWED \
             with a note saying why."
        );
    }

    #[test]
    fn parse_simple_kv() {
        let p = lossy(parse_properties(b"key=value\n"));
        assert_eq!(p, vec![("key".to_string(), "value".to_string())]);
    }

    #[test]
    fn parse_multiple_kv() {
        let p = lossy(parse_properties(b"a=1\nb=2\nc=3\n"));
        assert_eq!(
            p,
            vec![
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "2".to_string()),
                ("c".to_string(), "3".to_string()),
            ]
        );
    }

    #[test]
    fn parse_comments_skipped() {
        let p = lossy(parse_properties(b"#comment\n!exclamation\nkey=value\n"));
        assert_eq!(p, vec![("key".to_string(), "value".to_string())]);
    }

    #[test]
    fn parse_blank_lines_skipped() {
        let p = lossy(parse_properties(b"\n\nkey=value\n\n"));
        assert_eq!(p, vec![("key".to_string(), "value".to_string())]);
    }

    #[test]
    fn parse_whitespace_separator() {
        let p = lossy(parse_properties(b"key value\n"));
        assert_eq!(p, vec![("key".to_string(), "value".to_string())]);
    }

    #[test]
    fn save_convert_escapes_specials() {
        // Keys escape every space; `=`,`:`,`#`,`!` and `\` always escape.
        assert_eq!(save_convert("a b", true, false), "a\\ b");
        assert_eq!(save_convert("a=b:c#d!e", true, false), "a\\=b\\:c\\#d\\!e");
        assert_eq!(save_convert("c:\\path", false, false), "c\\:\\\\path");
        // Values only escape a LEADING space, not interior ones.
        assert_eq!(save_convert(" lead mid", false, false), "\\ lead mid");
        assert_eq!(save_convert("a\tb\nc", false, false), "a\\tb\\nc");
    }

    #[test]
    fn save_convert_unicode_escaping() {
        // escape_unicode=true escapes non-Latin chars as one `\u` per code unit,
        // in UPPER-case hex: `Properties.saveConvert` indexes a `hexDigit[]` of
        // `'0'..'9','A'..'F'`. These three assertions were written from THIS
        // implementation rather than from the spec and pinned the lower-case
        // form for as long as it was wrong; `load` accepts either case, so
        // nothing but a byte comparison against a JDK-written file could see it
        // (MEASURED, apps/probes/PropertiesShadowSweep 118-119).
        assert_eq!(save_convert("\u{00e9}", false, true), "\\u00E9"); // é
                                                                      // Supplementary code point -> surrogate pair (two \u units).
        assert_eq!(save_convert("\u{1F600}", false, true), "\\uD83D\\uDE00");
        // escape_unicode=false leaves the char literal (Writer charset encodes it).
        assert_eq!(save_convert("\u{00e9}", false, false), "\u{00e9}");
    }

    #[test]
    fn save_convert_then_parse_roundtrips() {
        // Representative of a real system-property snapshot: paths, `=`/`:`,
        // spaces and backslashes must survive store(save_convert) -> load(parse).
        let pairs = [
            ("java.class.path", "C:\\a;C:/b:dir with space"),
            ("line.separator", "\r\n"),
            ("key with space", "v=a:l#u!e"),
            ("plain", "value"),
        ];
        let mut text = String::new();
        for (k, v) in &pairs {
            text.push_str(&save_convert(k, true, true));
            text.push('=');
            text.push_str(&save_convert(v, false, true));
            text.push('\n');
        }
        let parsed = lossy(parse_properties(text.as_bytes()));
        let expected: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn render_store_text_includes_date_before_first_entry() {
        let entries = kv(&[("code2", "message2"), ("code1", "message1")]);
        let text = render_store_text(
            None,
            Some("Thu Jan 01 00:00:00 UTC 1970"),
            &entries,
            true,
            "\n",
        );
        assert!(text.starts_with("#Thu Jan 01 00:00:00 UTC 1970\n"));
        assert!(
            text.contains("\ncode2=message2\n"),
            "first entry must be newline-prefixed by the date comment: {text:?}"
        );
    }

    #[test]
    fn render_store_text_keeps_user_comment_before_date() {
        let entries = kv(&[("key", "value")]);
        let text = render_store_text(Some("header"), Some("DATE"), &entries, true, "\r\n");
        assert_eq!(text, "#header\r\n#DATE\r\nkey=value\r\n");
    }

    #[test]
    fn parse_colon_separator() {
        let p = lossy(parse_properties(b"key:value\n"));
        assert_eq!(p, vec![("key".to_string(), "value".to_string())]);
    }

    #[test]
    fn parse_continuation_line() {
        let p = lossy(parse_properties(b"key=long\\\n    value\n"));
        assert_eq!(p, vec![("key".to_string(), "longvalue".to_string())]);
    }

    #[test]
    fn parse_unicode_escape() {
        let p = lossy(parse_properties(b"key=\\u00e9\n"));
        assert_eq!(p[0].0, "key");
        assert!(p[0].1.starts_with('\u{00e9}'));
    }

    #[test]
    fn parse_keycloak_version_shape() {
        let bytes = b"version=26.2.4\nbuild-time=2025-04-26T13:00:00Z\nresources-version=26.2.4\n";
        let p = lossy(parse_properties(bytes));
        assert_eq!(p.len(), 3);
        assert_eq!(p[0], ("version".to_string(), "26.2.4".to_string()));
        assert_eq!(
            p[1],
            ("build-time".to_string(), "2025-04-26T13:00:00Z".to_string())
        );
        assert_eq!(
            p[2],
            ("resources-version".to_string(), "26.2.4".to_string())
        );
    }

    #[test]
    fn parse_caps_at_max_props() {
        // Generate 12_000 lines; we should accept exactly MAX_PROPS_PER_OBJECT.
        let mut bytes = Vec::new();
        for i in 0..12_000 {
            bytes.extend_from_slice(format!("k{}=v{}\n", i, i).as_bytes());
        }
        let p = parse_properties(&bytes);
        assert_eq!(p.len(), MAX_PROPS_PER_OBJECT);
    }

    #[test]
    fn parse_rejects_oversized_key() {
        let mut bytes = b"k".to_vec();
        bytes.extend(std::iter::repeat(b'a').take(MAX_KV_LEN + 1));
        bytes.extend_from_slice(b"=v\n");
        let p = parse_properties(&bytes);
        assert!(p.is_empty(), "oversized key must be rejected");
    }

    #[test]
    fn parse_no_separator_yields_empty_value() {
        let p = lossy(parse_properties(b"keyonly\n"));
        assert_eq!(p, vec![("keyonly".to_string(), String::new())]);
    }

    #[test]
    fn unescape_backslash_n() {
        assert_eq!(unescape(r"\n"), "\n");
        assert_eq!(unescape(r"\t"), "\t");
        assert_eq!(unescape(r"\f"), "\u{000c}");
        assert_eq!(unescape(r"\\"), "\\");
        assert_eq!(unescape(r"\:"), ":");
        assert_eq!(unescape(r"\="), "=");
    }

    #[test]
    fn unescape_unicode() {
        assert_eq!(unescape("\\u0041"), "A");
        assert_eq!(unescape("\\u00e9"), "\u{00e9}");
    }

    #[test]
    fn unescape_surrogate_pair_combines_to_supplementary() {
        // U+1F600 GRINNING FACE is written as the surrogate pair
        // 😀 in a .properties file. The two halves must combine
        // into the single supplementary code point (previously BOTH halves
        // were dropped, yielding an empty string).
        assert_eq!(unescape("\\uD83D\\uDE00"), "\u{1F600}");
        // Surrounded by ordinary text, and with lowercase hex.
        assert_eq!(unescape("a\\ud83d\\ude00b"), "a\u{1F600}b");
    }

    #[test]
    fn unescape_lone_surrogate_becomes_replacement_not_dropped() {
        // `unescape` is the LOSSY Rust-text view, so U+FFFD is the right answer
        // HERE and only here: a Rust `String` cannot hold the unit. The store
        // keeps the unit — `unescape_units_keeps_a_lone_surrogate_as_itself`
        // below is the assertion that matters, and it is written in units
        // precisely because this one passes on the broken code too.
        assert_eq!(unescape("\\uD800"), "\u{FFFD}"); // lone high
        assert_eq!(unescape("\\uDC00"), "\u{FFFD}"); // lone low
                                                     // High surrogate followed by a NON-low escape: the high is replaced,
                                                     // and the trailing 'A' (A) is preserved.
        assert_eq!(unescape("\\uD800\\u0041"), "\u{FFFD}A");
    }

    // -----------------------------------------------------------------------
    // G55-1 — the surrogate the store could not hold, and the key it could not
    // find again. MEASURED against Adoptium 25.0.3+9-hotspot on 2026-08-17
    // (`scratchpad/g55/G55Probe.java`, 158 rows): 21 `Properties` rows diverged,
    // and they were two distinct defects with one cause — a `String`-typed
    // store.
    //
    // Every assertion below is written in UTF-16 UNITS. A test written through
    // `unescape`/`read_string`/`to_lossy` PASSES ON THE BROKEN CODE, which is
    // how this survived: `U+FFFD` is a perfectly good `char`, so a decoded
    // comparison of two mangled strings agrees with itself.
    // -----------------------------------------------------------------------

    const HI: u16 = 0xD800;
    const LO: u16 = 0xDC00;

    #[test]
    fn unescape_units_keeps_a_lone_surrogate_as_itself() {
        assert_eq!(
            unescape_inner("\\uD800", false).unwrap().units(),
            &[HI],
            "a lone high surrogate escape is ONE code unit, not U+FFFD"
        );
        assert_eq!(unescape_inner("\\uDC00", false).unwrap().units(), &[LO]);
        assert_eq!(
            unescape_inner("a\\uD800b", false).unwrap().units(),
            &[b'a' as u16, HI, b'b' as u16]
        );
        // A well-formed pair stays two units — and re-encodes to the single
        // supplementary character, which is what the lossy view still shows.
        let pair = unescape_inner("\\uD83D\\uDE00", false).unwrap();
        assert_eq!(pair.units(), &[0xD83D, 0xDE00]);
        assert_eq!(pair.to_lossy(), "\u{1F600}");
        // A high surrogate followed by a NON-low escape keeps both units.
        assert_eq!(
            unescape_inner("\\uD800\\u0041", false).unwrap().units(),
            &[HI, 0x41]
        );
    }

    #[test]
    fn parse_keeps_a_lone_surrogate_escape_as_its_unit() {
        let parsed = parse_properties(b"k\\ud800=v\\udc00\n");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0.units(), &[b'k' as u16, HI]);
        assert_eq!(parsed[0].1.units(), &[b'v' as u16, LO]);
    }

    /// **The silent one.** Two keys that differ only in WHICH unpaired
    /// surrogate they carry are two different Java keys — `equals` is false and
    /// the hashes differ. Under the `String` store both decoded to `U+FFFD`, so
    /// the second `setProperty` overwrote the first: MEASURED `size()` = 1 where
    /// HotSpot says 2, and `getProperty(HI)` answered the LO key's value. No
    /// exception, no wrong-looking string — just an entry that was never there.
    #[test]
    fn two_keys_differing_only_in_their_unpaired_surrogate_stay_two_keys() {
        let hi = JavaText::from_units(vec![HI]);
        let lo = JavaText::from_units(vec![LO]);
        assert_ne!(hi, lo, "distinct code units are distinct keys");
        assert_eq!(
            hi.to_lossy(),
            lo.to_lossy(),
            "...and the lossy views are EQUAL, which is exactly why a \
             `String`-keyed store conflated them"
        );

        let mut m = PropsMap::default();
        m.insert(hi.clone(), JavaText::from("high"));
        m.insert(lo.clone(), JavaText::from("low"));
        assert_eq!(m.len(), 2, "two keys, two entries");
        assert_eq!(m.get(&hi).map(|v| v.to_lossy()).as_deref(), Some("high"));
        assert_eq!(m.get(&lo).map(|v| v.to_lossy()).as_deref(), Some("low"));
        assert_eq!(
            m.shift_remove(&hi).map(|v| v.to_lossy()).as_deref(),
            Some("high"),
            "removing one must not take the other with it"
        );
        assert_eq!(m.len(), 1);
        assert_eq!(m.get(&lo).map(|v| v.to_lossy()).as_deref(), Some("low"));
    }

    /// `reorder_by` indexes the side-table by key. With `&str` keys the same
    /// conflation reached the enumeration order — one key would have shadowed
    /// the other's position and the survivor would have been emitted twice.
    #[test]
    fn reorder_does_not_conflate_two_lone_surrogate_keys() {
        let hi = JavaText::from_units(vec![HI]);
        let lo = JavaText::from_units(vec![LO]);
        let side = vec![
            (hi.clone(), JavaText::from("high")),
            (lo.clone(), JavaText::from("low")),
        ];
        let got = reorder_by(&side, &[lo.clone(), hi.clone()]);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0.units(), &[LO]);
        assert_eq!(got[0].1.to_lossy(), "low");
        assert_eq!(got[1].0.units(), &[HI]);
        assert_eq!(got[1].1.to_lossy(), "high");
    }

    /// `JavaText` orders by code UNIT, which is what `String.compareTo` does.
    /// Rust's `String` orders by code POINT, which puts a supplementary
    /// character AFTER `U+FFFF` where Java puts it before — so a `String`-keyed
    /// sorted view of the same content is in a different order than the JDK's.
    /// MEASURED on both VMs (`scratchpad/g55/G55Ord.java`).
    #[test]
    fn java_text_orders_by_code_unit_where_a_rust_string_orders_by_code_point() {
        let supplementary = JavaText::from("\u{10000}"); // == [D800, DC00]
        let ffff = JavaText::from_units(vec![0xFFFF]);
        assert_eq!(supplementary.units(), &[0xD800, 0xDC00]);
        assert!(
            supplementary < ffff,
            "Java: a surrogate unit (D800) is below U+FFFF, so the \
             supplementary character sorts FIRST"
        );
        assert!(
            "\u{10000}" > "\u{FFFF}",
            "Rust `String` sorts it LAST — the divergence this type removes"
        );
    }

    #[test]
    fn save_convert_units_writes_a_lone_surrogate_as_a_reloadable_escape() {
        // `escape_unicode = false` is the `store(Writer)` overload, where an
        // ordinary non-ASCII char is written literally — but a lone surrogate
        // still has to be escaped, because there is no `char` to write.
        assert_eq!(save_convert_units(&[HI], false, false), "\\uD800");
        assert_eq!(save_convert_units(&[LO], false, true), "\\uDC00");
        // Round-trip: what `store` writes, `load` reads back as the same units.
        let mut text = String::new();
        text.push_str(&save_convert_units(&[b'k' as u16, HI], true, true));
        text.push('=');
        text.push_str(&save_convert_units(&[b'v' as u16, LO], false, true));
        text.push('\n');
        let parsed = parse_properties(text.as_bytes());
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0.units(), &[b'k' as u16, HI]);
        assert_eq!(parsed[0].1.units(), &[b'v' as u16, LO]);
    }

    #[test]
    fn save_convert_still_writes_a_well_formed_pair_as_two_escapes() {
        // The supplementary path must not change: two `\u` escapes, not one.
        assert_eq!(
            save_convert_units(&[0xD83D, 0xDE00], false, true),
            "\\uD83D\\uDE00"
        );
    }

    /// The writer half. `create_property_string` sends well-formed text down
    /// the unchanged `create_string` path and only reroutes what a `&str`
    /// cannot carry — so this asserts the reroute actually preserves the unit,
    /// read back through `lang_string::read_string_chars` rather than through
    /// any `String`.
    #[test]
    fn create_property_string_materialises_an_unpaired_surrogate_intact() {
        let mut ctx = mock_ctx();
        let value = JavaText::from_units(vec![b'v' as u16, b'a' as u16, LO, b'b' as u16]);
        let obj = create_property_string(&mut ctx, &value);
        assert_eq!(
            crate::lang_string::read_string_chars(&ctx, obj),
            vec![b'v' as u16, b'a' as u16, LO, b'b' as u16],
            "the value must come back with its unpaired low surrogate intact"
        );
    }

    #[test]
    fn create_property_string_leaves_well_formed_text_on_the_unchanged_path() {
        let mut ctx = mock_ctx();
        let obj = create_property_string(&mut ctx, &JavaText::from("v\u{1F600}b"));
        assert_eq!(
            crate::lang_string::read_string_chars(&ctx, obj),
            vec![b'v' as u16, 0xD83D, 0xDE00, b'b' as u16]
        );
        // Well-formed content stays on `ctx.create_string`, so whatever
        // identity/interning behaviour the green `Properties` vectors already
        // measured is unchanged — the reroute is reached only by content a
        // `&str` cannot carry.
        assert!(!crate::lang_string::has_unpaired_surrogate(
            JavaText::from("v\u{1F600}b").units()
        ));
    }

    /// The reader half, on content a `&str` CAN carry — the mock's
    /// `read_string` is stricter than the VM's (it refuses an unpaired
    /// surrogate outright where the VM substitutes U+FFFD), so the guard, not
    /// the decode, is what this pins.
    #[test]
    fn read_java_text_reads_units_and_refuses_a_non_string() {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("k\u{00e9}");
        assert_eq!(
            read_java_text(&ctx, s).map(|t| t.units().to_vec()),
            Some(vec![b'k' as u16, 0x00e9])
        );
        let not_a_string = ctx.alloc_object(ClassId::new(0), 2);
        assert_eq!(read_java_text(&ctx, not_a_string), None);
    }

    #[test]
    fn unescape_short_u_decodes_digits_present_not_dropped() {
        // Fewer than 4 hex digits is malformed per the JDK. We decode the
        // digits actually present rather than silently swallowing them.
        assert_eq!(unescape("\\u41"), "A"); // 0x41 from 2 digits
        assert_eq!(unescape("\\u41Z"), "AZ"); // stops at non-hex 'Z'
                                              // `\u` with no hex digit at all: preserve the 'u' literally.
        assert_eq!(unescape("\\u"), "u");
        assert_eq!(unescape("\\uZ"), "uZ");
    }

    #[test]
    fn iso_8859_1_bytes_round_trips_latin1_and_collapses_above() {
        // ASCII: identity.
        assert_eq!(iso_8859_1_bytes("abc=1"), b"abc=1".to_vec());
        // 'é' is U+00E9 — fits in Latin-1 and survives as 0xE9.
        assert_eq!(iso_8859_1_bytes("é"), vec![0xE9]);
        // 'Ω' is U+03A9 — outside Latin-1, must collapse to '?'.
        assert_eq!(iso_8859_1_bytes("Ω"), vec![b'?']);
        // Mixed: ASCII + Latin-1 + above-Latin-1 in one string.
        assert_eq!(iso_8859_1_bytes("kéΩ"), vec![b'k', 0xE9, b'?']);
        // The downgraded byte sequence for a Latin-1 line must
        // round-trip through `parse_properties` cleanly.
        let mut bytes = iso_8859_1_bytes("name=café\n");
        // Trailing newline preserved.
        assert_eq!(bytes.last(), Some(&b'\n'));
        bytes.push(0); // sanity: ensure Vec is mutable / well-formed
        bytes.pop();
        let parsed = lossy(parse_properties(&iso_8859_1_bytes("name=café\n")));
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0, "name");
        // `parse_properties` decodes 0xE9 back to U+00E9, so the
        // value reads as "café" again.
        assert_eq!(parsed[0].1, "café");
    }

    #[test]
    fn split_kv_separator_priority() {
        assert_eq!(
            split_key_value("key=value"),
            ("key".to_string(), "value".to_string())
        );
        assert_eq!(
            split_key_value("key:value"),
            ("key".to_string(), "value".to_string())
        );
        assert_eq!(
            split_key_value("key value"),
            ("key".to_string(), "value".to_string())
        );
        assert_eq!(
            split_key_value("key  =  value"),
            ("key".to_string(), "value".to_string())
        );
    }

    // gen r4w2/remap (2026-09-23): the relocated receiver must reach the CALLER.
    //
    // `publish_real_system_props` stored, and `native_system_get_properties*`
    // returned, the `Properties` singleton they held from BEFORE
    // `replace_real_map` ran Java. A moving young cycle inside the fill
    // relocated the (pinned) singleton, and `System.props` / the native's
    // return value named the vacated from-space address -- the boot crash in
    // `StaticProperty.<clinit>` under `CRATONVM_DBG=gc-stress=250000`. The mock
    // stands the collection in with `remap_native_pin_addr_for_test` from inside
    // the first upcall, exactly as `t19_h5_arfu_update_rereads_pins_after_operator_gc`
    // does for the atomic updaters.
    static RRM_THIS_OLD: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static RRM_THIS_NEW: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn relocating_key_set(
        ctx: &mut MockNativeContext,
        _receiver: ObjectRef,
        method_name: &str,
        _descriptor: &str,
        _args: &[Value],
    ) -> Option<MethodCallResult> {
        if method_name != "keySet" {
            return None;
        }
        ctx.remap_native_pin_addr_for_test(
            RRM_THIS_OLD.load(std::sync::atomic::Ordering::SeqCst),
            RRM_THIS_NEW.load(std::sync::atomic::Ordering::SeqCst),
        );
        // No key set: the prune reports trouble, the fill is incomplete, and
        // the function still has to leave through the write-back.
        Some(Ok(None))
    }

    #[test]
    fn replace_real_map_hands_the_caller_the_relocated_receiver() {
        let mut ctx = mock_ctx();
        let this_old = ctx.fresh_object_ref();
        let this_new = ctx.fresh_object_ref();
        let map = ctx.fresh_object_ref();
        // `fresh_object_ref` objects are class 0; give that class a `map` field
        // at slot 0 so the receiver reads as having a pre-existing real map.
        ctx.set_declared_fields(
            ClassId::new(0),
            vec![FieldMetadata {
                name: "map".to_string(),
                descriptor: "Ljava/util/concurrent/ConcurrentHashMap;".to_string(),
                access_flags: 0,
                slot_index: 0,
                declaring_class_id: ClassId::new(0),
                is_static: false,
            }],
        );
        ctx.set_field(this_old, 0, Value::Object(Some(map)));
        ctx.set_field(this_new, 0, Value::Object(Some(map)));
        RRM_THIS_OLD.store(
            this_old.as_ptr() as usize,
            std::sync::atomic::Ordering::SeqCst,
        );
        RRM_THIS_NEW.store(
            this_new.as_ptr() as usize,
            std::sync::atomic::Ordering::SeqCst,
        );
        ctx.set_invoke_virtual_hook(relocating_key_set);

        let mut this = this_old;
        let written = replace_real_map(&mut ctx, &mut this, &[]);

        assert_eq!(written, 0, "an unenumerable map is an incomplete fill");
        assert_eq!(
            this.as_ptr() as usize,
            this_new.as_ptr() as usize,
            "the caller must get the receiver's post-collection address back"
        );
        // The incomplete-fill reset went through the pin to the NEW address,
        // and the vacated copy was not written.
        assert_eq!(ctx.get_field(this_new, 0), Value::Object(None));
        assert_eq!(ctx.get_field(this_old, 0), Value::Object(Some(map)));
        assert_eq!(ctx.native_pin_count_for_test(), 0, "pins balanced");
    }
