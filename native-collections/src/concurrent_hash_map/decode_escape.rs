#[allow(unused_imports)]
use super::*;

/// Decode a single backslash-escape starting at `bytes[0] == b'\\'`.
/// Returns `(decoded_string, bytes_consumed_including_backslash)`.
/// Falls back to the literal trailing char for unknown escapes (matches
/// JDK behaviour: `\q` parses as `q`).
pub(crate) fn decode_escape(bytes: &[u8]) -> (String, usize) {
    debug_assert_eq!(bytes[0], b'\\');
    if bytes.len() < 2 {
        return (String::new(), 1);
    }
    match bytes[1] {
        b't' => ("\t".to_string(), 2),
        b'n' => ("\n".to_string(), 2),
        b'r' => ("\r".to_string(), 2),
        b'f' => ("\x0c".to_string(), 2),
        b'\\' => ("\\".to_string(), 2),
        b'"' => ("\"".to_string(), 2),
        b'\'' => ("'".to_string(), 2),
        b'u' => {
            // \uXXXX (exactly 4 hex digits per JDK spec).
            if bytes.len() < 6 {
                return ((bytes[1] as char).to_string(), 2);
            }
            let hex = std::str::from_utf8(&bytes[2..6]).unwrap_or("");
            match u32::from_str_radix(hex, 16) {
                Ok(cp) => match char::from_u32(cp) {
                    Some(ch) => (ch.to_string(), 6),
                    // Surrogate halves and other invalid code points: fall
                    // through to literal 'u' so we never lose data.
                    None => ("u".to_string(), 2),
                },
                Err(_) => ("u".to_string(), 2),
            }
        }
        // Per spec: any other char after `\` (including space, `=`, `:`,
        // and unknown letters) becomes itself, allowing them inside keys.
        other => {
            // Multi-byte UTF-8 starting at byte index 1.
            let end = utf8_char_end(bytes, 1);
            let s = std::str::from_utf8(&bytes[1..end])
                .map(|s| s.to_string())
                .unwrap_or_else(|_| (other as char).to_string());
            (s, end)
        }
    }
}

/// Return the byte-index just past the UTF-8 char that begins at `start`,
/// or `start + 1` for invalid sequences (safe fallback).
pub(crate) fn utf8_char_end(bytes: &[u8], start: usize) -> usize {
    if start >= bytes.len() {
        return start;
    }
    let b = bytes[start];
    let len = if b < 0x80 {
        1
    } else if b < 0xC0 {
        1 // continuation byte — should not start a char; treat as 1
    } else if b < 0xE0 {
        2
    } else if b < 0xF0 {
        3
    } else {
        4
    };
    (start + len).min(bytes.len())
}

/// Helper: read all text from a stream or reader object.
pub(crate) fn props_read_input(ctx: &mut dyn NativeContext, stream: ObjectRef) -> String {
    // Try BAIS protocol: field 0 = byte[], field 1 = pos, field 2 = mark, field 3 = count
    if let Value::Object(Some(data_arr)) = ctx.get_field(stream, 0) {
        let pos = match ctx.get_field(stream, 1) {
            Value::Int(p) => p as usize,
            _ => 0,
        };
        let count = match ctx.get_field(stream, 3) {
            Value::Int(c) => c as usize,
            _ => 0,
        };
        let mut bytes = Vec::with_capacity(count.saturating_sub(pos));
        for i in pos..count {
            if let Value::Int(b) = ctx.get_array_element(data_arr, i) {
                bytes.push(b as u8);
            }
        }
        return String::from_utf8_lossy(&bytes).into_owned();
    }
    // Fallback: try to read field 0 as a string directly (e.g. BufferedReader)
    if let Some(s) = ctx.read_string(stream) {
        return s;
    }
    String::new()
}

/// Store properties to an output stream as "key=value" lines.
pub(crate) fn native_props_store(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let mut output = String::new();

    // Optional comment header — mirror JDK `Properties.writeComments`: the
    // comment is prefixed with a bare `#` (no space), and an embedded newline
    // starts a fresh `#` comment line. The previous `# {}` form inserted a
    // spurious space; more importantly the entry loop below was broken (see
    // next comment), so a save/load round-trip such as H2's `FileLock`
    // watchdog (`AUTO_SERVER=TRUE`) read back an empty file and aborted with
    // "Concurrent update".
    if let Value::Object(Some(comment)) = args[2] {
        if let Some(c) = ctx.read_string(comment) {
            output.push('#');
            for ch in c.chars() {
                match ch {
                    '\n' | '\r' => {
                        output.push('\n');
                        output.push('#');
                    }
                    _ => output.push(ch),
                }
            }
            output.push('\n');
        }
    }

    // Entries. Use the canonical, layout-aware collector that already backs
    // `entrySet()`/`toString()` (`map_collect_entries`) rather than the old
    // slot-0-only `props_collect_keys` + per-key `get`. `props_collect_keys`
    // read field 0 of each bucket node as the key — correct for legacy nodes
    // (key=0) but NOT for real-JDK-created nodes (hash=0, key=1), where field
    // 0 is the primitive `int` hash. So every JDK-layout entry was silently
    // dropped and `store` wrote only the comment. `map_collect_entries`
    // resolves the real `this.map` ConcurrentHashMap backing / CHM/LHM
    // receivers / slot-0 buckets and uses the layout-aware node accessors.
    // Keys and values are escaped per JDK `Properties.saveConvert` so the
    // output round-trips through our (faithful) `Properties.load`.
    for (k, v) in map_collect_entries(ctx, this) {
        let key_str = match k {
            Value::Object(Some(o)) => match ctx.read_string(o) {
                Some(s) => s,
                None => continue,
            },
            _ => continue,
        };
        let val_str = match v {
            Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
            _ => String::new(),
        };
        output.push_str(&props_save_convert(&key_str, true));
        output.push('=');
        output.push_str(&props_save_convert(&val_str, false));
        output.push('\n');
    }

    // Write to output stream.
    //
    // MED fix: the previous implementation issued one `write(I)V` virtual
    // dispatch per byte — for a Properties with ~1000 entries this meant
    // ~50 000 virtual calls. Build a single `byte[]` once and call
    // `write([B)V` exactly once per line, which most OutputStream
    // implementations override to do a single bulk copy. We dispatch
    // line-by-line rather than the entire buffer in one go so a
    // pathological output stream (e.g. one that requires a flush between
    // lines) still gets reasonable behaviour, and so each line's bytes
    // fit easily into a single allocation.
    if let Value::Object(Some(ostream)) = args[1] {
        // gc-common w20-a: every line allocates its byte[] and runs `write`
        // (Java); the stream was a bare copy of the argument, dispatched on at
        // its entry address for every line. Pinned for the loop.
        let ostream_pin = ctx.pin_native_root(ostream);
        for line in output.split_inclusive('\n') {
            let bytes = line.as_bytes();
            let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
            for (i, &b) in bytes.iter().enumerate() {
                // Byte array slots hold sign-extended Int per the VM's
                // primitive-array value model.
                ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
            }
            // Prefer the bulk `write([B)V` overload. If a stream does
            // not implement it, the JDK default (AbstractOutputStream)
            // falls back to per-byte `write(I)V`, so we keep behaviour
            // even on minimal stream impls.
            // Read back AFTER the byte[]'s allocation, just before the call.
            let ostream = ctx.read_native_pin(ostream_pin, ostream);
            let _ = ctx.invoke_virtual(ostream, "write", "([B)V", &[Value::Object(Some(arr))]);
        }
        ctx.unpin_native_roots(ostream_pin);
    }
    Ok(None)
}

/// Collect all key ObjectRefs from a Properties' backing store.
///
/// Delegates to the canonical `map_collect_keys` (which resolves the real
/// `this.map` ConcurrentHashMap backing, CHM/LHM receivers, and slot-0
/// buckets, all with the layout-aware node accessors) and keeps only the
/// object-reference keys. The previous body read field 0 of each bucket node
/// directly, which is the key only for legacy nodes (key=0); for real-JDK
/// nodes (hash=0, key=1) field 0 is the primitive hash, so keys were dropped
/// — the same defect that left `Properties.store` writing zero entries.
pub(crate) fn props_collect_keys(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<ObjectRef> {
    map_collect_keys(ctx, this)
        .into_iter()
        .filter_map(|v| match v {
            Value::Object(Some(o)) => Some(o),
            _ => None,
        })
        .collect()
}

/// Escape a key or value for `Properties.store` output, mirroring JDK
/// `Properties.saveConvert` for the `store(OutputStream, …)` path (i.e. with
/// Unicode escaping enabled). Keys pass `escape_space = true` (every space is
/// escaped); values pass `false` (only a leading space is escaped). The
/// metacharacters `\ \t \n \r \f = : # !` are always escaped, and any
/// control / non-ASCII character is emitted as `\\uXXXX` UTF-16 unit(s), so
/// the result parses back to the original via `props_parse_logical_lines`.
pub(crate) fn props_save_convert(s: &str, escape_space: bool) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for (i, ch) in s.chars().enumerate() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{000C}' => out.push_str("\\f"),
            ' ' => {
                if i == 0 || escape_space {
                    out.push('\\');
                }
                out.push(' ');
            }
            '=' | ':' | '#' | '!' => {
                out.push('\\');
                out.push(ch);
            }
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{:04X}", unit));
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// propertyNames() -> returns a real Enumeration<Object> over the property keys.
///
/// Previously this delegated to `keySet`, which returns a `HashSet`. HashSet
/// doesn't implement `Enumeration`, so callers like Kafka's
/// `Utils.propsToMap` that invoke `hasMoreElements()` / `nextElement()` on
/// the result fell through to the snapshot-iterator native's generic-Iterator
/// fallback, which couldn't find a `hasNext` method on HashSet and silently
/// returned 0 — yielding an empty enumeration and a Properties-derived map
/// with zero entries. Kafka then reported `Missing required configuration
/// "process.roles"` even though server.properties listed it.
///
/// Now we allocate the same 2-field snapshot object the Enumeration interface
/// natives expect (field 0 = `Object[]` snapshot, field 1 = `int` cursor).
pub(crate) fn native_props_property_names(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // JDK 25 `Properties` stores entries in a private `ConcurrentHashMap map`
    // field, NOT in the inherited `Hashtable.table` array. Real-JDK
    // `Properties.size()` / `propertyNames()` / `entrySet()` bytecode all
    // route through `this.map`. Walk that map when present; fall back to the
    // legacy slot-0 backing for synthetic Properties allocations.
    let backing = match ctx.resolve_field_index("java/util/Properties", "map") {
        Some(slot) if slot < ctx.object_num_fields(this) => match ctx.get_field(this, slot) {
            Value::Object(Some(m)) => Some(m),
            _ => None,        },
        _ => None,
    };
    let keys = match backing {
        Some(m) => {
            // `Properties.map` is declared as `ConcurrentHashMap`. Use the
            // CHM-segmented key collector — `map_collect_keys` walks slot 0
            // as a plain Node[] and only finds the head segment objects on
            // a real-JDK CHM, yielding the wrong key type.
            let cid = ctx.class_id_of_object(m);
            let cn = ctx.class_name_of_id(cid).unwrap_or_default();
            if cn == "java/util/concurrent/ConcurrentHashMap"
                || cn.starts_with("java/util/concurrent/ConcurrentHashMap$")
            {
                chm_collect_all_keys(ctx, m)
            } else {
                map_collect_keys(ctx, m)
            }
        }
        None => map_collect_keys(ctx, this),
    };
    // Use our internal SnapshotEnumeration synthetic class so the
    // snapshot-iterator natives (`hasMoreElements`/`nextElement` registered on
    // this class name) win virtual dispatch. Field 0 = Object[] snapshot,
    // field 1 = cursor. Built through the pin-hardened helper — the
    // array/enumeration allocations can move the just-collected keys before
    // the raw stores (cceres5, see make_static_entry_set).
    let en = make_snapshot_enumeration(ctx, &keys)?;
    Ok(Some(Value::Object(Some(en))))
}

/// stringPropertyNames() -> Set<String>
///
/// # A FRESH set, not the receiver's own `keySet()` view
///
/// This used to take `native_map_key_set(this)` and then `add` each of the
/// defaults' keys to it. `native_map_key_set` returns a CACHED LIVE VIEW --
/// `cached_live_view(.., VIEW_KIND_KEYSET)`, the same instance `keySet()` hands
/// out, which is why `map.keySet() == map.keySet()` holds here as it does on
/// HotSpot. Adding to it therefore did two wrong things at once:
///
/// * it merged the DEFAULTS' keys into the view the receiver returns from
///   `keySet()`, which must not walk the chain -- `size()` and `keySet()`
///   deliberately see only this map's own entries, and only
///   `stringPropertyNames()` walks;
/// * it mutated a set the caller may already be holding from an earlier call.
///
/// In synthetic-JDK mode it did not even get that far: `native_hs_add` on a
/// view carrier raises `UnsupportedOperationException`, so
/// `stringPropertyNames()` threw outright on any `Properties` with a defaults
/// chain. That is what `probes/CollectionSlotFloor.java`'s
/// `Properties chain names` section reports.
///
/// Building one fresh set from the union fixes all three. `make_hashset_with_elements`
/// does the de-duplication (by `hashCode`/`equals`, so an overridden key in the
/// child shadows the parent's exactly once) and gives the set the real
/// `HashSet` -> `HashMap` layout that JDK bytecode reads through.
pub(crate) fn native_props_string_property_names(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Collect first, allocate second: everything in this loop is a field or
    // table READ, so no key gathered here can move before `make_hashset_with_elements`
    // pins them all.
    let mut keys: Vec<Value> = props_collect_keys(ctx, this)
        .into_iter()
        .map(|k| Value::Object(Some(k)))
        .collect();
    let mut defaults_val = props_get_defaults(ctx, this);
    // Bounded: a `Properties` whose `defaults` chain loops back on itself is
    // malformed, but it is reachable from application code
    // (`p.defaults = p` via a subclass), and a hang inside a bootstrap-path
    // native is the worst way to report it. The JDK's own walk is unbounded;
    // 64 is far past any real chain.
    for _ in 0..64 {
        let Value::Object(Some(defs)) = defaults_val else {
            break;
        };
        keys.extend(
            props_collect_keys(ctx, defs)
                .into_iter()
                .map(|k| Value::Object(Some(k))),
        );
        defaults_val = props_get_defaults(ctx, defs);
    }
    Ok(Some(Value::Object(Some(make_hashset_with_elements(
        ctx, &keys,
    )?))))
}

// ===========================================================================
// Unmodifiable collection views
// ===========================================================================
//
// `Collections.unmodifiableList/Set/Map/Collection(...)` return wrapper views
// that delegate every read to the backing collection and throw
// `UnsupportedOperationException` from every mutator. `List.of` / `Set.of` /
// `Map.of` produce fully immutable snapshots (the backing collection is
// private, so it can never be mutated).
//
// Each wrapper is a dedicated synthetic class with a single field (slot 0)
// holding the backing collection's ObjectRef. Read methods forward to the
// backing object via `invoke_virtual`, so dispatch lands on whatever native
// implements the concrete backing type (ArrayList / HashMap / HashSet / ...).
// Mutators are registered natives that unconditionally throw.

/// Synthetic class names for the unmodifiable wrappers.
pub(crate) const UNMOD_LIST_CLASS: &str = "cratonvm/internal/UnmodifiableList";
pub(crate) const UNMOD_SET_CLASS: &str = "cratonvm/internal/UnmodifiableSet";
pub(crate) const UNMOD_SORTED_SET_CLASS: &str = "cratonvm/internal/UnmodifiableSortedSet";
pub(crate) const UNMOD_NAVIGABLE_SET_CLASS: &str = "cratonvm/internal/UnmodifiableNavigableSet";
pub(crate) const UNMOD_MAP_CLASS: &str = "cratonvm/internal/UnmodifiableMap";
pub(crate) const UNMOD_COLLECTION_CLASS: &str = "cratonvm/internal/UnmodifiableCollection";
pub(crate) const UNMOD_ITR_CLASS: &str = "cratonvm/internal/UnmodifiableItr";
pub(crate) const UNMOD_LIST_ITR_CLASS: &str = "cratonvm/internal/UnmodifiableListItr";
/// `entrySet()` of an unmodifiable/immutable map. Distinct from
/// [`UNMOD_SET_CLASS`] because its elements are `Map.Entry` — its
/// `iterator()`/`forEach()` wrap each entry in [`UNMOD_MAP_ENTRY_CLASS`] so
/// `setValue()` throws instead of mutating the backing map, matching the
/// JDK's `Collections$UnmodifiableMap$UnmodifiableEntrySet`.
pub(crate) const UNMOD_ENTRY_SET_CLASS: &str = "cratonvm/internal/UnmodifiableEntrySet";
/// Iterator over an [`UNMOD_ENTRY_SET_CLASS`] whose `next()` wraps the
/// backing iterator's `Map.Entry` in [`UNMOD_MAP_ENTRY_CLASS`].
pub(crate) const UNMOD_ENTRY_ITR_CLASS: &str = "cratonvm/internal/UnmodifiableEntryItr";
/// Read-only `Map.Entry` view: `getKey`/`getValue`/`toString`/`hashCode`/
/// `equals` delegate to the backing (real, mutable) entry; `setValue` throws.
pub(crate) const UNMOD_MAP_ENTRY_CLASS: &str = "cratonvm/internal/UnmodifiableMapEntry";

/// Slot 0 of every wrapper holds the backing collection / iterator.
pub(crate) const UNMOD_FIELD_BACKING: usize = 0;

/// Slot 1 distinguishes the *immutable* `List.of`/`Set.of`/`Map.of`/`copyOf`
/// factories (`Int(1)`) from the *unmodifiable* `Collections.unmodifiable*`
/// wrappers (default). Both share the same `cratonvm/internal/Unmodifiable*`
/// stamp but report different concrete classes from `getClass()`
/// (`ImmutableCollections$*` vs `Collections$Unmodifiable*`). Set only by
/// `freeze_result`. CONTRACT: this slot index is mirrored by
/// `getclass_immutable_marker` in native-builtins (the `getClass()` display
/// map); keep the two in sync.
pub(crate) const UNMOD_FIELD_IMMUTABLE: usize = 1;

/// Slot 2 of a map-VIEW wrapper: set when the map it is a view OF came from an
/// immutable factory.
///
/// Deliberately NOT `UNMOD_FIELD_IMMUTABLE`. That slot is a cross-crate
/// contract -- `getclass_immutable_marker` in native-builtins reads it to
/// decide whether `getClass()` says `ImmutableCollections$*` or
/// `Collections$Unmodifiable*` -- and a `Map.of` keySet is NEITHER on HotSpot:
/// it is `java.util.AbstractMap$1`. Widening that marker would make
/// `getClass()` answer a third wrong name, and `unmod_is_immutable` also gates
/// RULE I's null-query NPE, which this measurement says nothing about.
///
/// So this is a narrow flag with a single reader.
pub(crate) const UNMOD_FIELD_IMMUTABLE_VIEW: usize = 2;

/// Whether `this` is a view of a map that came from `Map.of`/`Map.copyOf`.
pub(crate) fn unmod_is_immutable_map_view(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    ctx.object_num_fields(this) > UNMOD_FIELD_IMMUTABLE_VIEW
        && matches!(
            ctx.get_field(this, UNMOD_FIELD_IMMUTABLE_VIEW),
            Value::Int(1)
        )
}

pub(crate) fn is_unmod_set_class(name: &str) -> bool {
    matches!(
        name,
        UNMOD_SET_CLASS
            | UNMOD_SORTED_SET_CLASS
            | UNMOD_NAVIGABLE_SET_CLASS
            | UNMOD_ENTRY_SET_CLASS
    )
}

/// Build an `UnsupportedOperationException` error for a blocked mutator.
pub(crate) fn unsupported_op() -> MethodCallFailed {
    RuntimeError::UnsupportedOperationException {
        message: String::new(),
    }
    .into()
}

/// Allocate an unmodifiable wrapper of `class_name` around `backing`.
///
/// Two slots: slot 0 = backing collection (read by every wrapper native), slot
/// 1 = the [`UNMOD_FIELD_IMMUTABLE`] marker, left at its default here and set to
/// `Int(1)` only by `freeze_result` for the immutable `*.of`/`copyOf` factories.
///
/// Fallible since 2026-08-05 (Real-JDK wave 2, lane L7). The seven
/// `cratonvm/internal/Unmodifiable*` views stand in for
/// `java.util.Collections$Unmodifiable*` and the `ImmutableCollections$*`
/// family, whose real bytecode is not running — a compatibility substitution
/// contract §5 forbids under `--real-jdk`, whatever the stand-in is named. It
/// is refused there, as a catchable `NoClassDefFoundError` naming the class,
/// instead of being recorded as a violation and then performed anyway.
pub(crate) fn alloc_unmod_wrapper(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    backing: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let backing_pin = ctx.pin_native_root(backing);
    // Not `?`: the pin above is this frame's base and must be released before
    // unwinding, or it and everything pinned above it are stranded.
    let wrapper = match try_alloc_synthetic(ctx, class_name, 2) {
        Ok(wrapper) => wrapper,
        Err(err) => {
            ctx.unpin_native_roots(backing_pin);
            return Err(err);
        }
    };
    let backing = ctx.read_native_pin(backing_pin, backing);
    ctx.set_field(wrapper, UNMOD_FIELD_BACKING, Value::Object(Some(backing)));
    ctx.unpin_native_roots(backing_pin);
    Ok(wrapper)
}

/// Allocate an *immutable*-collection wrapper (the `List.of`/`Set.of`/`Map.of`/
/// `copyOf` factories) — an unmodifiable wrapper with the [`UNMOD_FIELD_IMMUTABLE`]
/// marker set, so `getClass()` reports the `java.util.ImmutableCollections$*`
/// family rather than `Collections$Unmodifiable*`.
///
/// # `H0-2` §4's twelve `instanceof` cells cannot be fixed by a retag
///
/// H4-1, 2026-08-20, source-verified. `H0-2` §5 proposes moving the map/set
/// cluster so that "`Map.of(...)` returns a real `ImmutableCollections$Map1`
/// and `is_subclass_of` walks a real chain". A `NativeKind` change cannot do
/// that, in either direction:
///
/// * **In `--real-jdk` the producers are ALREADY refused.** Every allocator of
///   a `cratonvm/internal/Unmodifiable*` — `register_factory_natives`
///   (`set_category(SyntheticStub)` at its own head), the six `unmodifiable*`
///   rows in `register_collections_extras_natives` (explicit inner
///   `SyntheticStub` window) and the three `copyOf` rows (explicit
///   `register_with_kind(..., SyntheticStub)`) — is dropped by
///   `NativeMethodRegistry::register_inner`'s `JdkOnly` refusal arm, so strict
///   mode never reaches this function and already answers all twelve cells
///   from real bytecode. `HANDOFF-20260819.md` §6 records the corroborating
///   arm result: `RImmutableFactoryTypes` is one of the SUITE=all five and the
///   `--real-jdk` arm over the same 102 vectors is 102/102.
/// * **In `Compatible` the kind is discarded.** `NativeKind::allowed_in`
///   returns an unconditional `true` for `Compatible`, and the only
///   Compatible-mode kind test is `invoke_or_native`'s `real_protected_stub`
///   arm, gated on `real_protected_stub_class` — a twelve-entry allow-list
///   that contains no `java/util` collection and no `cratonvm/internal/*`
///   class. So no tag on any registrar changes what this function does under
///   `--real-jdk`.
///
/// Restated as the rule: `HANDOFF-20260819.md` §1's trap runs both ways.
/// Retiring stubs moves compatible mode by zero; **retagging moves compatible
/// mode by zero too.** `H0-2` §4 is a compatible-mode measurement, so its
/// remedy has to be a compatible-mode change — retiring this carrier at its
/// producers, or `P4A` N1b option (c). Not a tag.
pub(crate) fn alloc_immutable_wrapper(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    backing: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let wrapper = alloc_unmod_wrapper(ctx, class_name, backing)?;
    ctx.set_field(wrapper, UNMOD_FIELD_IMMUTABLE, Value::Int(1));
    Ok(wrapper)
}

/// Allocate a wrapper for one of a map's three VIEWS, carrying
/// [`UNMOD_FIELD_IMMUTABLE_VIEW`] when the map itself was immutable.
pub(crate) fn alloc_unmod_view_wrapper(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    backing: ObjectRef,
    from_immutable: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    if !from_immutable {
        return alloc_unmod_wrapper(ctx, class_name, backing);
    }
    let backing_pin = ctx.pin_native_root(backing);
    let wrapper = match try_alloc_synthetic(ctx, class_name, 3) {
        Ok(w) => w,
        Err(err) => {
            ctx.unpin_native_roots(backing_pin);
            return Err(err);
        }
    };
    let backing = ctx.read_native_pin(backing_pin, backing);
    ctx.set_field(wrapper, UNMOD_FIELD_BACKING, Value::Object(Some(backing)));
    ctx.set_field(wrapper, UNMOD_FIELD_IMMUTABLE_VIEW, Value::Int(1));
    ctx.unpin_native_roots(backing_pin);
    Ok(wrapper)
}

/// Read the backing collection out of a wrapper. Recurses if the backing is
/// itself a wrapper (e.g. `unmodifiableList(unmodifiableList(x))`).
pub(crate) fn unmod_backing(ctx: &mut dyn NativeContext, wrapper: ObjectRef) -> Option<ObjectRef> {
    match ctx.get_field(wrapper, UNMOD_FIELD_BACKING) {
        Value::Object(Some(b)) => Some(b),
        _ => None,
    }
}

/// Forward a read-only call to the backing collection. If the wrapper's
/// backing is missing, returns a benign default.
pub(crate) fn unmod_delegate(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    descriptor: &str,
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let backing = match unmod_backing(ctx, this) {
        Some(b) => b,
        None => return Ok(Some(Value::Object(None))),
    };
    ctx.invoke_virtual(backing, method, descriptor, &args[1..])}

/// Forward a read-only NavigableMap/SortedMap *view* call to the backing
/// (e.g. `headMap`, `subMap`, `descendingMap`) and re-wrap the returned map as
/// unmodifiable, mirroring the JDK's UnmodifiableNavigableMap which returns
/// unmodifiable submaps. A non-map / null result is passed through unchanged.
pub(crate) fn unmod_delegate_rewrap_map(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    descriptor: &str,
) -> MethodCallResult {
    let res = unmod_delegate(ctx, args, method, descriptor)?;
    match res {
        Some(Value::Object(Some(m))) => Ok(Some(Value::Object(Some(alloc_unmod_wrapper(
            ctx,
            UNMOD_MAP_CLASS,
            m,
        )?)))),
        other => Ok(other),
    }
}

/// Forward a read-only NavigableSet/SortedSet *view* call to the backing
/// (e.g. `tailSet`, `headSet`, `subSet`, `descendingSet`) and re-wrap the
/// returned set as unmodifiable, mirroring the JDK's `UnmodifiableNavigableSet`
/// which returns unmodifiable sub-views. A non-set / null result is passed
/// through unchanged.
pub(crate) fn unmod_delegate_rewrap_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    descriptor: &str,
    wrapper_class: &str,
) -> MethodCallResult {
    let res = unmod_delegate(ctx, args, method, descriptor)?;
    match res {
        // Sub-views (tailSet/headSet/subSet/descendingSet) are only reachable
        // starting from an already-sorted/navigable set, and the JDK's own
        // UnmodifiableNavigableSet returns further NavigableSet-typed
        // sub-views — so re-wrap with UNMOD_SORTED_SET_CLASS (NOT the plain
        // UNMOD_SET_CLASS), keeping the SortedSet/NavigableSet surface (e.g.
        // comparator()) available on the result the same way the JDK does.
        Some(Value::Object(Some(s))) => Ok(Some(Value::Object(Some(alloc_unmod_wrapper(
            ctx,
            wrapper_class,
            s,
        )?)))),
        other => Ok(other),
    }
}

pub(crate) fn unmod_delegate_rewrap_sorted_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    descriptor: &str,
) -> MethodCallResult {
    unmod_delegate_rewrap_set(ctx, args, method, descriptor, UNMOD_SORTED_SET_CLASS)
}

pub(crate) fn unmod_delegate_rewrap_navigable_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    descriptor: &str,
) -> MethodCallResult {
    unmod_delegate_rewrap_set(ctx, args, method, descriptor, UNMOD_NAVIGABLE_SET_CLASS)
}

pub(crate) fn register_unmodifiable_natives(r: &mut NativeMethodRegistry) {
        // `SyntheticStub`, not `Bridge` (Real-JDK wave 2, lane L7 residual R1,
    // 2026-08-05). Every factory here returns a `cratonvm/internal/*` stand-in
    // for a `java.util.Collections$Unmodifiable*` / `ImmutableCollections$*` /
    // comparator object, and every method it registers on those stand-ins only
    // exists to serve one. `Bridge` asserts "no working real-bytecode fallback
    // exists"; for this family there plainly is one, in `java.base`, and it
    // works even though the backing collection is native-backed — a real
    // unmodifiable wrapper *delegates* every call to the map it was handed, and
    // that map's natives still answer. (Contrast `HashSet.iterator()`, which
    // cannot be retagged: the real iterator reads the real `table[]`, which
    // CratonVM's `HashMap.put` native never fills, so it would return a
    // silently EMPTY iteration rather than a loud error. That one stays a
    // refusal until the collections reclassification wave.)
    //
    // Under `--real-jdk` these are now dropped at registration — recorded as
    // `SyntheticNativeRegistered` violations naming this site — and the real
    // bytecode runs. `Compatible` / `--real-jdk` keep SyntheticStub
    // registrations, so both are unchanged.
    //
    // This is the "retag per subsystem, one PR each, with evidence" the
    // `register_collections_natives` header asks for, not the bulk flip it
    // forbids: four named registrars, each with a real-bytecode fallback that
    // was measured, not assumed.
        // ---- UnmodifiableCollection (also the shared base for List/Set) -------
    for c in [
        UNMOD_COLLECTION_CLASS,
        UNMOD_LIST_CLASS,
        UNMOD_SET_CLASS,
        UNMOD_SORTED_SET_CLASS,
        UNMOD_NAVIGABLE_SET_CLASS,
        UNMOD_ENTRY_SET_CLASS,
    ] {
        r.register(c, "size", "()I", native_unmod_size, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(c, "isEmpty", "()Z", native_unmod_is_empty, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(
            c,
            "contains",
            "(Ljava/lang/Object;)Z",
            native_unmod_contains, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "containsAll",
            "(Ljava/util/Collection;)Z",
            native_unmod_contains_all, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "iterator",
            "()Ljava/util/Iterator;",
            native_unmod_iterator, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(c, "toArray", "()[Ljava/lang/Object;", native_unmod_to_array, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(
            c,
            "toArray",
            "([Ljava/lang/Object;)[Ljava/lang/Object;",
            native_unmod_to_array_typed, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "toString",
            "()Ljava/lang/String;",
            native_unmod_to_string, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(c, "hashCode", "()I", native_unmod_hash_code, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(c, "equals", "(Ljava/lang/Object;)Z", native_unmod_equals, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(
            c,
            "forEach",
            "(Ljava/util/function/Consumer;)V",
            native_unmod_for_each, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "stream",
            "()Ljava/util/stream/Stream;",
            native_unmod_stream, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "spliterator",
            "()Ljava/util/Spliterator;",
            native_unmod_spliterator, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        // Mutators — all throw UnsupportedOperationException.
        r.register(c, "add", "(Ljava/lang/Object;)Z", native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(c, "remove", "(Ljava/lang/Object;)Z", native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(c, "clear", "()V", native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(c, "addAll", "(Ljava/util/Collection;)Z", native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(
            c,
            "removeAll",
            "(Ljava/util/Collection;)Z",
            native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "retainAll",
            "(Ljava/util/Collection;)Z",
            native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "removeIf",
            "(Ljava/util/function/Predicate;)Z",
            native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        // NavigableMap / SortedMap read-only views. `cratonvm/internal/
        // UnmodifiableMap` is synthetic (no real bytecode), so any nav method
        // not registered here raises NoSuchMethodError when a wrapped TreeMap /
        // NavigableMap is navigated — e.g. `unmodifiableNavigableMap(tm)
        // .higherEntry(k)`. Each simply forwards to the backing map (which has
        // the full nav surface registered above), preserving read-only-ness;
        // the mutating poll* variants stay unregistered/throwing.
        r.register(c, "firstKey", "()Ljava/lang/Object;", |ctx, args| {
            unmod_delegate(ctx, args, "firstKey", "()Ljava/lang/Object;")
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(c, "lastKey", "()Ljava/lang/Object;", |ctx, args| {
            unmod_delegate(ctx, args, "lastKey", "()Ljava/lang/Object;")
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        for m in ["ceilingKey", "floorKey", "higherKey", "lowerKey"] {
            let desc = "(Ljava/lang/Object;)Ljava/lang/Object;";
            let cb: cratonvm_native_api::NativeCallback = match m {
                "ceilingKey" => |ctx, args| {
                    unmod_delegate(
                        ctx,
                        args,
                        "ceilingKey",
                        "(Ljava/lang/Object;)Ljava/lang/Object;",
                    )
                },
                "floorKey" => |ctx, args| {
                    unmod_delegate(
                        ctx,
                        args,
                        "floorKey",
                        "(Ljava/lang/Object;)Ljava/lang/Object;",
                    )
                },
                "higherKey" => |ctx, args| {
                    unmod_delegate(
                        ctx,
                        args,
                        "higherKey",
                        "(Ljava/lang/Object;)Ljava/lang/Object;",
                    )
                },
                _ => |ctx, args| {
                    unmod_delegate(
                        ctx,
                        args,
                        "lowerKey",
                        "(Ljava/lang/Object;)Ljava/lang/Object;",
                    )
                },
            };
            r.register(c, m, desc, cb, cratonvm_native_api::NativeKind::SyntheticStub);
        }
        r.register(c, "firstEntry", "()Ljava/util/Map$Entry;", |ctx, args| {
            unmod_delegate(ctx, args, "firstEntry", "()Ljava/util/Map$Entry;")
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(c, "lastEntry", "()Ljava/util/Map$Entry;", |ctx, args| {
            unmod_delegate(ctx, args, "lastEntry", "()Ljava/util/Map$Entry;")
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        for m in ["ceilingEntry", "floorEntry", "higherEntry", "lowerEntry"] {
            let desc = "(Ljava/lang/Object;)Ljava/util/Map$Entry;";
            let cb: cratonvm_native_api::NativeCallback = match m {
                "ceilingEntry" => |ctx, args| {
                    unmod_delegate(
                        ctx,
                        args,
                        "ceilingEntry",
                        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
                    )
                },
                "floorEntry" => |ctx, args| {
                    unmod_delegate(
                        ctx,
                        args,
                        "floorEntry",
                        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
                    )
                },
                "higherEntry" => |ctx, args| {
                    unmod_delegate(
                        ctx,
                        args,
                        "higherEntry",
                        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
                    )
                },
                _ => |ctx, args| {
                    unmod_delegate(
                        ctx,
                        args,
                        "lowerEntry",
                        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
                    )
                },
            };
            r.register(c, m, desc, cb, cratonvm_native_api::NativeKind::SyntheticStub);
        }
    }

    // ---- UnmodifiableSortedSet / NavigableSet read-only surfaces ----------
    //
    // Plain `Collections.unmodifiableSet` must not satisfy `SortedSet`; AssertJ
    // checks that guard before calling `comparator()`. Sorted/navigable APIs use
    // distinct stamps so those casts keep working without broadening every
    // unmodifiable set view.
    {
        for c in [UNMOD_SORTED_SET_CLASS, UNMOD_NAVIGABLE_SET_CLASS] {
            r.register(c, "first", "()Ljava/lang/Object;", |ctx, args| {
                unmod_delegate(ctx, args, "first", "()Ljava/lang/Object;")
            }, cratonvm_native_api::NativeKind::SyntheticStub);
            r.register(c, "last", "()Ljava/lang/Object;", |ctx, args| {
                unmod_delegate(ctx, args, "last", "()Ljava/lang/Object;")
            }, cratonvm_native_api::NativeKind::SyntheticStub);
            r.register(c, "comparator", "()Ljava/util/Comparator;", |ctx, args| {
                unmod_delegate(ctx, args, "comparator", "()Ljava/util/Comparator;")
            }, cratonvm_native_api::NativeKind::SyntheticStub);
            r.register(
                c,
                "headSet",
                "(Ljava/lang/Object;)Ljava/util/SortedSet;",
                |ctx, args| {
                    unmod_delegate_rewrap_sorted_set(
                        ctx,
                        args,
                        "headSet",
                        "(Ljava/lang/Object;)Ljava/util/SortedSet;",
                    )
                }, cratonvm_native_api::NativeKind::SyntheticStub,
            );
            r.register(
                c,
                "tailSet",
                "(Ljava/lang/Object;)Ljava/util/SortedSet;",
                |ctx, args| {
                    unmod_delegate_rewrap_sorted_set(
                        ctx,
                        args,
                        "tailSet",
                        "(Ljava/lang/Object;)Ljava/util/SortedSet;",
                    )
                }, cratonvm_native_api::NativeKind::SyntheticStub,
            );
            r.register(
                c,
                "subSet",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/SortedSet;",
                |ctx, args| {
                    unmod_delegate_rewrap_sorted_set(
                        ctx,
                        args,
                        "subSet",
                        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/SortedSet;",
                    )
                }, cratonvm_native_api::NativeKind::SyntheticStub,
            );
        }

        let c = UNMOD_NAVIGABLE_SET_CLASS;
        for m in ["ceiling", "floor", "higher", "lower"] {
            let desc = "(Ljava/lang/Object;)Ljava/lang/Object;";
            let cb: cratonvm_native_api::NativeCallback = match m {
                "ceiling" => |ctx, args| {
                    unmod_delegate(
                        ctx,
                        args,
                        "ceiling",
                        "(Ljava/lang/Object;)Ljava/lang/Object;",
                    )
                },
                "floor" => |ctx, args| {
                    unmod_delegate(ctx, args, "floor", "(Ljava/lang/Object;)Ljava/lang/Object;")
                },
                "higher" => |ctx, args| {
                    unmod_delegate(
                        ctx,
                        args,
                        "higher",
                        "(Ljava/lang/Object;)Ljava/lang/Object;",
                    )
                },
                _ => |ctx, args| {
                    unmod_delegate(ctx, args, "lower", "(Ljava/lang/Object;)Ljava/lang/Object;")
                },
            };
            r.register(c, m, desc, cb, cratonvm_native_api::NativeKind::SyntheticStub);
        }
        r.register(
            c,
            "descendingIterator",
            "()Ljava/util/Iterator;",
            |ctx, args| unmod_delegate(ctx, args, "descendingIterator", "()Ljava/util/Iterator;"), cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(c, "pollFirst", "()Ljava/lang/Object;", native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(c, "pollLast", "()Ljava/lang/Object;", native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(
            c,
            "descendingSet",
            "()Ljava/util/NavigableSet;",
            |ctx, args| {
                unmod_delegate_rewrap_navigable_set(
                    ctx,
                    args,
                    "descendingSet",
                    "()Ljava/util/NavigableSet;",
                )
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "headSet",
            "(Ljava/lang/Object;Z)Ljava/util/NavigableSet;",
            |ctx, args| {
                unmod_delegate_rewrap_navigable_set(
                    ctx,
                    args,
                    "headSet",
                    "(Ljava/lang/Object;Z)Ljava/util/NavigableSet;",
                )
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "tailSet",
            "(Ljava/lang/Object;Z)Ljava/util/NavigableSet;",
            |ctx, args| {
                unmod_delegate_rewrap_navigable_set(
                    ctx,
                    args,
                    "tailSet",
                    "(Ljava/lang/Object;Z)Ljava/util/NavigableSet;",
                )
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "subSet",
            "(Ljava/lang/Object;ZLjava/lang/Object;Z)Ljava/util/NavigableSet;",
            |ctx, args| {
                unmod_delegate_rewrap_navigable_set(
                    ctx,
                    args,
                    "subSet",
                    "(Ljava/lang/Object;ZLjava/lang/Object;Z)Ljava/util/NavigableSet;",
                )
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
    }

    // ---- UnmodifiableList — adds positional reads + list mutators ---------
    {
        let c = UNMOD_LIST_CLASS;
        r.register(c, "get", "(I)Ljava/lang/Object;", native_unmod_get, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(c, "indexOf", "(Ljava/lang/Object;)I", native_unmod_index_of, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(
            c,
            "lastIndexOf",
            "(Ljava/lang/Object;)I",
            native_unmod_last_index_of, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(c, "subList", "(II)Ljava/util/List;", native_unmod_sub_list, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(
            c,
            "listIterator",
            "()Ljava/util/ListIterator;",
            native_unmod_list_iterator, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "listIterator",
            "(I)Ljava/util/ListIterator;",
            native_unmod_list_iterator_idx, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        // List mutators.
        r.register(
            c,
            "set",
            "(ILjava/lang/Object;)Ljava/lang/Object;",
            native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(c, "add", "(ILjava/lang/Object;)V", native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(c, "remove", "(I)Ljava/lang/Object;", native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(
            c,
            "addAll",
            "(ILjava/util/Collection;)Z",
            native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(c, "sort", "(Ljava/util/Comparator;)V", native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(
            c,
            "replaceAll",
            "(Ljava/util/function/UnaryOperator;)V",
            native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub,
        );
    }

    // ---- UnmodifiableMap --------------------------------------------------
    {
        let c = UNMOD_MAP_CLASS;
        r.register(c, "size", "()I", native_unmod_size, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(c, "isEmpty", "()Z", native_unmod_is_empty, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(
            c,
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            native_unmod_map_get, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "getOrDefault",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            native_unmod_map_get_or_default, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "containsKey",
            "(Ljava/lang/Object;)Z",
            native_unmod_map_contains_key, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "containsValue",
            "(Ljava/lang/Object;)Z",
            native_unmod_map_contains_value_checked, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(c, "keySet", "()Ljava/util/Set;", native_unmod_map_key_set, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(
            c,
            "values",
            "()Ljava/util/Collection;",
            native_unmod_map_values, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "entrySet",
            "()Ljava/util/Set;",
            native_unmod_map_entry_set, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "toString",
            "()Ljava/lang/String;",
            native_unmod_to_string, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(c, "hashCode", "()I", native_unmod_hash_code, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(c, "equals", "(Ljava/lang/Object;)Z", native_unmod_equals, cratonvm_native_api::NativeKind::SyntheticStub);
        // SequencedMap.reversed() (Java 21) — an unmodifiable reverse-ordered
        // view over the backing's entries. The synthetic wrapper does not
        // implement SequencedMap, so an unintercepted call raised
        // NoSuchMethodError (blocked ES `Version`/Log4j config walking). Build a
        // reverse snapshot of the backing and re-wrap it read-only.
        r.register(c, "reversed", "()Ljava/util/SequencedMap;", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let backing = match unmod_backing(ctx, this) {
                Some(b) => b,
                None => return Ok(Some(Value::Object(None))),
            };
            let rev = build_reversed_map_snapshot(ctx, backing)?;
            Ok(Some(Value::Object(Some(alloc_unmod_wrapper(
                ctx,
                UNMOD_MAP_CLASS,
                rev,
            )?))))
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(
            c,
            "forEach",
            "(Ljava/util/function/BiConsumer;)V",
            native_unmod_map_for_each, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        // Map mutators.
        r.register(
            c,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "remove",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(c, "clear", "()V", native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(c, "putAll", "(Ljava/util/Map;)V", native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(
            c,
            "putIfAbsent",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "replace",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "computeIfAbsent",
            "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;",
            native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "compute",
            "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
            native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "computeIfPresent",
            "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
            native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "merge",
            "(Ljava/lang/Object;Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
            native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "replaceAll",
            "(Ljava/util/function/BiFunction;)V",
            native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub,
        );

        // ---- NavigableMap / SortedMap read surface ------------------------
        // `Collections.unmodifiableNavigableMap` / `unmodifiableSortedMap`
        // reuse this same wrapper class, but only the plain Map methods were
        // registered. Any NavigableMap/SortedMap call (e.g. `floorEntry`,
        // dispatched from a `NavigableMap`-typed reference and retargeted to
        // the concrete wrapper) hit NoSuchMethodError — a dominant Elasticsearch
        // suite-setup blocker. Delegate every read to the backing (a real
        // NavigableMap such as TreeMap); re-wrap map-view returns as read-only;
        // the `poll*` views are mutators → throw. (`NativeCallback` is a bare
        // fn pointer, so each closure must inline its own name/descriptor — no
        // captures — matching the ASL `toArray` delegation pattern above.)
        r.register(c, "comparator", "()Ljava/util/Comparator;", |ctx, args| {
            unmod_delegate(ctx, args, "comparator", "()Ljava/util/Comparator;")
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(c, "firstKey", "()Ljava/lang/Object;", |ctx, args| {
            unmod_delegate(ctx, args, "firstKey", "()Ljava/lang/Object;")
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(c, "lastKey", "()Ljava/lang/Object;", |ctx, args| {
            unmod_delegate(ctx, args, "lastKey", "()Ljava/lang/Object;")
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(c, "firstEntry", "()Ljava/util/Map$Entry;", |ctx, args| {
            unmod_delegate(ctx, args, "firstEntry", "()Ljava/util/Map$Entry;")
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(c, "lastEntry", "()Ljava/util/Map$Entry;", |ctx, args| {
            unmod_delegate(ctx, args, "lastEntry", "()Ljava/util/Map$Entry;")
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(
            c,
            "floorEntry",
            "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
            |ctx, args| {
                unmod_delegate(
                    ctx,
                    args,
                    "floorEntry",
                    "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
                )
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "ceilingEntry",
            "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
            |ctx, args| {
                unmod_delegate(
                    ctx,
                    args,
                    "ceilingEntry",
                    "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
                )
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "higherEntry",
            "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
            |ctx, args| {
                unmod_delegate(
                    ctx,
                    args,
                    "higherEntry",
                    "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
                )
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "lowerEntry",
            "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
            |ctx, args| {
                unmod_delegate(
                    ctx,
                    args,
                    "lowerEntry",
                    "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
                )
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "floorKey",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            |ctx, args| {
                unmod_delegate(
                    ctx,
                    args,
                    "floorKey",
                    "(Ljava/lang/Object;)Ljava/lang/Object;",
                )
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "ceilingKey",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            |ctx, args| {
                unmod_delegate(
                    ctx,
                    args,
                    "ceilingKey",
                    "(Ljava/lang/Object;)Ljava/lang/Object;",
                )
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "higherKey",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            |ctx, args| {
                unmod_delegate(
                    ctx,
                    args,
                    "higherKey",
                    "(Ljava/lang/Object;)Ljava/lang/Object;",
                )
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "lowerKey",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            |ctx, args| {
                unmod_delegate(
                    ctx,
                    args,
                    "lowerKey",
                    "(Ljava/lang/Object;)Ljava/lang/Object;",
                )
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "navigableKeySet",
            "()Ljava/util/NavigableSet;",
            |ctx, args| unmod_delegate(ctx, args, "navigableKeySet", "()Ljava/util/NavigableSet;"), cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "descendingKeySet",
            "()Ljava/util/NavigableSet;",
            |ctx, args| unmod_delegate(ctx, args, "descendingKeySet", "()Ljava/util/NavigableSet;"), cratonvm_native_api::NativeKind::SyntheticStub,
        );
        // Map-returning views — re-wrap read-only.
        r.register(
            c,
            "descendingMap",
            "()Ljava/util/NavigableMap;",
            |ctx, args| {
                unmod_delegate_rewrap_map(ctx, args, "descendingMap", "()Ljava/util/NavigableMap;")
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "headMap",
            "(Ljava/lang/Object;Z)Ljava/util/NavigableMap;",
            |ctx, args| {
                unmod_delegate_rewrap_map(
                    ctx,
                    args,
                    "headMap",
                    "(Ljava/lang/Object;Z)Ljava/util/NavigableMap;",
                )
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "tailMap",
            "(Ljava/lang/Object;Z)Ljava/util/NavigableMap;",
            |ctx, args| {
                unmod_delegate_rewrap_map(
                    ctx,
                    args,
                    "tailMap",
                    "(Ljava/lang/Object;Z)Ljava/util/NavigableMap;",
                )
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "subMap",
            "(Ljava/lang/Object;ZLjava/lang/Object;Z)Ljava/util/NavigableMap;",
            |ctx, args| {
                unmod_delegate_rewrap_map(
                    ctx,
                    args,
                    "subMap",
                    "(Ljava/lang/Object;ZLjava/lang/Object;Z)Ljava/util/NavigableMap;",
                )
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "headMap",
            "(Ljava/lang/Object;)Ljava/util/SortedMap;",
            |ctx, args| {
                unmod_delegate_rewrap_map(
                    ctx,
                    args,
                    "headMap",
                    "(Ljava/lang/Object;)Ljava/util/SortedMap;",
                )
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "tailMap",
            "(Ljava/lang/Object;)Ljava/util/SortedMap;",
            |ctx, args| {
                unmod_delegate_rewrap_map(
                    ctx,
                    args,
                    "tailMap",
                    "(Ljava/lang/Object;)Ljava/util/SortedMap;",
                )
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "subMap",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/SortedMap;",
            |ctx, args| {
                unmod_delegate_rewrap_map(
                    ctx,
                    args,
                    "subMap",
                    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/SortedMap;",
                )
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        // Mutating navigable views.
        r.register(
            c,
            "pollFirstEntry",
            "()Ljava/util/Map$Entry;",
            native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "pollLastEntry",
            "()Ljava/util/Map$Entry;",
            native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub,
        );
    }

    // ---- UnmodifiableItr — read-only iterator -----------------------------
    {
        let c = UNMOD_ITR_CLASS;
        r.register(c, "hasNext", "()Z", native_unmod_itr_has_next, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(c, "next", "()Ljava/lang/Object;", native_unmod_itr_next, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(c, "remove", "()V", native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub);
    }

    // ---- UnmodifiableEntrySet — entrySet() of an unmodifiable/immutable
    // map. Overrides the plain-Set `iterator`/`forEach` registered by the
    // shared loop above so each yielded `Map.Entry` is wrapped in
    // `UnmodifiableMapEntry` (setValue() must throw, not silently mutate the
    // backing map through the "locked" view — see
    // parametermap-immutability-not-locked-FIXED.md).
    {
        let c = UNMOD_ENTRY_SET_CLASS;
        r.register(
            c,
            "iterator",
            "()Ljava/util/Iterator;",
            native_unmod_entry_set_iterator, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "forEach",
            "(Ljava/util/function/Consumer;)V",
            native_unmod_entry_set_for_each, cratonvm_native_api::NativeKind::SyntheticStub,
        );
    }

    // ---- UnmodifiableEntryItr — iterator over an UnmodifiableEntrySet -----
    {
        let c = UNMOD_ENTRY_ITR_CLASS;
        r.register(c, "hasNext", "()Z", native_unmod_itr_has_next, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(
            c,
            "next",
            "()Ljava/lang/Object;",
            native_unmod_entry_itr_next, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(c, "remove", "()V", native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub);
    }

    // ---- UnmodifiableMapEntry — read-only Map.Entry view -------------------
    // Wraps a real (mutable) Map.Entry so setValue() throws
    // UnsupportedOperationException instead of writing through to the
    // backing map, matching `Collections$UnmodifiableMap$UnmodifiableEntrySet
    // $UnmodifiableEntry`. Only entries obtained via UnmodifiableEntrySet's
    // iterator/forEach are wrapped — entries from other paths (e.g. a direct
    // `Map.get`) are the real mutable objects.
    {
        let c = UNMOD_MAP_ENTRY_CLASS;
        r.register(c, "getKey", "()Ljava/lang/Object;", |ctx, args| {
            unmod_delegate(ctx, args, "getKey", "()Ljava/lang/Object;")
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(c, "getValue", "()Ljava/lang/Object;", |ctx, args| {
            unmod_delegate(ctx, args, "getValue", "()Ljava/lang/Object;")
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(
            c,
            "setValue",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "toString",
            "()Ljava/lang/String;",
            native_unmod_to_string, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(c, "hashCode", "()I", native_unmod_hash_code, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(c, "equals", "(Ljava/lang/Object;)Z", native_unmod_equals, cratonvm_native_api::NativeKind::SyntheticStub);
    }

    // ---- UnmodifiableListItr — read-only ListIterator ---------------------
    // A self-contained `ListIterator` over a snapshot of the backing list:
    //   field 0 = Object[] snapshot of the list elements
    //   field 1 = Int cursor (the `nextIndex`)
    // Read operations walk the snapshot; the mutators (`set`/`add`/`remove`)
    // throw `UnsupportedOperationException`, consistent with an unmodifiable
    // list and the JDK's `Collections$UnmodifiableList$1` list-iterator view.
    {
        let c = UNMOD_LIST_ITR_CLASS;
        r.register(c, "hasNext", "()Z", native_unmod_listitr_has_next, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(c, "next", "()Ljava/lang/Object;", native_unmod_listitr_next, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(c, "hasPrevious", "()Z", native_unmod_listitr_has_previous, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(
            c,
            "previous",
            "()Ljava/lang/Object;",
            native_unmod_listitr_previous, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(c, "nextIndex", "()I", native_unmod_listitr_next_index, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(
            c,
            "previousIndex",
            "()I",
            native_unmod_listitr_previous_index, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            c,
            "forEachRemaining",
            "(Ljava/util/function/Consumer;)V",
            native_unmod_listitr_for_each_remaining, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        // Mutators — all throw UnsupportedOperationException.
        r.register(c, "set", "(Ljava/lang/Object;)V", native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(c, "add", "(Ljava/lang/Object;)V", native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(c, "remove", "()V", native_unmod_throw, cratonvm_native_api::NativeKind::SyntheticStub);
    }
    }

/// Universal mutator: throws `UnsupportedOperationException`.
pub(crate) fn native_unmod_throw(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Err(unsupported_op())
}

pub(crate) fn native_unmod_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "size", "()I")
}

pub(crate) fn native_unmod_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "isEmpty", "()Z")
}

pub(crate) fn native_unmod_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    reject_null_immutable_query(&*ctx, args, 1)?;
    unmod_delegate(ctx, args, "contains", "(Ljava/lang/Object;)Z")
}

pub(crate) fn native_unmod_contains_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "containsAll", "(Ljava/util/Collection;)Z")
}

pub(crate) fn native_unmod_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Bounds-check BEFORE delegating, and keep the
    // `ArrayIndexOutOfBoundsException` this path has always thrown.
    //
    // The delegate is a real `java/util/ArrayList`, so it now answers with the
    // plain `IndexOutOfBoundsException` that HotSpot's `ArrayList` throws --
    // right for that receiver, wrong for this one. Measured against HotSpot 25
    // with `probes/ListOutOfBoundsProbe`: `List.of()` and `List.of(a,b,c,d)`
    // throw `ArrayIndexOutOfBoundsException` (`ImmutableCollections.ListN`
    // indexes its array directly), while the 1-2 element `List12` throws the
    // plain class with a different wording again ("Index: 3 Size: 1").
    //
    // The discriminator is slot 1 (`UNMOD_FIELD_IMMUTABLE`), not the class:
    // `unmod_list_oob_error` separates a 1-2 element `List.of` from
    // `Collections.unmodifiableList` over a 1-2 element `ArrayList` with the
    // same bit `getClass()` already splits on. See that function.
    if let (Some(Value::Object(Some(this))), Some(Value::Int(index))) = (args.first(), args.get(1))
    {
        if let Some(backing) = unmod_receiver_backing(ctx, *this) {
            // The SIZE must not come from `al_state` alone -- it reads an
            // `elementData`-shaped backing and reports 0 for every other List
            // shape, so this pre-check rejected PERFECTLY VALID indices on a
            // view over a LinkedList or an Arrays$ArrayList, while the same
            // list iterated fine (iteration does not come through here):
            //
            //   Collections.unmodifiableList(new LinkedList<>(List.of("x","y")))
            //       .get(0)  -> AIOOBE    (HotSpot: "x")
            //
            // and it is `unmod_delegate` below, not this check, that produces
            // the right answer once the index is allowed through. See
            // `unmod_view_size`. This is the registration that actually serves
            // `Collections.unmodifiableList(..).get(i)` --
            // `--dump-native-registry` names it
            // `cratonvm/internal/UnmodifiableList.get`.
            let (data, al_size) = al_state(ctx, backing);
            if let Some(n) = unmod_view_size(data, al_size) {
                if *index < 0 || *index >= n {
                    if let Some(err) = unmod_list_oob_error(&*ctx, *this, *index, n) {
                        return Err(err);
                    }
                    // `None` = a `Collections.unmodifiable*` view; `unmod_delegate`
                    // below lets the backing throw its own, which is what HotSpot
                    // does for that receiver.
                }
            }
            // `None` = `al_state` could not read this backing. STAND ASIDE and
            // let `unmod_delegate` below bounds-check: the backing's own `get`
            // does it correctly and throws the JDK's own exception for its
            // shape. Asking the backing for its `size()` here instead would
            // cost a full VM dispatch on EVERY get -- two per element for the
            // `for (i..n) list.get(i)` loop that is the common shape -- for a
            // check the delegate is about to do anyway.
        }
    }
    unmod_delegate(ctx, args, "get", "(I)Ljava/lang/Object;")
}

pub(crate) fn native_unmod_index_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    reject_null_immutable_query(&*ctx, args, 1)?;
    unmod_delegate(ctx, args, "indexOf", "(Ljava/lang/Object;)I")
}

pub(crate) fn native_unmod_last_index_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    reject_null_immutable_query(&*ctx, args, 1)?;
    unmod_delegate(ctx, args, "lastIndexOf", "(Ljava/lang/Object;)I")
}

pub(crate) fn native_unmod_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "toArray", "()[Ljava/lang/Object;")
}

pub(crate) fn native_unmod_to_array_typed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(
        ctx,
        args,
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
    )
}

pub(crate) fn native_unmod_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "toString", "()Ljava/lang/String;")
}

pub(crate) fn native_unmod_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // HIB-CV-30: for a List-shaped immutable wrapper (e.g. `List.of(..)` →
    // `cratonvm/internal/UnmodifiableList` over a synthetic ArrayList backing),
    // compute the `List.hashCode()` contract directly over the backing's
    // element array instead of delegating to `invoke_virtual(backing,
    // "hashCode")`. That delegation re-enters real `AbstractList.hashCode`
    // bytecode, which — when it hashes a GC-relocated element whose class does
    // not override `hashCode` — reads an unstable identity hash for that
    // element (0, then 1, ... per GC cycle) rather than the header value. The
    // canonical victim is JUnit 6's `Namespace.hashCode()` over
    // `List.of(extensionClassName, testInstance)`: the drift silently breaks
    // `NamespacedHierarchicalStore` lookups, which (via `@TestInstance(PER_CLASS)`)
    // makes Hibernate rebuild the EntityManagerFactory between test methods —
    // surfacing as "a cascaded association is null". `native_al_hash_code`
    // reads each element fresh from the backing array and hashes it immediately,
    // so it does not hold a stale element ref across an allocation/GC.
    if let Some(Value::Object(Some(this))) = args.first() {
        if let Some(backing) = unmod_backing(ctx, *this) {
            // Only redirect for ordered list backings; Set/Map wrappers use the
            // unordered contract and must keep delegating.
            let bcid = ctx.class_id_of_object(backing);
            let is_list = ctx
                .class_name_of_id(bcid)
                .map(|n| n == "java/util/ArrayList" || n == "java/util/LinkedList")
                .unwrap_or(false);
            if is_list {
                return native_al_hash_code(ctx, &[Value::Object(Some(backing))]);
            }
        }
    }
    unmod_delegate(ctx, args, "hashCode", "()I")
}

pub(crate) fn native_unmod_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `this.equals(other)` forwards to `backing.equals(other)`. If `other` is
    // itself an unmodifiable wrapper, it must be unwrapped to its backing
    // collection first — otherwise the backing list's `AbstractList.equals`
    // tries to iterate the *wrapper* (e.g. via `listIterator()`, which the
    // wrapper does not expose) and the comparison spuriously fails. This is
    // the JDK behaviour: `List.of(a,b).equals(List.of(a,b))` is `true`, and
    // crucially `ResourceBundle$Control.FORMAT_DEFAULT.equals(itself)` must
    // be `true` or `getNoFallbackControl` throws IllegalArgumentException.
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let backing = match unmod_backing(ctx, this) {
        Some(b) => b,
        None => return Ok(Some(Value::Object(None))),
    };
    // Unwrap the argument: walk down any chain of unmodifiable wrappers so
    // the backing's `equals` sees a concrete ArrayList / HashSet / HashMap.
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => {
            let mut cur = *o;
            while is_unmod_wrapper(ctx, cur) {
                match unmod_backing(ctx, cur) {
                    Some(b) => cur = b,
                    None => break,
                }
            }
            Value::Object(Some(cur))
        }
        v => v.cloned().unwrap_or(Value::Object(None)),
    };
    ctx.invoke_virtual(backing, "equals", "(Ljava/lang/Object;)Z", &[other])
}

pub(crate) fn native_unmod_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "forEach", "(Ljava/util/function/Consumer;)V")
}

pub(crate) fn native_unmod_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "stream", "()Ljava/util/stream/Stream;")
}

/// `Spliterator.characteristics()` for an IMMUTABLE collection, measured across
/// every shape the factories produce.
///
/// The backing's own answer is wrong for these because the JDK does not ask the
/// backing: `ImmutableCollections` builds its spliterator with its own flags,
/// and the size-1 shapes take `Collections.singletonSpliterator` instead.
/// MEASURED against HotSpot 25.0.4+7 (`apps/probes/UtilCoverageSweep`):
///
/// ```text
///   Set.of("a")            17745      List.of("a")           17745
///   Set.of()               16449      List.of()              16464
///   Set.of("a","b")        16449      List.of x3             16464
///   Set.of x3 (SetN)       16449      Map.of().entrySet()    17745  (size 1)
///   Map.of().keySet()      16449      Map.of().values()      16448
/// ```
///
/// **The discriminator is SIZE, not class.** Every 17745 is a size-1 immutable
/// -- `Set.of("a")`, `List.of("a")`, a one-entry `entrySet`, and
/// `Collections.singleton`, which this VM already answered correctly and is the
/// control that names the rule. The JDK routes those to
/// `Collections.singletonSpliterator` (the `Collections$2` a strict-mode run
/// reports); everything else goes through
/// `Spliterators.spliterator(collection, flags)`, which contributes
/// `SIZED | SUBSIZED` on top of the family's own bits.
///
/// COMPATIBLE MODE ONLY. Strict refuses `cratonvm/internal/Unmodifiable*` and
/// runs java.base's own bodies, which is why it was already 0-diff on all of
/// these -- the tenth row in this campaign where `--real-jdk` is the more
/// correct mode.
pub(crate) const SPL_IMMUTABLE_SINGLETON: i32 =
    SPL_SIZED | SPL_DISTINCT | SPL_ORDERED | SPL_NONNULL | 0x0400 | SPL_SUBSIZED;

/// The characteristics HotSpot's immutable factories answer, by shape.
///
/// MEASURED across the whole matrix (`apps/probes/ImmutableSplProbe`, HotSpot
/// 25.0.4+7) rather than derived from the cells that failed, because the
/// obvious rule -- "a size-1 immutable is a singleton" -- is contradicted by
/// the very same map:
///
/// ```text
///   Set.of("a")              17745  ImmutableCollections$Set12 / Collections$2
///   List.of("a")             17745  ImmutableCollections$List12 / Collections$2
///   Map.of("a",1).entrySet() 17745  ImmutableCollections$Set12 / Collections$2
///   Map.of("a",1).keySet()   16449  AbstractMap$1 / Spliterators$IteratorSpliterator
///   Map.of("a",1).values()   16448  AbstractMap$2 / Spliterators$IteratorSpliterator
/// ```
///
/// The CLASS column is the rule. Every 17745 is `Collections$2`, which is
/// `Collections.singletonSpliterator`, and `List12`/`Set12` route to it when
/// their second slot is the empty sentinel -- so "size 1" is right, but only
/// for a collection the factory built as a `List12`/`Set12`. A `Map.of`'s
/// keySet and values are not: they are the anonymous `AbstractMap$1`/`$2`
/// views, which carry no immutable bits at all, at any size. `entrySet` is the
/// odd one because `Map1.entrySet()` is literally `Set.of(entry)`, and a
/// two-entry map's `MapN$1` entrySet drops back to 16449.
///
/// `Collections.unmodifiableSet(hashSet)` is the control that must NOT move:
/// the JDK's WRAPPER really does hand back the backing's spliterator, so that
/// arm never reaches here.
pub(crate) fn immutable_spliterator_characteristics(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    size: i32,
    is_map_view: bool,
) -> i32 {
    let cls = ctx.class_name_arc_of_id(ctx.class_id_of_object(this));
    if is_map_view {
        // A view of an immutable map is a plain `AbstractMap` view, with one
        // exception the class name already tells apart.
        return match cls.as_deref() {
            Some(UNMOD_ENTRY_SET_CLASS) if size == 1 => SPL_IMMUTABLE_SINGLETON,
            Some(UNMOD_COLLECTION_CLASS) => SPL_SIZED | SPL_SUBSIZED,
            _ => SPL_SIZED | SPL_DISTINCT | SPL_SUBSIZED,
        };
    }
    // Size outranks the carrier here, and only here: it is the whole reason
    // `Set.of("a")` and `List.of("a")` agree at 17745 while their two- and
    // three-element siblings do not.
    if size == 1 {
        return SPL_IMMUTABLE_SINGLETON;
    }
    match cls.as_deref() {
        // A list keeps its encounter order and is not distinct.
        Some(UNMOD_LIST_CLASS) => SPL_SIZED | SPL_ORDERED | SPL_SUBSIZED,
        // A values view is neither distinct nor ordered.
        Some(UNMOD_COLLECTION_CLASS) => SPL_SIZED | SPL_SUBSIZED,
        // Sets, keySets and entrySets: distinct, no encounter order.
        _ => SPL_SIZED | SPL_DISTINCT | SPL_SUBSIZED,
    }
}

pub(crate) fn native_unmod_spliterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // An IMMUTABLE receiver -- or a view of an immutable map -- answers its own
    // family's bits; an unmodifiable WRAPPER delegates, because the JDK's
    // wrapper really does hand back the backing's spliterator
    // (`unmodifiableSet(hashSet)` is 65 on both VMs, and that row is the
    // control for leaving this arm alone).
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return unmod_delegate(ctx, args, "spliterator", "()Ljava/util/Spliterator;");
    };
    let is_map_view = unmod_is_immutable_map_view(&*ctx, this);
    if !unmod_is_immutable(&*ctx, this) && !is_map_view {
        return unmod_delegate(ctx, args, "spliterator", "()Ljava/util/Spliterator;");
    }

    let spl = unmod_delegate(ctx, args, "spliterator", "()Ljava/util/Spliterator;")?;

    // ONLY a spliterator THIS CRATE MINTED may be written to. The backing's
    // `spliterator()` is not guaranteed to reach `native_al_spliterator`: for a
    // `List.of` the delegate lands on java.base's own `ArrayList.spliterator()`
    // bytecode and hands back a REAL `ArrayList$ArrayListSpliterator`, whose
    // slot 3 is its `this$0`. Writing a characteristics mask there clobbered
    // it, and the next `estimateSize()` died in `getFence` with "Cannot read
    // field modCount because this.this$0 is null" -- which is how this was
    // found, one probe row after the change that caused it.
    //
    // `two-producers-of-one-carrier-class` again, in its most direct form: the
    // synthetic `java/util/Spliterator` and the JDK's own classes both arrive
    // here, and only one of them has a slot 3 that means what this code thinks
    // it means.
    let spl_is_ours = matches!(spl, Some(Value::Object(Some(o)))
        if ctx.class_name_arc_of_id(ctx.class_id_of_object(o)).as_deref()
            == Some("java/util/Spliterator"));

    if let (true, Some(Value::Object(Some(spl_obj)))) = (spl_is_ours, spl) {
        let size = match ctx.get_field(spl_obj, 2) {
            Value::Int(n) => n,
            _ => -1,
        };
        let chars = immutable_spliterator_characteristics(&*ctx, this, size, is_map_view);
        let hint = immutable_spliterator_display_hint(&*ctx, this, size, is_map_view);
        if ctx.object_num_fields(spl_obj) > SPL_FIELD_DISPLAY_HINT {
            ctx.set_field(spl_obj, SPL_FIELD_CHARACTERISTICS, Value::Int(chars));
            ctx.set_field(spl_obj, SPL_FIELD_DISPLAY_HINT, Value::Int(hint));
            return Ok(spl);
        }
        // The backing handed back a narrower shape (three fields, no
        // characteristics slot; or four, no display-hint slot). Widen it:
        // copy the array, cursor and length across and add both slots.
        let arr = ctx.get_field(spl_obj, 0);
        let cursor = ctx.get_field(spl_obj, 1);
        let wide = try_alloc_synthetic(ctx, "java/util/Spliterator", 5)?;
        ctx.set_field(wide, 0, arr);
        ctx.set_field(wide, 1, cursor);
        ctx.set_field(wide, 2, Value::Int(size));
        ctx.set_field(wide, SPL_FIELD_CHARACTERISTICS, Value::Int(chars));
        ctx.set_field(wide, SPL_FIELD_DISPLAY_HINT, Value::Int(hint));
        return Ok(Some(Value::Object(Some(wide))));
    }

    // The delegate handed back one of the JDK's OWN spliterators, which we may
    // not write to and whose mask is the BACKING's rather than the immutable
    // family's. `List.of("a")` is that case: java.base answers 16464 for the
    // `ArrayList` behind it where HotSpot answers 17745 for a `List12`. Mint
    // ours over the same elements instead. The rule was already right and
    // simply had nowhere to be written, which is what left this one cell open
    // after the set shapes were fixed.
    let arr = unmod_delegate(ctx, args, "toArray", "()[Ljava/lang/Object;")?;
    if let Some(Value::Object(Some(a))) = arr {
        let len = ctx.array_length(a) as i32;
        let chars = immutable_spliterator_characteristics(&*ctx, this, len, is_map_view);
        let hint = immutable_spliterator_display_hint(&*ctx, this, len, is_map_view);
        let pin = ctx.pin_native_root(a);
        // Not `?`: the pin is this frame's base and must be released before
        // unwinding, or it and everything pinned above it are stranded.
        let minted = match try_alloc_synthetic(ctx, "java/util/Spliterator", 5) {
            Ok(m) => m,
            Err(err) => {
                ctx.unpin_native_roots(pin);
                return Err(err);
            }
        };
        let a = ctx.read_native_pin(pin, a);
        ctx.set_field(minted, 0, Value::Object(Some(a)));
        ctx.set_field(minted, 1, Value::Int(0));
        ctx.set_field(minted, 2, Value::Int(len));
        ctx.set_field(minted, SPL_FIELD_CHARACTERISTICS, Value::Int(chars));
        ctx.set_field(minted, SPL_FIELD_DISPLAY_HINT, Value::Int(hint));
        ctx.unpin_native_roots(pin);
        return Ok(Some(Value::Object(Some(minted))));
    }
    Ok(spl)
}

/// `subList` returns another unmodifiable view over the backing sub-list.
pub(crate) fn native_unmod_sub_list(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let sub = unmod_delegate(ctx, args, "subList", "(II)Ljava/util/List;")?;
    if let Some(Value::Object(Some(inner))) = sub {
        let w = alloc_unmod_wrapper(ctx, UNMOD_LIST_CLASS, inner)?;
        return Ok(Some(Value::Object(Some(w))));
    }
    Ok(sub)
}

/// `CRATONVM_COMPAT_UNMOD_ITERATOR_JDK` -- default ON, `=0` restores the
/// `toArray()` snapshot. JIT round 16 (lane compatdrain),
/// `r15w3-compat3-collection-review-findings-CLOSED-20261001.md` item 6:
/// `Collections.unmodifiableList(l).iterator()` delegates to `l.iterator()`
/// live, as JDK 25's inherited `UnmodifiableCollection.iterator()` does. The
/// same switch makes `native-builtins`' `getClass()` name the
/// `UnmodifiableItr` carrier `Collections$UnmodifiableCollection$1`.
pub(crate) fn unmod_iterator_jdk_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_UNMOD_ITERATOR_JDK")
}

/// `iterator()` returns a read-only iterator.
pub(crate) fn native_unmod_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let is_list_wrapper = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .map(|n| n == UNMOD_LIST_CLASS)
        .unwrap_or(false);
    // JIT round 16 (lane compatdrain), r15w3-compat3 item 6: JDK 25's
    // `UnmodifiableList` inherits `UnmodifiableCollection.iterator()`, a LIVE
    // wrapper over the backing's own `c.iterator()` (`Collections$
    // UnmodifiableCollection$1`; HotSpot sees a change made during the walk,
    // or throws the backing's CME). Only the immutable factories' lists keep
    // the snapshot: their backing never changes.
    let live_list_iterator =
        is_list_wrapper && unmod_iterator_jdk_enabled() && !unmod_is_immutable(&*ctx, this);
    if is_list_wrapper && !live_list_iterator {
        let snapshot = match unmod_list_snapshot(ctx, args) {
            Some(a) => a,
            None => return Ok(Some(Value::Object(None))),
        };
        let immutable = matches!(args.first(), Some(Value::Object(Some(w)))
            if unmod_is_immutable(&*ctx, *w));
        return Ok(Some(Value::Object(Some(alloc_unmod_list_itr(
            ctx, snapshot, 0, immutable, false,
        )?))));
    }

    let inner = unmod_delegate(ctx, args, "iterator", "()Ljava/util/Iterator;")?;
    if let Some(Value::Object(Some(itr))) = inner {
        let w = alloc_unmod_wrapper(ctx, UNMOD_ITR_CLASS, itr)?;
        return Ok(Some(Value::Object(Some(w))));
    }
    Ok(inner)
}

pub(crate) fn native_unmod_itr_has_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "hasNext", "()Z")
}

pub(crate) fn native_unmod_itr_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "next", "()Ljava/lang/Object;")
}

/// `UnmodifiableEntrySet.iterator()` — like `native_unmod_iterator`, but
/// wraps the backing iterator in `UnmodifiableEntryItr` so each yielded
/// `Map.Entry` is itself wrapped (see `native_unmod_entry_itr_next`).
pub(crate) fn native_unmod_entry_set_iterator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let inner = unmod_delegate(ctx, args, "iterator", "()Ljava/util/Iterator;")?;
    if let Some(Value::Object(Some(itr))) = inner {
        let w = alloc_unmod_wrapper(ctx, UNMOD_ENTRY_ITR_CLASS, itr)?;
        return Ok(Some(Value::Object(Some(w))));
    }
    Ok(inner)
}

/// `UnmodifiableEntryItr.next()` — wraps the backing iterator's `Map.Entry`
/// in `UnmodifiableMapEntry` so a caller's `setValue()` throws instead of
/// writing through to the backing map (the `Map.replaceAll` default method
/// iterates `entrySet()` and calls `entry.setValue(...)` directly, so an
/// unwrapped entry here silently defeats `ParameterMap`'s lock).
pub(crate) fn native_unmod_entry_itr_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let next = unmod_delegate(ctx, args, "next", "()Ljava/lang/Object;")?;
    if let Some(Value::Object(Some(entry))) = next {
        let w = alloc_unmod_wrapper(ctx, UNMOD_MAP_ENTRY_CLASS, entry)?;
        return Ok(Some(Value::Object(Some(w))));
    }
    Ok(next)
}

/// `UnmodifiableEntrySet.forEach(action)` — wraps each entry the same way
/// the iterator does before invoking the consumer, so `action.accept(entry)`
/// calling `entry.setValue(...)` still throws instead of mutating the
/// backing map.
pub(crate) fn native_unmod_entry_set_for_each(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let (this, consumer) = match (args.first(), args.get(1)) {
        (Some(Value::Object(Some(t))), Some(Value::Object(Some(c)))) => (*t, *c),
        _ => return Ok(None),
    };
    let backing = match unmod_backing(ctx, this) {
        Some(b) => b,
        None => return Ok(None),
    };
    // gc-common w20-a: `consumer` is a bare copy of the argument and every
    // turn below runs Java and allocates; it used to be handed to `accept`
    // through its entry address. Pinned first, so it is the frame's base.
    let consumer_pin = ctx.pin_native_root(consumer);
    let itr = match ctx.invoke_virtual(backing, "iterator", "()Ljava/util/Iterator;", &[]) {
        Ok(Some(Value::Object(Some(i)))) => i,
        Ok(_) => {
            ctx.unpin_native_roots(consumer_pin);
            return Ok(None);
        }
        Err(e) => {
            ctx.unpin_native_roots(consumer_pin);
            return Err(e);
        }
    };
    let itr_pin = ctx.pin_native_root(itr);
    let outcome = (|| -> Result<(), MethodCallFailed> {
        loop {
            let itr_now = ctx.read_native_pin(itr_pin, itr);
            let has_next = matches!(
                ctx.invoke_virtual(itr_now, "hasNext", "()Z", &[])?,
                Some(Value::Int(n)) if n != 0
            );
            if !has_next {
                return Ok(());
            }
            // `hasNext` ran Java: re-read the iterator before `next`.
            let itr_now = ctx.read_native_pin(itr_pin, itr);
            let entry = match ctx.invoke_virtual(itr_now, "next", "()Ljava/lang/Object;", &[])? {
                Some(Value::Object(Some(e))) => e,
                _ => return Ok(()),
            };
            let wrapped = alloc_unmod_wrapper(ctx, UNMOD_MAP_ENTRY_CLASS, entry)?;
            let consumer_now = ctx.read_native_pin(consumer_pin, consumer);
            ctx.invoke_virtual(
                consumer_now,
                "accept",
                "(Ljava/lang/Object;)V",
                &[Value::Object(Some(wrapped))],
            )?;
        }
    })();
    ctx.unpin_native_roots(consumer_pin);
    outcome.map(|()| None)
}

/// Slot layout of a `UnmodifiableListItr`.
pub(crate) const UNMOD_LIST_ITR_SNAPSHOT: usize = 0;
pub(crate) const UNMOD_LIST_ITR_CURSOR: usize = 1;
/// Whether the wrapper this iterator walks came from an immutable factory
/// (`List.of`/`copyOf`) rather than `Collections.unmodifiableList` — copied
/// from the wrapper's own [`UNMOD_FIELD_IMMUTABLE`] at mint time, because the
/// iterator carries only a snapshot `Object[]` and loses the wrapper once
/// built. Read by `getclass_display_class_id`'s `CollListItr` arm
/// (native-builtins) to pick `ImmutableCollections$ListItr` vs one of the
/// `Collections$Unmodifiable*` names — see [`UNMOD_LIST_ITR_VIA_LIST_ITERATOR`]
/// for the other half of that choice. CONTRACT: keep the slot index in sync
/// with that reader.
pub(crate) const UNMOD_LIST_ITR_IMMUTABLE: usize = 2;
/// Whether this iterator was minted by `listIterator()`/`listIterator(int)`
/// rather than `iterator()`. Both entry points share this one carrier class —
/// `native_unmod_iterator`'s list-wrapper branch and
/// `native_unmod_list_iterator[_idx]` all bottom out in
/// [`alloc_unmod_list_itr`] — but HotSpot hands out two different classes for
/// an `unmodifiableList` wrapper (`Collections$UnmodifiableCollection$1` for
/// `iterator()`, `Collections$UnmodifiableList$1` for `listIterator()`; a
/// `List.of` wrapper answers `ImmutableCollections$ListItr` either way, so
/// this bit is only consulted when [`UNMOD_LIST_ITR_IMMUTABLE`] is unset).
/// Measured: `apps/probes/ItrNames` against HotSpot 25. CONTRACT: keep the
/// slot index in sync with `getclass_display_class_id`'s `CollListItr` arm.
pub(crate) const UNMOD_LIST_ITR_VIA_LIST_ITERATOR: usize = 3;

/// Snapshot the elements of an unmodifiable-list wrapper into an `Object[]`,
/// by delegating to the backing collection's `toArray()`.
pub(crate) fn unmod_list_snapshot(ctx: &mut dyn NativeContext, args: &[Value]) -> Option<ObjectRef> {
    match unmod_delegate(ctx, args, "toArray", "()[Ljava/lang/Object;") {
        Ok(Some(Value::Object(Some(arr)))) => Some(arr),
        _ => None,
    }
}

/// Allocate a read-only `ListIterator` over `snapshot`, positioned at `cursor`.
/// Fallible since 2026-08-05 (Real-JDK wave 2, lane L7) — see
/// [`alloc_unmod_wrapper`] for why these views are refused under `--real-jdk`.
///
/// `immutable` and `via_list_iterator` are display-only (see the two constants
/// above) and change nothing about how this iterator walks or fails.
pub(crate) fn alloc_unmod_list_itr(
    ctx: &mut dyn NativeContext,
    snapshot: ObjectRef,
    cursor: i32,
    immutable: bool,
    via_list_iterator: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    // GC-SAFETY: same contract as `make_iterator_from_array` — the shell
    // allocation can relocate the snapshot. Covered by
    // `gc_native_pins::unmodifiable_list_iterator_roots_snapshot_graph_across_allocation`.
    let snapshot_pin = ctx.pin_native_root(snapshot);
    // Not `?`: `snapshot_pin` is this frame's pin base and must be released
    // before unwinding.
    let it = match try_alloc_synthetic(ctx, UNMOD_LIST_ITR_CLASS, 4) {
        Ok(it) => it,
        Err(err) => {
            ctx.unpin_native_roots(snapshot_pin);
            return Err(err);
        }
    };
    let it_pin = ctx.pin_native_root(it);
    let it = ctx.read_native_pin(it_pin, it);
    let snapshot = ctx.read_native_pin(snapshot_pin, snapshot);
    ctx.set_field(it, UNMOD_LIST_ITR_SNAPSHOT, Value::Object(Some(snapshot)));
    let it = ctx.read_native_pin(it_pin, it);
    ctx.set_field(it, UNMOD_LIST_ITR_CURSOR, Value::Int(cursor));
    let it = ctx.read_native_pin(it_pin, it);
    ctx.set_field(it, UNMOD_LIST_ITR_IMMUTABLE, Value::Int(immutable as i32));
    let it = ctx.read_native_pin(it_pin, it);
    ctx.set_field(
        it,
        UNMOD_LIST_ITR_VIA_LIST_ITERATOR,
        Value::Int(via_list_iterator as i32),
    );
    let it = ctx.read_native_pin(it_pin, it);
    ctx.unpin_native_roots(snapshot_pin);
    Ok(it)
}

/// `listIterator()` returns a read-only `ListIterator` over a `toArray()`
/// SNAPSHOT of the backing list, not the live list HotSpot's
/// `Collections$UnmodifiableList$1` walks (CD16-4). Unobservable for an
/// immutable-factory wrapper (its backing never changes); for a
/// `Collections.unmodifiableList` wrapper it is reached only where the
/// stand-in itself is: `--compatible` with `-jdk-unmodifiable-views`, or an
/// image without the JDK's bytecode. Over a real JDK both modes run the JDK's
/// factory and views (`j27_unmodifiable_view_factory_registration_tests`;
/// `docs/feature-designs/collections-interception.md`).
pub(crate) fn native_unmod_list_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let snapshot = match unmod_list_snapshot(ctx, args) {
        Some(a) => a,
        None => return Ok(Some(Value::Object(None))),
    };
    let immutable = matches!(args.first(), Some(Value::Object(Some(w)))
        if unmod_is_immutable(&*ctx, *w));
    Ok(Some(Value::Object(Some(alloc_unmod_list_itr(
        ctx, snapshot, 0, immutable, true,
    )?))))
}

/// `listIterator(int)` returns a read-only `ListIterator` positioned at `index`.
pub(crate) fn native_unmod_list_iterator_idx(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let snapshot = match unmod_list_snapshot(ctx, args) {
        Some(a) => a,
        None => return Ok(Some(Value::Object(None))),
    };
    let len = ctx.array_length(snapshot) as i32;
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let immutable = matches!(args.first(), Some(Value::Object(Some(this)))
        if unmod_is_immutable(&*ctx, *this));
    if index < 0 || index > len {
        // The JDK throws the PLAIN `IndexOutOfBoundsException`. Throwing the
        // subclass because "a `catch (IndexOutOfBoundsException)` still catches
        // it" was the old rationale here, and it is the wrong way round: the
        // subclass passes every catch the superclass would, and fails every
        // test of the class itself.
        //
        // The wording follows the same split as `unmod_list_oob_error` — a
        // SPACE for the immutable factories ("Index: 5 Size: 2"),
        // `ArrayList`/`LinkedList`'s comma for an unmodifiable view.
        let message = if immutable {
            format!("Index: {index} Size: {len}")
        } else {
            format!("Index: {index}, Size: {len}")
        };
        return Err(RuntimeError::ioobe(message).into());
    }
    Ok(Some(Value::Object(Some(alloc_unmod_list_itr(
        ctx, snapshot, index, immutable, true,
    )?))))
}

/// Read the (snapshot, cursor) state out of a list-iterator wrapper.
pub(crate) fn unmod_list_itr_state(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Option<(ObjectRef, i32, i32)> {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    let snapshot = match ctx.get_field(this, UNMOD_LIST_ITR_SNAPSHOT) {
        Value::Object(Some(a)) => a,
        _ => return None,
    };
    let cursor = match ctx.get_field(this, UNMOD_LIST_ITR_CURSOR) {
        Value::Int(c) => c,
        _ => 0,
    };
    let len = ctx.array_length(snapshot) as i32;
    Some((snapshot, cursor, len))
}

pub(crate) fn native_unmod_listitr_has_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match unmod_list_itr_state(ctx, args) {
        Some((_, cursor, len)) => Ok(Some(Value::Int(if cursor < len { 1 } else { 0 }))),
        None => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_unmod_listitr_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (this, snapshot, cursor, len) = match (args.first(), unmod_list_itr_state(ctx, args)) {
        (Some(Value::Object(Some(o))), Some((s, c, l))) => (*o, s, c, l),
        _ => {
            return Err(RuntimeError::NoSuchElementException {
                message: String::new(),
            }
            .into());
        }
    };
    if cursor >= len {
        return Err(RuntimeError::NoSuchElementException {
            message: String::new(),
        }
        .into());
    }
    let elem = ctx.get_array_element(snapshot, cursor as usize);
    ctx.set_field(this, UNMOD_LIST_ITR_CURSOR, Value::Int(cursor + 1));
    Ok(Some(elem))
}

pub(crate) fn native_unmod_listitr_has_previous(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match unmod_list_itr_state(ctx, args) {
        Some((_, cursor, _)) => Ok(Some(Value::Int(if cursor > 0 { 1 } else { 0 }))),
        None => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_unmod_listitr_previous(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (this, snapshot, cursor, _) = match (args.first(), unmod_list_itr_state(ctx, args)) {
        (Some(Value::Object(Some(o))), Some((s, c, l))) => (*o, s, c, l),
        _ => {
            return Err(RuntimeError::NoSuchElementException {
                message: String::new(),
            }
            .into());
        }
    };
    if cursor <= 0 {
        return Err(RuntimeError::NoSuchElementException {
            message: String::new(),
        }
        .into());
    }
    let idx = cursor - 1;
    let elem = ctx.get_array_element(snapshot, idx as usize);
    ctx.set_field(this, UNMOD_LIST_ITR_CURSOR, Value::Int(idx));
    Ok(Some(elem))
}

pub(crate) fn native_unmod_listitr_next_index(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match unmod_list_itr_state(ctx, args) {
        Some((_, cursor, _)) => Ok(Some(Value::Int(cursor))),
        None => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_unmod_listitr_previous_index(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match unmod_list_itr_state(ctx, args) {
        Some((_, cursor, _)) => Ok(Some(Value::Int(cursor - 1))),
        None => Ok(Some(Value::Int(-1))),
    }
}

pub(crate) fn native_unmod_listitr_for_each_remaining(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let (this, snapshot, mut cursor, len) = match (args.first(), unmod_list_itr_state(ctx, args)) {
        (Some(Value::Object(Some(o))), Some((s, c, l))) => (*o, s, c, l),
        _ => return Ok(None),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => return Ok(None),
    };
    let this_pin = ctx.pin_native_root(this);
    let snapshot_pin = ctx.pin_native_root(snapshot);
    let consumer_pin = ctx.pin_native_root(consumer);
    while cursor < len {
        let this = ctx.read_native_pin(this_pin, this);
        let snapshot = ctx.read_native_pin(snapshot_pin, snapshot);
        let elem = ctx.get_array_element(snapshot, cursor as usize);
        let elem_pin = match elem {
            Value::Object(Some(o)) => ctx.pin_native_root(o),
            _ => usize::MAX,
        };
        cursor += 1;
        ctx.set_field(this, UNMOD_LIST_ITR_CURSOR, Value::Int(cursor));
        let consumer = ctx.read_native_pin(consumer_pin, consumer);
        let elem = read_pinned_elem(ctx, elem_pin, elem);
        if let Err(e) = ctx.invoke_virtual(consumer, "accept", "(Ljava/lang/Object;)V", &[elem]) {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
        if elem_pin != usize::MAX {
            ctx.unpin_native_roots(elem_pin);
        }
    }
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

// ---- Map delegations ------------------------------------------------------

pub(crate) fn native_unmod_map_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    reject_null_immutable_query(&*ctx, args, 1)?;
    unmod_delegate(ctx, args, "get", "(Ljava/lang/Object;)Ljava/lang/Object;")
}

pub(crate) fn native_unmod_map_get_or_default(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    unmod_delegate(
        ctx,
        args,
        "getOrDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    )
}

pub(crate) fn native_unmod_map_contains_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    reject_null_immutable_query(&*ctx, args, 1)?;
    unmod_delegate(ctx, args, "containsKey", "(Ljava/lang/Object;)Z")
}

pub(crate) fn native_unmod_map_contains_value_checked(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    reject_null_immutable_query(&*ctx, args, 1)?;
    native_unmod_map_contains_value(ctx, args)
}

pub(crate) fn native_unmod_map_contains_value(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    unmod_delegate(ctx, args, "containsValue", "(Ljava/lang/Object;)Z")
}

pub(crate) fn native_unmod_map_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "forEach", "(Ljava/util/function/BiConsumer;)V")
}

/// `keySet()` returns an unmodifiable Set view of the backing map's key set.
pub(crate) fn native_unmod_map_key_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let from_immutable = matches!(args.first(), Some(Value::Object(Some(m)))
        if unmod_is_immutable(&*ctx, *m));
    let ks = unmod_delegate(ctx, args, "keySet", "()Ljava/util/Set;")?;
    if let Some(Value::Object(Some(inner))) = ks {
        let w = alloc_unmod_view_wrapper(ctx, UNMOD_SET_CLASS, inner, from_immutable)?;
        return Ok(Some(Value::Object(Some(w))));
    }
    Ok(ks)
}

/// `values()` returns an unmodifiable Collection view of the backing values.
pub(crate) fn native_unmod_map_values(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let from_immutable = matches!(args.first(), Some(Value::Object(Some(m)))
        if unmod_is_immutable(&*ctx, *m));
    let vs = unmod_delegate(ctx, args, "values", "()Ljava/util/Collection;")?;
    if let Some(Value::Object(Some(inner))) = vs {
        let w = alloc_unmod_view_wrapper(ctx, UNMOD_COLLECTION_CLASS, inner, from_immutable)?;
        return Ok(Some(Value::Object(Some(w))));
    }
    Ok(vs)
}

/// `entrySet()` returns an unmodifiable Set view of the backing entry set.
/// Uses `UNMOD_ENTRY_SET_CLASS` (not the plain `UNMOD_SET_CLASS`) so its
/// iterator/forEach wrap each yielded `Map.Entry` — a raw entry from the
/// backing map would let `entry.setValue(...)` mutate through the
/// "unmodifiable" view instead of throwing.
pub(crate) fn native_unmod_map_entry_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let from_immutable = matches!(args.first(), Some(Value::Object(Some(m)))
        if unmod_is_immutable(&*ctx, *m));
    let es = unmod_delegate(ctx, args, "entrySet", "()Ljava/util/Set;")?;
    if let Some(Value::Object(Some(inner))) = es {
        let w = alloc_unmod_view_wrapper(ctx, UNMOD_ENTRY_SET_CLASS, inner, from_immutable)?;
        return Ok(Some(Value::Object(Some(w))));
    }
    Ok(es)
}

// ===========================================================================
// Phase 39: Collections extras — unmodifiableMap/Set, emptyMap/Set/Iterator,
//           frequency, disjoint, singleton, Enumeration stubs
// ===========================================================================

pub(crate) fn register_collections_extras_natives(r: &mut NativeMethodRegistry) {
            let c = "java/util/Collections";
    r.register(
        c,
        "emptyMap",
        "()Ljava/util/Map;",
        native_collections_empty_map, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "emptySet",
        "()Ljava/util/Set;",
        native_collections_empty_set, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "emptyIterator",
        "()Ljava/util/Iterator;",
        native_collections_empty_iterator, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "singleton",
        "(Ljava/lang/Object;)Ljava/util/Set;",
        native_collections_singleton, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "singletonMap",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map;",
        native_collections_singleton_map, cratonvm_native_api::NativeKind::Bridge,
    );
    // The six `unmodifiable*` factories only, in their own window — this
    // registrar also holds `emptyMap` / `singleton*` / `synchronized*`, which
    // are a different question. See `register_unmodifiable_natives` for why
    // this family has a real-bytecode fallback and the iterator family does
    // not (Real-JDK wave 2, lane L7 residual R1, 2026-08-05).
            r.register(
        c,
        "unmodifiableMap",
        "(Ljava/util/Map;)Ljava/util/Map;",
        native_collections_unmodifiable_map, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        c,
        "unmodifiableSet",
        "(Ljava/util/Set;)Ljava/util/Set;",
        native_collections_unmodifiable_set, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // Keep the real-JDK bytecode authoritative for the sorted/navigable map
    // factories. The plain synthetic UnmodifiableMap stamp implements Map,
    // not SortedMap/NavigableMap; routing these factories to the plain-map
    // callback therefore violates their declared return type and makes
    // Charset.availableCharsets() fail its checkcast. The JDK implementations
    // create the correctly typed, read-only wrapper classes.
    r.register(
        c,
        "unmodifiableSortedSet",
        "(Ljava/util/SortedSet;)Ljava/util/SortedSet;",
        native_collections_unmodifiable_sorted_set, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        c,
        "unmodifiableNavigableSet",
        "(Ljava/util/NavigableSet;)Ljava/util/NavigableSet;",
        native_collections_unmodifiable_navigable_set, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        c,
        "unmodifiableCollection",
        "(Ljava/util/Collection;)Ljava/util/Collection;",
        native_collections_unmodifiable_collection, cratonvm_native_api::NativeKind::SyntheticStub,
    );
        r.register(
        c,
        "synchronizedList",
        "(Ljava/util/List;)Ljava/util/List;",
        native_collections_synchronized_list, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "synchronizedMap",
        "(Ljava/util/Map;)Ljava/util/Map;",
        native_collections_synchronized_map, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "synchronizedSet",
        "(Ljava/util/Set;)Ljava/util/Set;",
        native_collections_synchronized_set, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "synchronizedCollection",
        "(Ljava/util/Collection;)Ljava/util/Collection;",
        native_collections_synchronized_collection, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "frequency",
        "(Ljava/util/Collection;Ljava/lang/Object;)I",
        native_collections_frequency, cratonvm_native_api::NativeKind::Bridge,
    );
    // `Collections.disjoint` deliberately NOT registered: the previous native
    // (`native_collections_disjoint`) only read ArrayList-backed collections via
    // `al_state` and bailed to "disjoint = true" for any other Collection — so
    // `Collections.disjoint(hashSetA, hashSetB)` always claimed no overlap even
    // when the sets shared elements (keycloak DisclosureRedList red-list guard
    // never fired). The real pure-Java `Collections.disjoint` iterates one
    // collection and calls `contains` on the other; both work for HashSet on
    // CratonVM, so we let the real bytecode run. (Two more stale stubs lived in
    // native-builtins phases_early/phases_late — also removed.)
    r.register(
        c,
        "max",
        "(Ljava/util/Collection;)Ljava/lang/Object;",
        native_collections_max, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "min",
        "(Ljava/util/Collection;)Ljava/lang/Object;",
        native_collections_min, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "swap", "(Ljava/util/List;II)V", native_collections_swap, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "fill",
        "(Ljava/util/List;Ljava/lang/Object;)V",
        native_collections_fill, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "nCopies",
        "(ILjava/lang/Object;)Ljava/util/List;",
        native_collections_n_copies, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "shuffle",
        "(Ljava/util/List;)V",
        native_collections_shuffle, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "addAll",
        "(Ljava/util/Collection;[Ljava/lang/Object;)Z",
        native_collections_add_all, cratonvm_native_api::NativeKind::Bridge,
    );
    // List.copyOf / Set.copyOf / Map.copyOf — `SyntheticStub` for the same
    // reason as `unmodifiableList` above: each returns a `cratonvm/internal/*`
    // stand-in that `--real-jdk` refuses to fabricate, and each is a duplicate
    // of a registration `register_factory_natives` already retagged.
    //
    // `CRATONVM_COMPAT_JDK_IMMUTABLES` (read once here, per VM): not registered
    // at all, so `--compatible` runs the JDK's `ImmutableCollections.listCopy`
    // / `setCopy` / `Map.copyOf` bodies and hands out `List12` / `ListN` /
    // `Set12` / `SetN` / `Map1` / `MapN`, as HotSpot and `--real-jdk` do.
    // Measured first, as for `List.of` in wave 48: a real immutable collection
    // already answers every row of `L6W49CopyOfIsTheJdks` under `--compatible`
    // (invokeinterface reaches its own bytecode). Interpreter round i1 wave 49,
    // lane L6: stage 4 of
    // `docs/feature-designs/real-jdk-mode.md`.
    // The HotSpot-alignment row `jdk-immutables` since interpreter round i6
    // wave 118, lane L5.
    if !r.aligned(cratonvm_types::HotSpotAlignment::CompatJdkImmutables) {
        r.register(
            "java/util/List",
            "copyOf",
            "(Ljava/util/Collection;)Ljava/util/List;",
            native_list_copy_of,
            cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            "java/util/Set",
            "copyOf",
            "(Ljava/util/Collection;)Ljava/util/Set;",
            native_set_copy_of,
            cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            "java/util/Map",
            "copyOf",
            "(Ljava/util/Map;)Ljava/util/Map;",
            native_map_copy_of,
            cratonvm_native_api::NativeKind::SyntheticStub,
        );
    }

    // Enumeration interface
    let en = "java/util/Enumeration";
    r.register(en, "hasMoreElements", "()Z", native_snapshot_itr_has_next, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        en,
        "nextElement",
        "()Ljava/lang/Object;",
        native_snapshot_itr_next, cratonvm_native_api::NativeKind::Bridge,
    );

    // Snapshot Enumeration used by `Properties.propertyNames()` and similar
    // call sites that need a real Enumeration over a fixed key list. We can't
    // reuse `Collections$EmptyEnumeration` because its real-JDK bytecode for
    // `hasMoreElements` is hardcoded to `iconst_0; ireturn` and wins concrete
    // dispatch over the interface-level Enumeration native, so a non-empty
    // snapshot built on EmptyEnumeration appears empty. Register concrete
    // overrides on a dedicated synthetic class.
    let sne = "cratonvm/internal/SnapshotEnumeration";
    r.register(sne, "hasMoreElements", "()Z", native_snapshot_itr_has_next, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        sne,
        "nextElement",
        "()Ljava/lang/Object;",
        native_snapshot_itr_next, cratonvm_native_api::NativeKind::Bridge,
    );
    }

/// Identity — just returns the first argument (used for synchronized wrappers)
pub(crate) fn native_collections_identity(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    Ok(Some(args.first().cloned().unwrap_or(Value::Object(None))))
}

/// `Collections.synchronizedMap(m)` — wrap in a REAL `Collections$SynchronizedMap`
/// so every method (notably `computeIfAbsent`) is `synchronized (mutex)`. The old
/// identity stub returned the bare map, making `synchronizedMap(...).computeIfAbsent`
/// non-atomic under concurrency (duplicate values for the same key) — which broke
/// Spring's `ConcurrencyLimitBeanPostProcessor` per-proxy throttle cache. Null
/// passes through.
pub(crate) fn native_collections_synchronized_map(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(v @ Value::Object(Some(_))) => ctx.new_object_initialized(
            "java/util/Collections$SynchronizedMap",
            "(Ljava/util/Map;)V",
            &[v.clone()],
        ),
        other => Ok(Some(other.cloned().unwrap_or(Value::Object(None)))),
    }
}

/// `Collections.synchronizedList(l)` — real `SynchronizedList`, or
/// `SynchronizedRandomAccessList` when the backing implements `RandomAccess`.
/// Same rationale as [`native_collections_synchronized_map`].
pub(crate) fn native_collections_synchronized_list(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    reject_null_collections_arg(args.first())?;
    match args.first() {
        Some(v @ Value::Object(Some(list))) => {
            let is_random_access = match ctx.class_id_by_name("java/util/RandomAccess") {
                Some(ra) => ctx.is_subclass(ctx.class_id_of_object(*list), ra),
                None => false,
            };
            let cls = if is_random_access {
                "java/util/Collections$SynchronizedRandomAccessList"
            } else {
                "java/util/Collections$SynchronizedList"
            };
            ctx.new_object_initialized(cls, "(Ljava/util/List;)V", &[v.clone()])
        }
        other => Ok(Some(other.cloned().unwrap_or(Value::Object(None)))),
    }
}

/// `Collections.synchronizedSet(s)` — real `SynchronizedSet`.
pub(crate) fn native_collections_synchronized_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(v @ Value::Object(Some(_))) => ctx.new_object_initialized(
            "java/util/Collections$SynchronizedSet",
            "(Ljava/util/Set;)V",
            &[v.clone()],
        ),
        other => Ok(Some(other.cloned().unwrap_or(Value::Object(None)))),
    }
}

/// `Collections.synchronizedCollection(c)` — real `SynchronizedCollection`.
pub(crate) fn native_collections_synchronized_collection(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(v @ Value::Object(Some(_))) => ctx.new_object_initialized(
            "java/util/Collections$SynchronizedCollection",
            "(Ljava/util/Collection;)V",
            &[v.clone()],
        ),
        other => Ok(Some(other.cloned().unwrap_or(Value::Object(None)))),
    }
}

// `List.copyOf` / `Set.copyOf` / `Map.copyOf` — return an INDEPENDENT immutable
// snapshot of the source (JDK contract), NOT the source itself. The previous
// identity impl aliased the source: a later mutation of the source leaked into
// the "copy". Surfaced as Kafka AdminApiDriver, whose `RequestSpec.keys` is
// built via `Set.copyOf(scopeKeys)` — when the driver later mapped another key
// to the same broker, the already-returned spec's key set gained that key
// (`{bar}` -> `{bar, foo}`). Fix: build a fresh backing collection from the
// source's elements, then wrap it unmodifiable so reads see the snapshot and
// mutators throw.
pub(crate) fn native_list_copy_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `List.copyOf(null)` is `ImmutableCollections.listCopy(coll)`, whose
    // `coll.isEmpty()` dereferences it. A usable empty list for a null argument
    // is the fabricated-success shape.
    reject_null_copy_of_arg(
        args.first(),
        "Cannot invoke \"java.util.Collection.isEmpty()\" because \"coll\" is null",
    )?;
    let src = args.first().copied().unwrap_or(Value::Object(None));
    // `List.copyOf(x) == x` when `x` is ALREADY immutable — not an optimisation
    // the JDK left implicit: `ImmutableCollections.listCopy` returns its
    // argument, and application code compares the two with `==`. The
    // independent-snapshot rule above still holds for everything else, which is
    // what `already_immutable_of` is careful about.
    if let Some(same) = already_immutable_of(ctx, src, UNMOD_LIST_CLASS) {
        return Ok(Some(Value::Object(Some(same))));
    }
    // C4-3: `listCopy`'s `coll.isEmpty()` -> `List.of()`.
    let (empty, mut src) = copy_of_empty_pinned(ctx, src, UNMOD_LIST_CLASS)?;
    if let Some(empty) = empty {
        return Ok(Some(Value::Object(Some(empty))));
    }
    // CW2-5: `List.of(coll.toArray())`.
    if let Some(r) = list_copy_of_one_pass(ctx, &mut src) {
        return r;
    }
    // GC-safety: the allocation and the copy-in below both collect, and `src`
    // and `backing` are bare locals read after each. Same idiom, and the same
    // reason, as `native_map_copy_of` — which had it and these two did not.
    //
    // Measured, not inferred: on BindableTests under `CRATONVM_DBG_GC_STRESS`
    // `native_al_init_from_collection` relocated `backing`, and the pre-move
    // address went into the wrapper's slot 0 and out to the program. See
    // `docs/feature-designs/real-jdk-mode.md`'s BindableTests residual page.
    //
    // The pin goes up BEFORE the null scan: `source_collection_has_null` runs
    // the source's `toArray()` (Java), and pinning after it pinned an address
    // that call may already have moved (round 15 wave 2, lane compat).
    let src_pin = pin_value(ctx, src);
    if let Value::Object(Some(s)) = src {
        if source_collection_has_null(ctx, s) {
            if src_pin != usize::MAX {
                ctx.unpin_native_roots(src_pin);
            }
            return Err(MethodCallFailed::from(RuntimeError::NullPointerException {
                message: None,
            }));
        }
    }
    let mut backing = try_alloc_synthetic(ctx, "java/util/ArrayList", AL_NUM_FIELDS)?;
    let src = read_pinned_elem(ctx, src_pin, src);
    let backing_at_call = backing;
    let copied = rooted_across(ctx, &mut [&mut backing], |ctx| {
        native_al_init_from_collection(ctx, &[Value::Object(Some(backing_at_call)), src])
    });
    if src_pin != usize::MAX {
        ctx.unpin_native_roots(src_pin);
    }
    copied?;
    Ok(Some(Value::Object(Some(alloc_immutable_wrapper(
        ctx,
        UNMOD_LIST_CLASS,
        backing,
    )?))))
}

pub(crate) fn native_set_copy_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    reject_null_copy_of_arg(
        args.first(),
        "Cannot invoke \"java.util.Collection.isEmpty()\" because \"coll\" is null",
    )?;
    let src = args.first().copied().unwrap_or(Value::Object(None));
    // Identity for an already-immutable source, as `List.copyOf`. NOTE the
    // asymmetry with `Set.of`: `copyOf` DEDUPLICATES and must not throw on a
    // repeat (`Set.copyOf(List.of("a","a")).size() == 1`); only nulls are
    // rejected.
    if let Some(same) = already_immutable_of(ctx, src, UNMOD_SET_CLASS) {
        return Ok(Some(Value::Object(Some(same))));
    }
    // C4-3: `Set.copyOf`'s `coll.isEmpty()` -> `Set.of()`.
    let (empty, src) = copy_of_empty_pinned(ctx, src, UNMOD_SET_CLASS)?;
    if let Some(empty) = empty {
        return Ok(Some(Value::Object(Some(empty))));
    }
    // CW2-5: `Set.of(new HashSet<>(coll).toArray())`.
    if copyof_one_pass_enabled() {
        return set_copy_of_one_pass(ctx, src);
    }
    // GC-safety: see `native_list_copy_of` — identical shape, identical fix,
    // the pin ahead of the null scan's `toArray()` included.
    let src_pin = pin_value(ctx, src);
    if let Value::Object(Some(s)) = src {
        if source_collection_has_null(ctx, s) {
            if src_pin != usize::MAX {
                ctx.unpin_native_roots(src_pin);
            }
            return Err(MethodCallFailed::from(RuntimeError::NullPointerException {
                message: None,
            }));
        }
    }
    let mut backing = try_alloc_synthetic(ctx, "java/util/HashSet", HS_NUM_FIELDS)?;
    let src = read_pinned_elem(ctx, src_pin, src);
    let backing_at_call = backing;
    let copied = rooted_across(ctx, &mut [&mut backing], |ctx| {
        native_hs_init_from_collection(ctx, &[Value::Object(Some(backing_at_call)), src])
    });
    if src_pin != usize::MAX {
        ctx.unpin_native_roots(src_pin);
    }
    copied?;
    Ok(Some(Value::Object(Some(alloc_immutable_wrapper(
        ctx,
        UNMOD_SET_CLASS,
        backing,
    )?))))
}

pub(crate) fn native_map_copy_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    reject_null_copy_of_arg(
        args.first(),
        "Cannot invoke \"java.util.Map.isEmpty()\" because \"map\" is null",
    )?;
    let src = args.first().copied().unwrap_or(Value::Object(None));
    if let Some(same) = already_immutable_of(ctx, src, UNMOD_MAP_CLASS) {
        return Ok(Some(Value::Object(Some(same))));
    }
    // C4-3: `Map.copyOf`'s `map.isEmpty()` -> `Map.of()`.
    let (empty, src) = copy_of_empty_pinned(ctx, src, UNMOD_MAP_CLASS)?;
    if let Some(empty) = empty {
        return Ok(Some(Value::Object(Some(empty))));
    }
    // CW2-5: `Map.ofEntries(map.entrySet().toArray(new Entry[0]))` -- the
    // source's entries in its own order, through `of_map` (`Map1` / `MapN`'s
    // checks: a key an `IdentityHashMap` holds twice is the JDK's `duplicate
    // key`, where the `HashMap` copy below folded it; a null is found per pair
    // in order).
    if copyof_one_pass_enabled() {
        if let Value::Object(Some(s)) = src {
            // C3W3-2 (`CRATONVM_COMPAT_MAP_COPY_JDK_QUESTIONS`, round 15 wave
            // 4, lane compat4): a program's map is asked `entrySet().toArray(new
            // Entry[0])`, then each entry `getKey()` / `getValue()` -- not
            // `isEmpty()` a second time and an iterator walk.
            let pairs = match foreign_map_source(&*ctx, s) {
                Some(prog) => map_entries_by_to_array(ctx, prog)?,
                // Handed straight to `of_map`, which pins them first.
                None => collect_entries_any(ctx, s)?,
            };
            return of_map(ctx, &pairs);
        }
    }
    // GC-safety: the allocation and the copy-in below both collect; `src` and
    // `backing` are bare locals used after each. See `rooted_across`.
    let src_pin = pin_value(ctx, src);
    let mut backing = try_alloc_synthetic(ctx, "java/util/HashMap", MAP_NUM_FIELDS)?;
    let src = read_pinned_elem(ctx, src_pin, src);
    let backing_at_call = backing;
    let copied = rooted_across(ctx, &mut [&mut backing], |ctx| {
        native_map_init_from_map(ctx, &[Value::Object(Some(backing_at_call)), src])
    });
    if src_pin != usize::MAX {
        ctx.unpin_native_roots(src_pin);
    }
    copied?;
    // `Map.copyOf` rejects a null key or value, like the `of` factories. Read
    // off the BUILT backing (ours, so the walk is a field read) rather than
    // re-entering the source's `entrySet`.
    if map_collect_entries(ctx, backing)
        .iter()
        .any(|(k, v)| matches!(k, Value::Object(None)) || matches!(v, Value::Object(None)))
    {
        return Err(MethodCallFailed::from(RuntimeError::NullPointerException {
            message: None,
        }));
    }
    let wrapper = alloc_immutable_wrapper(ctx, UNMOD_MAP_CLASS, backing)?;
    Ok(Some(Value::Object(Some(wrapper))))
}

/// `Collections.unmodifiableMap` — wrap the source map in a live read-only view.
pub(crate) fn native_collections_unmodifiable_map(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(Value::Object(Some(src))) => {
            let w = alloc_unmod_wrapper(ctx, UNMOD_MAP_CLASS, *src)?;
            Ok(Some(Value::Object(Some(w))))
        }
        _ => Ok(Some(Value::Object(None))),
    }
}

/// `Collections.unmodifiableSet` — wrap the source set in a live read-only view.
///
/// Uses the plain `UNMOD_SET_CLASS` stamp (Set/Collection/Serializable only —
/// NOT SortedSet/NavigableSet). `unmodifiableSortedSet`/`unmodifiableNavigableSet`
/// go through `native_collections_unmodifiable_sorted_set` below instead, so a
/// plain wrapped `LinkedHashSet`/`HashSet` never claims a `comparator()` (etc.)
/// surface it can't actually delegate (real `LinkedHashSet` has no such method).
pub(crate) fn native_collections_unmodifiable_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(Value::Object(Some(src))) => {
            let w = alloc_unmod_wrapper(ctx, UNMOD_SET_CLASS, *src)?;
            Ok(Some(Value::Object(Some(w))))
        }
        _ => Ok(Some(Value::Object(None))),
    }
}

/// `Collections.unmodifiableSortedSet` ? wrap as a read-only SortedSet view.
pub(crate) fn native_collections_unmodifiable_sorted_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(Value::Object(Some(src))) => {
            let w = alloc_unmod_wrapper(ctx, UNMOD_SORTED_SET_CLASS, *src)?;
            Ok(Some(Value::Object(Some(w))))
        }
        _ => Ok(Some(Value::Object(None))),
    }
}

/// `Collections.unmodifiableNavigableSet` — wrap as a read-only NavigableSet view.
pub(crate) fn native_collections_unmodifiable_navigable_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(Value::Object(Some(src))) => {
            let w = alloc_unmod_wrapper(ctx, UNMOD_NAVIGABLE_SET_CLASS, *src)?;
            Ok(Some(Value::Object(Some(w))))
        }
        _ => Ok(Some(Value::Object(None))),
    }
}

/// `Collections.unmodifiableCollection` — wrap the source collection in a
/// live read-only view.
pub(crate) fn native_collections_unmodifiable_collection(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(Value::Object(Some(src))) => {
            let w = alloc_unmod_wrapper(ctx, UNMOD_COLLECTION_CLASS, *src)?;
            Ok(Some(Value::Object(Some(w))))
        }
        _ => Ok(Some(Value::Object(None))),
    }
}

pub(crate) fn native_collections_empty_map(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    if let Some(v) = collections_empty_singleton(ctx, "EMPTY_MAP") {
        return Ok(Some(v));
    }
    // `alloc_real_jdk` FIRST, exactly as `native_collections_singleton_list`
    // does one screen up, and for the same reason: it resolves the class the
    // JDK would have returned in BOTH modes — real-JDK from the image, and
    // `--synthetic-jdk` from `class_manager`'s fabrication tables, which carry
    // `Collections$Empty*` field shapes and interface rows already. Only the
    // static-field cache above is real-Real-JDK.
    //
    // Without it this fallback minted an ordinary mutable synthetic, so in
    // `--synthetic-jdk` (measured 2026-09-02, `apps/probes/EmptySingletonImmutable`):
    //
    //     Collections.emptyList()  class=java.util.ArrayList
    //                              instanceof ArrayList = true
    //                              add("x") = SUCCEEDED
    //
    // The `add` is the defect. `ensure_collections_empty_singletons` above
    // records what a mutable empty singleton cost the last time one shipped —
    // kotlin-reflect's shaded protobuf tests `instanceof ArrayList` to decide
    // whether to replace its `emptyList()` placeholder, skipped the
    // replacement, and mutated the shared object. Here each call happens to
    // mint a FRESH list, so the write is not shared — it is silently DISCARDED
    // instead, which is the same class of wrong answer with a quieter failure.
    if let Some(o) = alloc_real_jdk(ctx, "java/util/Collections$EmptyMap") {
        return Ok(Some(Value::Object(Some(o))));
    }
    // Last resort: fresh synthetic empty map (gc-common w20-a: see
    // `alloc_initialised_backing_map`).
    let map = alloc_initialised_backing_map(ctx)?;
    Ok(Some(Value::Object(Some(map))))
}

pub(crate) fn native_collections_empty_set(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    if let Some(v) = collections_empty_singleton(ctx, "EMPTY_SET") {
        return Ok(Some(v));
    }
    // See `native_collections_empty_map` for why this precedes the synthetic.
    if let Some(o) = alloc_real_jdk(ctx, "java/util/Collections$EmptySet") {
        return Ok(Some(Value::Object(Some(o))));
    }
    // Last resort: fresh synthetic empty set (gc-common w20-a: see
    // `alloc_synthetic_hash_set`).
    let (set, _) = alloc_synthetic_hash_set(ctx)?;
    Ok(Some(Value::Object(Some(set))))
}

pub(crate) fn native_collections_empty_iterator(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let itr = alloc_empty_array_holder(ctx, "java/util/Collections$EmptyItr", 2)?;
    Ok(Some(Value::Object(Some(itr))))
}

pub(crate) fn native_collections_singleton(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let elem = args.first().cloned().unwrap_or(Value::Object(None));
    // gc-common w20-a: `elem` is a bare copy of the argument and every arm
    // below allocates before storing it; the synthetic arm also stored the map
    // into, and returned, the set's pre-allocation address. The element is
    // pinned for the whole call and read back at each use; the set is pinned
    // across the `put` (which allocates a node and runs `hashCode`).
    let elem_pin = pin_value(ctx, elem);
    let out = (|| -> MethodCallResult {
        if let Some(set) = alloc_real_jdk(ctx, "java/util/Collections$SingletonSet") {
            let elem = read_pinned_elem(ctx, elem_pin, elem);
            ctx.set_field_by_name(set, "element", elem);
            return Ok(Some(Value::Object(Some(set))));
        }
        let (set, inner_map) = alloc_synthetic_hash_set(ctx)?;
        // Add elem. The value was `Value::Object(None)` — a NULL marker, which
        // `native_hs_add`/`native_hs_remove` read as "the element is absent" and
        // which real `HashSet.remove`'s `== PRESENT` identity test can never
        // satisfy. See [`hs_present_marker_at`].
        let elem = read_pinned_elem(ctx, elem_pin, elem);
        let present = hs_present_marker(&*ctx, elem);
        let (set, put) = rooted_across1(ctx, set, |ctx| {
            native_map_put(ctx, &[Value::Object(Some(inner_map)), elem, present])
        });
        put?;
        Ok(Some(Value::Object(Some(set))))
    })();
    if elem_pin != usize::MAX {
        ctx.unpin_native_roots(elem_pin);
    }
    out
}

pub(crate) fn native_collections_singleton_map(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let key = args.first().cloned().unwrap_or(Value::Object(None));
    let val = args.get(1).cloned().unwrap_or(Value::Object(None));
    // gc-common w20-a: `key` and `val` are bare copies of the arguments and
    // both arms allocate before storing them; the synthetic arm also `put`
    // into, and returned, the map's pre-initialisation address. Both are
    // pinned for the whole call (`base` is the first pin taken).
    let (base, pins) = pin_value_slice(ctx, &[key, val]);
    let out = (|| -> MethodCallResult {
        if let Some(map) = alloc_real_jdk(ctx, "java/util/Collections$SingletonMap") {
            let key = read_pinned_elem(ctx, pins[0], key);
            let val = read_pinned_elem(ctx, pins[1], val);
            ctx.set_field_by_name(map, "k", key);
            ctx.set_field_by_name(map, "v", val);
            return Ok(Some(Value::Object(Some(map))));
        }
        let map = alloc_initialised_backing_map(ctx)?;
        let key = read_pinned_elem(ctx, pins[0], key);
        let val = read_pinned_elem(ctx, pins[1], val);
        let (map, put) = rooted_across1(ctx, map, |ctx| {
            native_map_put(ctx, &[Value::Object(Some(map)), key, val])
        });
        put?;
        Ok(Some(Value::Object(Some(map))))
    })();
    if base != usize::MAX {
        ctx.unpin_native_roots(base);
    }
    out
}

pub(crate) fn native_collections_frequency(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    reject_null_collections_arg(args.first())?;
    let coll = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let mut target = args.get(1).cloned().unwrap_or(Value::Object(None));
    let (data, size) = al_state(ctx, coll);
    // Stub-removal wave 2 (2026-07-28): `al_state` only understands the
    // ArrayList shape (field 0 = backing array, field 1 = size). EVERY other
    // Collection — `Arrays.asList` (an `Arrays$ArrayList`), `HashSet`,
    // `LinkedList`, any user Collection — fell into the `None` arm and
    // reported frequency 0, i.e. "this element does not occur", for a
    // collection full of matches. Measured against HotSpot 25:
    // Arrays.asList 3 -> 0, HashSet 1 -> 0, LinkedList 2 -> 0.
    // Drive the receiver's own `toArray()` instead; every Collection has one.
    let (data, size) = match data {
        Some(d) => (d, size),
        None => {
            let coll_pin = ctx.pin_native_root(coll);
            let target_pin = pin_value(ctx, target);
            let arr = ctx.invoke_virtual(coll, "toArray", "()[Ljava/lang/Object;", &[])?;
            target = read_pinned_elem(ctx, target_pin, target);
            ctx.unpin_native_roots(coll_pin);
            match arr {
                Some(Value::Object(Some(a))) => {
                    let n = ctx.array_length(a) as i32;
                    (a, n)
                }
                _ => return Ok(Some(Value::Int(0))),
            }
        }
    };
    // Family-1 fix (cce0079): pinned scan (counts every match, so no
    // early-exit helper).
    let data_pin = ctx.pin_native_root(data);
    let th = pin_value(ctx, target);
    let mut data = data;

    let mut count = 0i32;
    for i in 0..size as usize {
        let elem = ctx.get_array_element(data, i);
        let matched = list_element_matches(ctx, &elem, &target)?;
        data = ctx.read_native_pin(data_pin, data);
        target = read_pinned_elem(ctx, th, target);
        if matched {
            count += 1;
        }
    }
    ctx.unpin_native_roots(data_pin);
    Ok(Some(Value::Int(count)))
}

// Retained for reference only — no longer registered (see the note at the
// `Collections` registration site); the real JDK bytecode handles `disjoint`.
#[allow(dead_code)]
pub(crate) fn native_collections_disjoint(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let c1 = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(1))),
    };
    let c2 = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(1))),
    };
    let (data1, size1) = al_state(ctx, c1);
    let (data2, size2) = al_state(ctx, c2);
    let (d1, d2) = match (data1, data2) {
        (Some(a), Some(b)) => (a, b),
        _ => return Ok(Some(Value::Int(1))),
    };
    for i in 0..size1 as usize {
        let elem = ctx.get_array_element(d1, i);
        for j in 0..size2 as usize {
            let other = ctx.get_array_element(d2, j);
            if values_equal(ctx, &elem, &other) {
                return Ok(Some(Value::Int(0)));
            }
        }
    }
    Ok(Some(Value::Int(1)))
}

/// `Collections.max(Collection)` / `Collections.min(Collection)` share this
/// natural-ordering scan: pick element 0, then walk the rest comparing via
/// `Comparable.compareTo` (the real JDK contract — `max`/`min` are literally
/// specified in terms of `compareTo`), replacing the running candidate when
/// `cmp() > 0` (max) or `cmp() < 0` (min). Ties keep the earliest element,
/// matching `Collections.max`/`min`'s documented "first of equal maximums"
/// behavior.
///
/// The previous implementation compared `val_to_string(elem)` — i.e. each
/// element's `read_string()` decode — which only means something for actual
/// `java.lang.String` elements. For any other `Comparable` (e.g. a
/// `DeadNode`/custom class), `read_string` returns `None` for a non-String
/// object, so every element degraded to the same empty string and the `>`/`<`
/// comparison was *always false* — silently returning element 0 regardless of
/// true ordering. That produced a deterministic-but-wrong "minimum" for
/// `RestClient.selectNodes`'s `Collections.min(selectedDeadNodes)` dead-host
/// revival pick, always resurrecting the first-registered host instead of the
/// one with the lowest `deadUntilNanos` (RestClientMultipleHostsTests.
/// testRoundRobinRetryErrors: "host [...] not found, most likely used
/// multiple times").
pub(crate) fn native_collections_extreme(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    want_max: bool,
) -> MethodCallResult {
    reject_null_collections_arg(args.first())?;
    let coll = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, size) = al_state(ctx, coll);
    let elems = if let Some(data) = data {
        let len = size.max(0) as usize;
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            out.push(ctx.get_array_element(data, i));
        }
        out
    } else {
        collect_collection_elements_or_real(ctx, coll)?
    };
    if elems.is_empty() {
        return Err(
            cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: String::new(),
            }
            .into(),
        );
    }
    // cceres3: pin across GC-capable call (stream stale-at-store wave) — the
    // compareTo dispatch re-enters Java, moving `best` and every later elem;
    // track `best` by handle like native_stream_min does.
    let (elem_base, elem_handles) = pin_value_slice(ctx, &elems);
    let mut best = elems[0];
    let mut best_handle = elem_handles[0];
    for i in 1..elems.len() {
        let elem = read_pinned_elem(ctx, elem_handles[i], elems[i]);
        let best_arg = read_pinned_elem(ctx, best_handle, best);
        let cmp = match compare_via_compare_to(ctx, &elem, &best_arg) {
            Ok(c) => c,
            Err(e) => {
                if elem_base != usize::MAX {
                    ctx.unpin_native_roots(elem_base);
                }
                return Err(e);
            }
        };
        if (want_max && cmp > 0) || (!want_max && cmp < 0) {
            best = elems[i];
            best_handle = elem_handles[i];
        }
    }
    let best = read_pinned_elem(ctx, best_handle, best);
    if elem_base != usize::MAX {
        ctx.unpin_native_roots(elem_base);
    }
    Ok(Some(best))
}

pub(crate) fn native_collections_max(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_collections_extreme(ctx, args, true)
}

pub(crate) fn native_collections_min(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_collections_extreme(ctx, args, false)
}

pub(crate) fn native_collections_swap(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    reject_null_collections_arg(args.first())?;
    let list = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // MEASURED 2026-08-13 (scratchpad/orch/Exp.java): out-of-range is
    // IndexOutOfBoundsException with the JDK's ArrayList text. Two defects
    // here: there was NO bounds check at all, and `*v as usize` turned a
    // NEGATIVE index into a huge one, so `swap(l, -1, 0)` was silently
    // accepted rather than refused. Bound against the list SIZE, not the
    // backing array capacity -- an ArrayList over-allocates, so the array
    // length would admit indices past the end.
    let i = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => return Ok(None),
    };
    let j = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => return Ok(None),
    };
    let (data, size) = al_state(ctx, list);
    for idx in [i, j] {
        if idx < 0 || idx >= size {
            return Err(RuntimeError::IndexOutOfBoundsException {
                message: Some(format!("Index {idx} out of bounds for length {size}")),
            }
            .into());
        }
    }
    if let Some(d) = data {
        let (i, j) = (i as usize, j as usize);
        let a = ctx.get_array_element(d, i);
        let b = ctx.get_array_element(d, j);
        ctx.set_array_element(d, i, b);
        ctx.set_array_element(d, j, a);
        return Ok(None);
    }
    // Not an ArrayList-layout receiver. `al_state` answers `(None, 0)` for a
    // sublist view, a `LinkedList`, or any application `List`, and the loop
    // above then swapped nothing and reported success — a SILENT no-op, which
    // is what `probes/SubListBehaviourProbe`'s `write.swap` row measures. It
    // failed on both collectors AND under `--real-jdk`, because this native is
    // reached whatever runs `Collections.swap`. Go through the receiver's own
    // `get`/`set`, which is what the JDK's own body does.
    list_swap_via_accessors(ctx, list, i as i32, j as i32)
}

/// `Collections.swap` for a receiver whose backing array this file cannot
/// read: two `get`s and two `set`s through the list's own methods.
///
/// Each invoke can allocate and therefore move the other element, so both are
/// pinned across the pair.
pub(crate) fn list_swap_via_accessors(
    ctx: &mut dyn NativeContext,
    list: ObjectRef,
    i: i32,
    j: i32,
) -> MethodCallResult {
    let list_pin = ctx.pin_native_root(list);
    let a = ctx
        .invoke_virtual(list, "get", "(I)Ljava/lang/Object;", &[Value::Int(i)])?
        .unwrap_or(Value::Object(None));
    let a_pin = pin_value(ctx, a);
    let list = ctx.read_native_pin(list_pin, list);
    let b = ctx
        .invoke_virtual(list, "get", "(I)Ljava/lang/Object;", &[Value::Int(j)])?
        .unwrap_or(Value::Object(None));
    let b_pin = pin_value(ctx, b);
    let list = ctx.read_native_pin(list_pin, list);
    let a = read_pinned_elem(ctx, a_pin, a);
    let b = read_pinned_elem(ctx, b_pin, b);
    ctx.invoke_virtual(
        list,
        "set",
        "(ILjava/lang/Object;)Ljava/lang/Object;",
        &[Value::Int(i), b],
    )?;
    let list = ctx.read_native_pin(list_pin, list);
    let a = read_pinned_elem(ctx, a_pin, a);
    ctx.invoke_virtual(
        list,
        "set",
        "(ILjava/lang/Object;)Ljava/lang/Object;",
        &[Value::Int(j), a],
    )?;
    ctx.unpin_native_roots(list_pin);
    Ok(None)
}

pub(crate) fn native_collections_fill(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    reject_null_collections_arg(args.first())?;
    let list = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let val = args.get(1).cloned().unwrap_or(Value::Object(None));
    let (data, size) = al_state(ctx, list);
    if let Some(d) = data {
        for i in 0..size as usize {
            ctx.set_array_element(d, i, val);
        }
        return Ok(None);
    }
    // Same silent no-op as `native_collections_swap`, same fix: a receiver
    // whose backing array this file cannot read gets its own `size`/`set`.
    let list_pin = ctx.pin_native_root(list);
    let val_pin = pin_value(ctx, val);
    let n = match ctx.invoke_virtual(list, "size", "()I", &[])? {
        Some(Value::Int(n)) => n,
        _ => 0,
    };
    for i in 0..n {
        let list_cur = ctx.read_native_pin(list_pin, list);
        let val_cur = read_pinned_elem(ctx, val_pin, val);
        ctx.invoke_virtual(
            list_cur,
            "set",
            "(ILjava/lang/Object;)Ljava/lang/Object;",
            &[Value::Int(i), val_cur],
        )?;
    }
    ctx.unpin_native_roots(list_pin);
    Ok(None)
}

pub(crate) fn native_collections_n_copies(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let n = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let val = args.get(1).cloned().unwrap_or(Value::Object(None));
    // MEASURED 2026-08-13 (scratchpad/orch/Two.java): a negative length is
    // IllegalArgumentException with the count in the text, NOT a clamp. The
    // `n.max(0)` below turned nCopies(-1, x) into an empty list -- a plausible
    // answer, so nothing downstream could notice. The check precedes both
    // allocations and the pins: an early return between pin and unpin leaks.
    if n < 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("List length = {n}").into(),
        }
        .into());
    }
    let __al_n_fields = al_slots(ctx).2;
    // cceres3: pin across GC-capable call (stream stale-at-store wave) — the
    // two allocations can move `val` and the freshly allocated `list`.
    let val_handle = pin_value(ctx, val);
    // Not `?` (gc-common w20-a): a refusal releases the value's pin.
    let list = match try_alloc_synthetic(ctx, "java/util/ArrayList", __al_n_fields) {
        Ok(l) => l,
        Err(e) => {
            if val_handle != usize::MAX {
                ctx.unpin_native_roots(val_handle);            }
            return Err(e);
        }
    };
    let list_pin = ctx.pin_native_root(list);
    let arr: ObjectRef = alloc_ref_array(ctx, n.max(0) as usize);
    let val = read_pinned_elem(ctx, val_handle, val);
    for i in 0..n.max(0) as usize {
        ctx.set_array_element(arr, i, val);
    }
    let list = ctx.read_native_pin(list_pin, list);
    al_set_data(ctx, list, arr);
    al_set_size(ctx, list, n.max(0));
    // IMMUTABLE. The JDK answers a `Collections$CopiesList`, which refuses
    // `add`/`set`/`remove`; this returned a plain `ArrayList`, so
    // `nCopies(2, "x").set(0, "y")` succeeded (apps/probes/CollectionsShadowSweep
    // 40-41). `nCopies` is used precisely BECAUSE its result is shared and
    // cannot change -- a mutable one is handed to several holders at once.
    // Wrapped while still pinned: the wrapper allocates.
    let list = ctx.read_native_pin(list_pin, list);
    // The `JdkUnmodifiableViews` row (interpreter round i1 wave 51, lane L5):
    // the JDK's `Collections.unmodifiableList` first, which a real image runs
    // as bytecode there, so `nCopies` hands out no stand-in either; a
    // synthetic image (no such body) falls to the native below.
    if ctx.aligned(cratonvm_types::HotSpotAlignment::JdkUnmodifiableViews) {
        if let Ok(v @ Some(Value::Object(Some(_)))) = ctx.invoke(
            "java/util/Collections",
            "unmodifiableList",
            "(Ljava/util/List;)Ljava/util/List;",
            &[Value::Object(Some(list))],
        ) {
            ctx.unpin_native_roots(if val_handle == usize::MAX {
                list_pin
            } else {
                val_handle
            });
            return Ok(v);
        }
    }
    let list = ctx.read_native_pin(list_pin, list);
    let wrapped = match native_collections_unmodifiable_list(ctx, &[Value::Object(Some(list))]) {
        Ok(v @ Some(Value::Object(Some(_)))) => Ok(v),
        // `cratonvm/internal/UnmodifiableList` is a FABRICATION, and
        // `--real-jdk` refuses it. `Collections.unmodifiableList` is registered
        // as a `SyntheticStub`, so strict mode drops the registration and real
        // bytecode serves the Java call -- but calling the native's BODY here
        // bypasses that drop, which is how a fix for the immutability row
        // introduced a `NoClassDefFoundError` in the strict arm. Ask the real
        // static instead; every real image has it, and it answers a genuine
        // `Collections$UnmodifiableRandomAccessList`.
        _ => {
            let list = ctx.read_native_pin(list_pin, list);
            match ctx.invoke(
                "java/util/Collections",
                "unmodifiableList",
                "(Ljava/util/List;)Ljava/util/List;",
                &[Value::Object(Some(list))],
            ) {
                Ok(v @ Some(Value::Object(Some(_)))) => Ok(v),
                // A synthetic-JDK image has neither. The list stays mutable,
                // which is what it was before this became immutable at all.
                _ => Ok(Some(Value::Object(Some(
                    ctx.read_native_pin(list_pin, list),
                )))),
            }
        }
    };
    ctx.unpin_native_roots(if val_handle == usize::MAX {
        list_pin
    } else {
        val_handle
    });
    wrapped
}

pub(crate) fn native_collections_shuffle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    reject_null_collections_arg(args.first())?;
    let list = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let (data, size) = al_state(ctx, list);
    if let Some(d) = data {
        let n = size as usize;
        // Simple Fisher-Yates using system time as seed
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(42u64, |t| t.as_nanos() as u64);
        let mut rng = seed;
        for i in (1..n).rev() {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let j = (rng >> 33) as usize % (i + 1);
            let a = ctx.get_array_element(d, i);
            let b = ctx.get_array_element(d, j);
            ctx.set_array_element(d, i, b);
            ctx.set_array_element(d, j, a);
        }
    }
    Ok(None)
}

pub(crate) fn native_collections_add_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // BOTH arguments: the varargs array is dereferenced for its length, so
    // `Collections.addAll(c, (Object[]) null)` is an NPE too.
    reject_null_collections_arg(args.first())?;
    reject_null_collections_arg(args.get(1))?;
    let coll = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elements = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // Every `Collection.add` dispatch can execute arbitrary Java code and
    // collect. Root both the destination and the source array before the first
    // callback, then refresh them for every iteration. Keeping the array as a
    // raw entry-argument copy made the second element read dereference
    // from-space during WildFly's parallel subsystem boot.
    let pin_base = ctx.pin_native_root(coll);
    let elements_pin = ctx.pin_native_root(elements);
    let elements = ctx.read_native_pin(elements_pin, elements);
    let len = ctx.array_length(elements);
    let mut modified = false;
    for i in 0..len {
        let elements = ctx.read_native_pin(elements_pin, elements);
        let elem = ctx.get_array_element(elements, i);
        // Dispatch to the target collection's real `add` instead of
        // hard-coding `native_al_add` (the ArrayList `elementData`/`size`
        // layout). `Collections.addAll` accepts any `Collection` — a
        // HashSet, LinkedList, TreeSet, etc. store elements nothing like
        // ArrayList, so `native_al_add` would write the wrong fields and
        // silently drop every element. JVMS/JDK spec is `result |=
        // c.add(element)`; honour that via virtual dispatch.
        let coll = ctx.read_native_pin(pin_base, coll);
        match ctx.invoke_virtual(coll, "add", "(Ljava/lang/Object;)Z", &[elem]) {
            Ok(Some(Value::Int(1))) => modified = true,
            Ok(_) => {}
            Err(error) => {
                ctx.unpin_native_roots(pin_base);
                return Err(error);
            }
        }
    }
    ctx.unpin_native_roots(pin_base);
    Ok(Some(Value::Int(if modified { 1 } else { 0 })))
}

