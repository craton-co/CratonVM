#[allow(unused_imports)]
use super::*;

pub fn register_phase57_nio_file_b(r: &mut NativeMethodRegistry, __path_rows_before: usize, __fs_rows_before: usize, __fsp_rows_before: usize) {
    let path = "java/nio/file/Path";
    let _paths = "java/nio/file/Paths";
    let files = "java/nio/file/Files";
    let fs_class = "java/nio/file/FileSystem";
    let fsp = "java/nio/file/spi/FileSystemProvider";
    r.register(
        "org/keycloak/quarkus/runtime/configuration/MicroProfileConfigProvider$MicroProfileScope",
        "get",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let this_pin = ctx.pin_native_root(this);
            let name_pin = ctx.pin_native_root(name);
            let this_cur = ctx.read_native_pin(this_pin, this);
            let name_cur = ctx.read_native_pin(name_pin, name);
            let result = ctx.invoke_virtual(
                this_cur,
                "get",
                "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
                &[Value::Object(Some(name_cur)), Value::Object(None)],
            );
            ctx.unpin_native_roots(this_pin);
            ctx.unpin_native_roots(name_pin);
            result
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- smallrye-config KeyStoreConfigSourceFactory.getConfigSources ---
    // CratonVM divergence: In real Quarkus, the @ConfigMapping interface
    // io.smallrye.config.source.keystore.KeyStoreConfig is auto-registered with
    // the SmallRyeConfig instance via build-time discovery/reflection.  Under
    // CratonVM that registration path is not active, so the factory's call to
    // SmallRyeConfig.getConfigMapping(KeyStoreConfig.class) throws
    // NoSuchElementException("SRCFG00027: Could not find a mapping for
    // io.smallrye.config.source.keystore.KeyStoreConfig"), aborting Keycloak
    // 26 boot during Configuration.getConfig().  Keycloak's default config
    // does not use keystore-backed config sources, so returning an empty
    // Iterable here is semantically equivalent to having no configured
    // keystores.  Short-circuit at getConfigSources so we never hit the
    // mapping lookup in getKeyStoreConfig.
    #[cfg(any())]
    r.register(
        "io/smallrye/config/source/keystore/KeyStoreConfigSourceFactory",
        "getConfigSources",
        "(Lio/smallrye/config/ConfigSourceContext;)Ljava/lang/Iterable;",
        |ctx, _args| {
            use cratonvm_types::ArrayElementType;
            let arr = ctx.new_array(ArrayElementType::Reference, 0);
            // Pin across the list alloc below — a moving young GC there would
            // relocate the fresh array (native stale-local family).
            let arr_pin = ctx.pin_native_root(arr);
            let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
            let arr = ctx.read_native_pin(arr_pin, arr);
            // By NAME, not by raw slot. These three writes were
            // `set_field(list, 0/1/2, ..)` against the REAL JDK layout
            // (`AbstractList.modCount`, `ArrayList.elementData`,
            // `ArrayList.size`) -- an absolute extent of 3 on a class this
            // code does not own, which is what the T9C gate scores against the
            // synthetic table's 2 and what that gate's own "rule when this
            // fails" says to replace. Resolving by name is layout-independent:
            // correct here, and in synthetic mode a write that finds no such
            // field is dropped rather than landing on the wrong slot.
            ctx.set_field_by_name(list, "modCount", Value::Int(0));
            ctx.set_field_by_name(list, "elementData", Value::Object(Some(arr)));
            ctx.set_field_by_name(list, "size", Value::Int(0));
            ctx.unpin_native_roots(arr_pin);
            Ok(Some(Value::Object(Some(list))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- SmallRyeConfig.getConfigMapping(Class, String) — Keycloak Gap 6 ---
    // Quarkus recorder steps (e.g. VirtualThreads) and `SharedConfig.<clinit>`
    // call `getConfigMapping(<@ConfigMapping iface>.class, prefix)` to get the
    // config-mapping impl. The real method reads the config's `mappings`
    // registry, which CratonVM never populates → the old "Round 87" shim
    // allocated a synthetic of the INTERFACE (no method bodies) → callers like
    // `VirtualThreadsConfig.enabled()` threw `AbstractMethodError`.
    //
    // The native now LAZILY REGISTERS the requested mapping with the live
    // `SmallRyeConfig` via the real `ConfigMappings.registerConfigMappings` and
    // returns the genuine, config-backed `<iface>$$CMImpl`. See
    // `native_smallrye_get_config_mapping` above for the full rationale.
    r.register(
        "io/smallrye/config/SmallRyeConfig",
        "getConfigMapping",
        "(Ljava/lang/Class;Ljava/lang/String;)Ljava/lang/Object;",
        native_smallrye_get_config_mapping, cratonvm_native_api::NativeKind::Bridge,
    );

    // The single-arg form `getConfigMapping(Class)` delegates to the two-arg
    // form on real SmallRyeConfig, but when the JIT/interp doesn't re-enter
    // the bytecode path (e.g. direct invokevirtual without inline cache), we
    // mirror the same logic here so both arms are covered. This used to
    // allocate a bare synthetic of the mapping INTERFACE (no method bodies),
    // which threw AbstractMethodError on the first call (e.g.
    // `TestConfig.classOrderer()` during Quarkus JUnit discovery) — the same
    // bug class the 2-arg form's Gap-6 fix already solved. Derive the real
    // prefix from `@ConfigMapping` and reuse that fixed path.
    r.register(
        "io/smallrye/config/SmallRyeConfig",
        "getConfigMapping",
        "(Ljava/lang/Class;)Ljava/lang/Object;",
        |ctx, args| {
            let class_mirror = match args.get(1) {
                Some(Value::Object(Some(c))) => *c,
                _ => return Ok(Some(Value::Object(None))),
            };
            // Pin across the annotation-prefix lookup below — a moving young
            // GC there would relocate them (native stale-local family).
            let this_val = args[0];
            let this_pin = pinned_object_value(ctx, this_val);
            let mirror_pin = ctx.pin_native_root(class_mirror);
            let prefix = config_mapping_prefix(ctx, class_mirror);
            let this_val = read_pinned_object_value(ctx, this_pin, this_val);
            let class_mirror = ctx.read_native_pin(mirror_pin, class_mirror);
            if let Some((h, _)) = this_pin {
                ctx.unpin_native_roots(h);
            } else {
                ctx.unpin_native_roots(mirror_pin);
            }
            native_smallrye_get_config_mapping(
                ctx,
                &[this_val, Value::Object(Some(class_mirror)), prefix],
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- Quarkus LoggingSetupRecorder.handleFailedStart ---
    // Keycloak's test framework registers a SmallRye config that already has
    // Quarkus' Charset/MemorySize converters. The recorder builds a transient
    // logging config from it; under CratonVM that transient builder needs the
    // discovered Quarkus converters made explicit before mapping validation.
    r.register(
        "io/quarkus/runtime/logging/LoggingSetupRecorder",
        "handleFailedStart",
        "()V",
        |ctx, _args| native_quarkus_logging_handle_failed_start(ctx, None), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "io/quarkus/runtime/logging/LoggingSetupRecorder",
        "handleFailedStart",
        "(Lio/quarkus/runtime/RuntimeValue;)V",
        |ctx, args| {
            let supplier = match args.first() {
                Some(Value::Object(Some(o))) => Some(*o),
                _ => None,
            };
            native_quarkus_logging_handle_failed_start(ctx, supplier)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- picocli CommandLine$Help$Ansi$Text.concat(Text) ---
    // Keycloak's PicocliTest builds large help/usage strings through picocli's
    // immutable-ish `Ansi.Text` API. The Java `concat(Text)` path clones the
    // receiver, slices two `StringBuilder`s, allocates a fresh `ArrayList`, and
    // re-materializes every `StyledSection` through a tiny constructor. Under
    // CratonVM this hot path dominated the class enough to look like a hang;
    // HotSpot JIT finishes the same class quickly. Keep the optimization
    // app-scoped: preserve Picocli's object shape, but perform the clone/slice
    // and section reindexing in one native pass.
    #[derive(Clone)]
    struct PicocliStyledSectionData {
        start_index: i32,
        length: i32,
        start_styles: String,
        end_styles: String,
    }

    fn picocli_i32_field(ctx: &dyn NativeContext, obj: ObjectRef, name: &str) -> i32 {
        match ctx.get_field_by_name(obj, name) {
            Value::Int(v) => v,
            _ => 0,
        }
    }

    fn picocli_obj_field(ctx: &dyn NativeContext, obj: ObjectRef, name: &str) -> Option<ObjectRef> {
        match ctx.get_field_by_name(obj, name) {
            Value::Object(Some(o)) => Some(o),
            _ => None,
        }
    }

    fn picocli_string_field(ctx: &dyn NativeContext, obj: ObjectRef, name: &str) -> String {
        picocli_obj_field(ctx, obj, name)
            .and_then(|s| ctx.read_string(s))
            .unwrap_or_default()
    }

    fn picocli_utf16_len(s: &str) -> i32 {
        s.encode_utf16().count().min(i32::MAX as usize) as i32
    }

    fn picocli_utf16_slice(s: &str, from: i32, len: i32) -> String {
        let units: Vec<u16> = s.encode_utf16().collect();
        let total = units.len();
        let start = (from.max(0) as usize).min(total);
        let wanted = len.max(0) as usize;
        let end = start.saturating_add(wanted).min(total);
        String::from_utf16_lossy(&units[start..end])
    }

    fn picocli_list_size(ctx: &mut dyn NativeContext, list: ObjectRef) -> usize {
        match ctx.get_field_by_name(list, "size") {
            Value::Int(n) if n > 0 => n as usize,
            Value::Int(_) => 0,
            _ => match ctx.invoke_virtual(list, "size", "()I", &[]) {
                Ok(Some(Value::Int(n))) if n > 0 => n as usize,
                _ => 0,
            },
        }
    }

    fn picocli_list_get(
        ctx: &mut dyn NativeContext,
        list: ObjectRef,
        index: usize,
    ) -> Option<ObjectRef> {
        if let Value::Object(Some(data)) = ctx.get_field_by_name(list, "elementData") {
            if index < ctx.array_length(data) {
                if let Value::Object(Some(o)) = ctx.get_array_element(data, index) {
                    return Some(o);
                }
            }
        }
        match ctx.invoke_virtual(
            list,
            "get",
            "(I)Ljava/lang/Object;",
            &[Value::Int(index.min(i32::MAX as usize) as i32)],
        ) {
            Ok(Some(Value::Object(Some(o)))) => Some(o),
            _ => None,
        }
    }

    fn picocli_collect_sections(
        ctx: &mut dyn NativeContext,
        list: Option<ObjectRef>,
        delta: i32,
    ) -> Vec<PicocliStyledSectionData> {
        let Some(list) = list else {
            return Vec::new();
        };
        // gc-common w18-f: `size` / `get` run Java for a list that is not an
        // `ArrayList`, so the list is rooted and re-read for every element
        // (it was held raw across the whole loop). Each `section` is read
        // completely before the next call.
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let list_h = scope.root(list);
        let count = picocli_list_size(&mut *scope, list).min(100_000);
        let mut out = Vec::with_capacity(count);
        for idx in 0..count {
            let list_now = scope.get(&list_h);
            let Some(section) = picocli_list_get(&mut *scope, list_now, idx) else {
                continue;
            };
            out.push(PicocliStyledSectionData {
                start_index: picocli_i32_field(&*scope, section, "startIndex")
                    .saturating_add(delta),
                length: picocli_i32_field(&*scope, section, "length"),
                start_styles: picocli_string_field(&*scope, section, "startStyles"),
                end_styles: picocli_string_field(&*scope, section, "endStyles"),
            });
        }
        out
    }

    fn picocli_text_plain_slice(ctx: &mut dyn NativeContext, text: ObjectRef) -> String {
        // gc-common w18-f: the offsets are read BEFORE the `toString` call
        // (Java), which may move `text`; they used to be read after it, off a
        // stale `text`.
        let from = picocli_i32_field(ctx, text, "from");
        let len = picocli_i32_field(ctx, text, "length");
        let full = match picocli_obj_field(ctx, text, "plain") {
            Some(plain) => crate::lang_string::invoke_to_string(ctx, plain).unwrap_or_default(),
            None => String::new(),
        };
        picocli_utf16_slice(&full, from, len)
    }

    fn picocli_new_styled_section_value(
        ctx: &mut dyn NativeContext,
        start_index: i32,
        length: i32,
        start_styles: &str,
        end_styles: &str,
    ) -> MethodCallResult {
        let obj = match ctx.new_object("picocli/CommandLine$Help$Ansi$StyledSection")? {
            Some(Value::Object(Some(o))) => o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let pin = ctx.pin_native_root(obj);
        let cur = ctx.read_native_pin(pin, obj);
        ctx.set_field_by_name(cur, "startIndex", Value::Int(start_index));
        ctx.set_field_by_name(cur, "length", Value::Int(length));

        let start_obj = ctx.create_string(start_styles);
        let cur = ctx.read_native_pin(pin, obj);
        ctx.set_field_by_name(cur, "startStyles", Value::Object(Some(start_obj)));

        let end_obj = ctx.create_string(end_styles);
        let cur = ctx.read_native_pin(pin, obj);
        ctx.set_field_by_name(cur, "endStyles", Value::Object(Some(end_obj)));
        let cur = ctx.read_native_pin(pin, obj);
        ctx.unpin_native_roots(pin);
        Ok(Some(Value::Object(Some(cur))))
    }

    fn native_picocli_styled_section_init(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        let start = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let len = match args.get(2) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        ctx.set_field_by_name(this, "startIndex", Value::Int(start));
        ctx.set_field_by_name(this, "length", Value::Int(len));
        ctx.set_field_by_name(
            this,
            "startStyles",
            args.get(3).copied().unwrap_or(Value::Object(None)),
        );
        ctx.set_field_by_name(
            this,
            "endStyles",
            args.get(4).copied().unwrap_or(Value::Object(None)),
        );
        Ok(None)
    }

    fn native_picocli_styled_section_with_start_index(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let new_start = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let len = picocli_i32_field(ctx, this, "length");
        let start_styles = picocli_string_field(ctx, this, "startStyles");
        let end_styles = picocli_string_field(ctx, this, "endStyles");
        picocli_new_styled_section_value(ctx, new_start, len, &start_styles, &end_styles)
    }

    fn native_picocli_text_concat_text(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let other = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(Some(this)))),
        };

        // gc-common w18-f: one handle scope for the whole native. Every object
        // held across a call that can run Java (`toString`, the list's
        // `size`/`get`/`add`, the `StringBuilder` / `ArrayList` / `Text`
        // constructors) is rooted and re-read after it. Before, `other` and
        // `this` were pinned but read raw after the first `toString`, the
        // two `sections` lists were held raw across each other's walk, and the
        // fresh `StringBuilder` was held raw across the `ArrayList`'s
        // constructor.
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let this_h = scope.root(this);
        let other_h = scope.root(other);

        let cur = scope.get(&this_h);
        let left = picocli_text_plain_slice(&mut *scope, cur);
        let cur = scope.get(&other_h);
        let right = picocli_text_plain_slice(&mut *scope, cur);
        let left_len = picocli_utf16_len(&left);
        let (this, other) = (scope.get(&this_h), scope.get(&other_h));
        let self_from = picocli_i32_field(&*scope, this, "from");
        let other_from = picocli_i32_field(&*scope, other, "from");
        let this_sections = picocli_obj_field(&*scope, this, "sections");
        let other_sections_h = picocli_obj_field(&*scope, other, "sections").map(|s| scope.root(s));
        let mut sections =
            picocli_collect_sections(&mut *scope, this_sections, self_from.saturating_neg());
        let other_sections = other_sections_h.as_ref().map(|h| scope.get(h));
        sections.extend(picocli_collect_sections(
            &mut *scope,
            other_sections,
            left_len.saturating_sub(other_from),
        ));

        let combined = format!("{left}{right}");
        let combined_len = picocli_utf16_len(&combined);
        let combined_string = scope.create_string(&combined);
        let plain = match scope.new_object_initialized(
            "java/lang/StringBuilder",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(combined_string))],
        )? {
            Some(Value::Object(Some(o))) => o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let plain_h = scope.root(plain);

        let section_list = match scope.new_object_initialized("java/util/ArrayList", "()V", &[])? {
            Some(Value::Object(Some(o))) => o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let list_h = scope.root(section_list);
        for section in sections {
            let section_val = picocli_new_styled_section_value(
                &mut *scope,
                section.start_index,
                section.length,
                &section.start_styles,
                &section.end_styles,
            )?;
            if let Some(Value::Object(Some(section_obj))) = section_val {
                let list_now = scope.get(&list_h);
                let _ = scope.invoke_virtual(
                    list_now,
                    "add",
                    "(Ljava/lang/Object;)Z",
                    &[Value::Object(Some(section_obj))],
                );
            }
        }

        let result = match scope.new_object("picocli/CommandLine$Help$Ansi$Text")? {
            Some(Value::Object(Some(o))) => o,
            _ => return Ok(Some(Value::Object(None))),
        };
        // Nothing below allocates.
        let plain = scope.get(&plain_h);
        let section_list = scope.get(&list_h);
        let this = scope.get(&this_h);
        let outer = scope.get_field_by_name(this, "this$0");
        let max_length = scope.get_field_by_name(this, "maxLength");
        let color_scheme = scope.get_field_by_name(this, "colorScheme");
        scope.set_field_by_name(result, "this$0", outer);
        scope.set_field_by_name(result, "maxLength", max_length);
        scope.set_field_by_name(result, "from", Value::Int(0));
        scope.set_field_by_name(result, "length", Value::Int(combined_len));
        scope.set_field_by_name(result, "plain", Value::Object(Some(plain)));
        scope.set_field_by_name(result, "sections", Value::Object(Some(section_list)));
        scope.set_field_by_name(result, "colorScheme", color_scheme);
        Ok(Some(Value::Object(Some(result))))
    }

    fn picocli_is_code_point_cjk(cp: i32) -> bool {
        cp == 0x00B1
            || (0x3040..=0x309F).contains(&cp)
            || (0x30A0..=0x30FF).contains(&cp)
            || (0x31F0..=0x31FF).contains(&cp)
            || (0x3130..=0x318F).contains(&cp)
            || (0x1100..=0x11FF).contains(&cp)
            || (0xAC00..=0xD7AF).contains(&cp)
            || (0x4E00..=0x9FFF).contains(&cp)
            || (0x3400..=0x4DBF).contains(&cp)
            || (0x20000..=0x2A6DF).contains(&cp)
            || (0xFE30..=0xFE4F).contains(&cp)
            || (0xF900..=0xFAFF).contains(&cp)
            || (0x2E80..=0x2EFF).contains(&cp)
            || (0x3000..=0x303F).contains(&cp)
            || (0x3200..=0x32FF).contains(&cp)
            || (0xFF00..=0xFF60).contains(&cp)
    }

    fn picocli_cjk_adjusted_len(s: &str) -> i32 {
        let mut width: i32 = 0;
        for ch in s.chars() {
            width = width.saturating_add(if picocli_is_code_point_cjk(ch as i32) {
                2
            } else {
                1
            });
        }
        width
    }

    fn picocli_text_plain_slice_with_len(
        ctx: &mut dyn NativeContext,
        text: ObjectRef,
        len: i32,
    ) -> String {
        // gc-common w18-f: `from` is read BEFORE the `toString` call (Java),
        // which may move `text`; it used to be read after, off a stale `text`.
        let from = picocli_i32_field(ctx, text, "from");
        let full = match picocli_obj_field(ctx, text, "plain") {
            Some(plain) => crate::lang_string::invoke_to_string(ctx, plain).unwrap_or_default(),
            None => String::new(),
        };
        picocli_utf16_slice(&full, from, len)
    }

    fn native_picocli_is_code_point_cjk(
        _ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let cp = match args.first() {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        Ok(Some(Value::Int(if picocli_is_code_point_cjk(cp) {
            1
        } else {
            0
        })))
    }

    fn native_picocli_text_get_cjk_adjusted_length(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let len = match args.get(2) {
            Some(Value::Int(v)) => *v,
            _ => picocli_i32_field(ctx, this, "length"),
        };
        let text = picocli_text_plain_slice_with_len(ctx, this, len);
        Ok(Some(Value::Int(picocli_cjk_adjusted_len(&text))))
    }

    r.register(
        "picocli/CommandLine$Help$Ansi$StyledSection",
        "<init>",
        "(IILjava/lang/String;Ljava/lang/String;)V",
        native_picocli_styled_section_init, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "picocli/CommandLine$Help$Ansi$StyledSection",
        "withStartIndex",
        "(I)Lpicocli/CommandLine$Help$Ansi$StyledSection;",
        native_picocli_styled_section_with_start_index, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "picocli/CommandLine$Help$Ansi$Text",
        "concat",
        "(Lpicocli/CommandLine$Help$Ansi$Text;)Lpicocli/CommandLine$Help$Ansi$Text;",
        native_picocli_text_concat_text, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        "picocli/CommandLine$Model$UsageMessageSpec",
        "isCodePointCJK",
        "(I)Z",
        native_picocli_is_code_point_cjk, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "picocli/CommandLine$Help$Ansi$Text",
        "getCJKAdjustedLength",
        "()I",
        native_picocli_text_get_cjk_adjusted_length, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "picocli/CommandLine$Help$Ansi$Text",
        "getCJKAdjustedLength",
        "(II)I",
        native_picocli_text_get_cjk_adjusted_length, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- Keycloak PropertyMappers$WildcardMappersConfig.get(String) ---
    // `PicocliTest` repeatedly walks SmallRye config property names while
    // sanitizing command mappers. The Java implementation routes every kc.* /
    // quarkus.* key through `wildcardMappers.stream().filter(...).toList()`;
    // under CratonVM that stream/lambda path can spin at the predicate. Mirror
    // the simple Keycloak logic directly over the backing set and keep returning
    // the original mapper objects.
    fn keycloak_wildcard_value_valid(value: &str) -> bool {
        !value.is_empty()
            && value.chars().all(|ch| {
                ch.is_ascii_alphanumeric() || matches!(ch, '[' | ']' | '$' | '-' | '.' | '_')
            })
    }

    fn keycloak_wildcard_mapper_matches(
        ctx: &dyn NativeContext,
        mapper: ObjectRef,
        key: &str,
    ) -> bool {
        let from_prefix = picocli_string_field(ctx, mapper, "fromPrefix");
        if !from_prefix.is_empty() && key.starts_with(&from_prefix) {
            return keycloak_wildcard_value_valid(&key[from_prefix.len()..]);
        }

        let to_prefix = picocli_string_field(ctx, mapper, "toPrefix");
        let to_suffix = picocli_string_field(ctx, mapper, "toSuffix");
        if to_prefix.is_empty()
            || !key.starts_with(&to_prefix)
            || !key.ends_with(&to_suffix)
            || key.len() < to_prefix.len().saturating_add(to_suffix.len())
        {
            return false;
        }
        let end = key.len().saturating_sub(to_suffix.len());
        keycloak_wildcard_value_valid(&key[to_prefix.len()..end])
    }

    fn keycloak_empty_array_list(ctx: &mut dyn NativeContext) -> MethodCallResult {
        match ctx.new_object_initialized("java/util/ArrayList", "()V", &[])? {
            Some(Value::Object(Some(list))) => Ok(Some(Value::Object(Some(list)))),
            _ => Ok(Some(Value::Object(None))),
        }
    }

    fn native_keycloak_wildcard_mappers_get(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return keycloak_empty_array_list(ctx),
        };
        let key = match args.get(1) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };

        // Pin across the ArrayList ctor / iterator invokes below — a moving
        // young GC there would relocate `this`/`list` (native stale-local
        // family).
        let this_pin = ctx.pin_native_root(this);
        let list = match ctx.new_object_initialized("java/util/ArrayList", "()V", &[])? {
            Some(Value::Object(Some(list))) => list,
            _ => {
                ctx.unpin_native_roots(this_pin);
                return Ok(Some(Value::Object(None)));
            }
        };
        let list_pin = ctx.pin_native_root(list);
        let this = ctx.read_native_pin(this_pin, this);
        if !(key.starts_with("kc.") || key.starts_with("quarkus.")) {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Object(Some(list))));
        }

        let Some(set) = picocli_obj_field(ctx, this, "wildcardMappers") else {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Object(Some(list))));
        };
        let iterator = match ctx.invoke_virtual(set, "iterator", "()Ljava/util/Iterator;", &[])? {
            Some(Value::Object(Some(iterator))) => iterator,
            _ => {
                let list = ctx.read_native_pin(list_pin, list);
                ctx.unpin_native_roots(this_pin);
                return Ok(Some(Value::Object(Some(list))));
            }
        };

        let iterator_pin = ctx.pin_native_root(iterator);
        for _ in 0..10_000 {
            let iterator = ctx.read_native_pin(iterator_pin, iterator);
            let has_next = match ctx.invoke_virtual(iterator, "hasNext", "()Z", &[])? {
                Some(Value::Int(v)) => v != 0,
                _ => false,
            };
            if !has_next {
                break;
            }

            let iterator = ctx.read_native_pin(iterator_pin, iterator);
            let mapper = match ctx.invoke_virtual(iterator, "next", "()Ljava/lang/Object;", &[])? {
                Some(Value::Object(Some(mapper))) => mapper,
                _ => continue,
            };
            if keycloak_wildcard_mapper_matches(ctx, mapper, &key) {
                let mapper_pin = ctx.pin_native_root(mapper);
                let mapper = ctx.read_native_pin(mapper_pin, mapper);
                let list = ctx.read_native_pin(list_pin, list);
                let _ = ctx.invoke_virtual(
                    list,
                    "add",
                    "(Ljava/lang/Object;)Z",
                    &[Value::Object(Some(mapper))],
                )?;
                ctx.unpin_native_roots(mapper_pin);
            }
        }

        let list = ctx.read_native_pin(list_pin, list);
        ctx.unpin_native_roots(this_pin);
        Ok(Some(Value::Object(Some(list))))
    }

    fn keycloak_optional_string(
        ctx: &mut dyn NativeContext,
        value: Option<String>,
    ) -> Result<Value, MethodCallFailed> {
        let optional = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
        // Pin across the create_string below — a moving young GC there would
        // relocate the fresh Optional (native stale-local family).
        let optional_pin = ctx.pin_native_root(optional);
        let optional_value = value
            .map(|s| Value::Object(Some(ctx.create_string(&s))))
            .unwrap_or(Value::Object(None));
        let optional = ctx.read_native_pin(optional_pin, optional);
        ctx.set_field(optional, 0, optional_value);
        ctx.unpin_native_roots(optional_pin);
        Ok(Value::Object(Some(optional)))
    }

    fn keycloak_is_not_blank(value: &str) -> bool {
        value.chars().any(|ch| !ch.is_whitespace())
    }

    fn keycloak_prefixed_optional_field(
        ctx: &mut dyn NativeContext,
        obj: ObjectRef,
        field_name: &str,
        prefix: &str,
    ) -> MethodCallResult {
        let value = match ctx.get_field_by_name(obj, field_name) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let result = if keycloak_is_not_blank(&value) {
            Some(format!("{prefix}{value}"))
        } else {
            None
        };
        Ok(Some(keycloak_optional_string(ctx, result)?))
    }

    fn native_keycloak_property_mapper_get_enabled_when(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(keycloak_optional_string(ctx, None)?)),
        };
        keycloak_prefixed_optional_field(ctx, this, "enabledWhen", "Available only when ")
    }

    fn native_keycloak_property_mapper_get_required_when(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(keycloak_optional_string(ctx, None)?)),
        };
        keycloak_prefixed_optional_field(ctx, this, "requiredWhen", "Required when ")
    }

    fn native_keycloak_wildcard_is_valid_value(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let value = match args.first() {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        Ok(Some(Value::Int(if keycloak_wildcard_value_valid(&value) {
            1
        } else {
            0
        })))
    }

    fn native_keycloak_wildcard_extract_value(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(keycloak_optional_string(ctx, None)?)),
        };
        let key = match args.get(1) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        let from_prefix = picocli_string_field(ctx, this, "fromPrefix");
        let candidate = if !from_prefix.is_empty() && key.starts_with(&from_prefix) {
            Some(key[from_prefix.len()..].to_string())
        } else {
            let to_prefix = picocli_string_field(ctx, this, "toPrefix");
            let to_suffix = picocli_string_field(ctx, this, "toSuffix");
            if !to_prefix.is_empty()
                && key.starts_with(&to_prefix)
                && key.ends_with(&to_suffix)
                && key.len() >= to_prefix.len().saturating_add(to_suffix.len())
            {
                let end = key.len().saturating_sub(to_suffix.len());
                Some(key[to_prefix.len()..end].to_string())
            } else {
                None
            }
        }
        .filter(|s| keycloak_wildcard_value_valid(s));
        Ok(Some(keycloak_optional_string(ctx, candidate)?))
    }

    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/PropertyMappers$WildcardMappersConfig",
        "get",
        "(Ljava/lang/String;)Ljava/util/List;",
        native_keycloak_wildcard_mappers_get, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/WildcardPropertyMapper",
        "isValidWildcardValue",
        "(Ljava/lang/String;)Z",
        native_keycloak_wildcard_is_valid_value, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/WildcardPropertyMapper",
        "extractWildcardValue",
        "(Ljava/lang/String;)Ljava/util/Optional;",
        native_keycloak_wildcard_extract_value, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- SmallRyeConfigSources.getValue / MapBackedConfigValueConfigSource.getConfigValue ---
    // These methods are tiny dispatch loops in SmallRye config, but Picocli's
    // validation path calls them enough that CratonVM can spend minutes in the
    // interpreted interface chain. Keep the semantics intact: walk the exact
    // `configSources` list, return the first non-null ConfigValue, then delegate
    // to the interceptor context.
    fn native_smallrye_map_backed_get_config_value(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let key = args.get(1).copied().unwrap_or(Value::Object(None));
        let Some(properties) = picocli_obj_field(ctx, this, "properties") else {
            return Ok(Some(Value::Object(None)));
        };
        ctx.invoke_virtual(
            properties,
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[key],
        )
    }

    fn native_smallrye_config_sources_get_value(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let context = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let key = args.get(2).copied().unwrap_or(Value::Object(None));
        let Some(config_sources) = picocli_obj_field(ctx, this, "configSources") else {
            return ctx.invoke_virtual(
                context,
                "proceed",
                "(Ljava/lang/String;)Lio/smallrye/config/ConfigValue;",
                &[key],
            );
        };

        // gc-common w18-f: the interceptor context, the key String and the
        // source list are rooted across the Java calls below (`size`/`get`
        // when the list is not an `ArrayList`, every `getConfigValue`), and
        // re-read before each use. All three were held raw across the whole
        // loop, so a collection in the first source's lookup handed the next
        // source, and the final `proceed`, vacated addresses.
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let context_h = scope.root(context);
        let sources_h = scope.root(config_sources);
        let key_h = match key {
            Value::Object(Some(k)) => Some(scope.root(k)),
            _ => None,
        };

        let sources = scope.get(&sources_h);
        let count = picocli_list_size(&mut *scope, sources).min(100_000);
        for idx in 0..count {
            let sources = scope.get(&sources_h);
            let Some(source) = picocli_list_get(&mut *scope, sources, idx) else {
                continue;
            };
            let k = match &key_h {
                Some(h) => Value::Object(Some(scope.get(h))),
                None => key,
            };
            let value = scope.invoke_virtual(
                source,
                "getConfigValue",
                "(Ljava/lang/String;)Lio/smallrye/config/ConfigValue;",
                &[k],
            )?;
            if matches!(value, Some(Value::Object(Some(_)))) {
                return Ok(value);
            }
        }

        let context = scope.get(&context_h);
        let k = match &key_h {
            Some(h) => Value::Object(Some(scope.get(h))),
            None => key,
        };
        scope.invoke_virtual(
            context,
            "proceed",
            "(Ljava/lang/String;)Lio/smallrye/config/ConfigValue;",
            &[k],
        )
    }

    r.register(
        "io/smallrye/config/MapBackedConfigValueConfigSource",
        "getConfigValue",
        "(Ljava/lang/String;)Lio/smallrye/config/ConfigValue;",
        native_smallrye_map_backed_get_config_value, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "io/smallrye/config/SmallRyeConfigSources",
        "getValue",
        "(Lio/smallrye/config/ConfigSourceInterceptorContext;Ljava/lang/String;)Lio/smallrye/config/ConfigValue;",
        native_smallrye_config_sources_get_value, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- SmallRye simple property helpers ---
    // These preserve SmallRye's object shape while avoiding repeated Java
    // interpreter trips through trivial constructors/accessors on Picocli's
    // validation path. The relaxed-name matching bytecode still runs normally.
    fn smallrye_property_name_hash(name: &str) -> i32 {
        let mut hash = 0i32;
        let mut in_quote = false;
        for ch in name.encode_utf16() {
            if in_quote {
                if ch == b'"' as u16 {
                    in_quote = false;
                }
            } else if ch == b'"' as u16 {
                in_quote = true;
            } else if ch == b'[' as u16 || ch == b']' as u16 {
                hash = hash.wrapping_mul(31).wrapping_add(ch as i32);
            }
        }
        hash
    }

    fn native_smallrye_property_name_init(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        let name_value = args.get(1).copied().unwrap_or(Value::Object(None));
        let hash = match name_value {
            Value::Object(Some(s)) => ctx
                .read_string(s)
                .map(|name| smallrye_property_name_hash(&name))
                .unwrap_or(0),
            _ => 0,
        };
        ctx.set_field_by_name(this, "name", name_value);
        ctx.set_field_by_name(this, "hashCode", Value::Int(hash));
        Ok(None)
    }

    fn native_smallrye_property_name_hash_code(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field_by_name(this, "hashCode")))
    }

    fn native_smallrye_property_name_string_field(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field_by_name(this, "name")))
    }

    fn native_smallrye_sysprop_get_system_property(
        ctx: &mut dyn NativeContext,
        args: &[Value],
        key_index: usize,
    ) -> MethodCallResult {
        let key = match args.get(key_index) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => return Ok(Some(Value::Object(None))),
        };
        Ok(Some(match ctx.get_system_property(&key) {
            Some(value) => Value::Object(Some(ctx.create_string(&value))),
            None => Value::Object(None),
        }))
    }

    r.register(
        "io/smallrye/config/PropertyName",
        "<init>",
        "(Ljava/lang/String;)V",
        native_smallrye_property_name_init, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "io/smallrye/config/PropertyName",
        "hashCode",
        "()I",
        native_smallrye_property_name_hash_code, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "io/smallrye/config/PropertyName",
        "getName",
        "()Ljava/lang/String;",
        native_smallrye_property_name_string_field, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "io/smallrye/config/PropertyName",
        "toString",
        "()Ljava/lang/String;",
        native_smallrye_property_name_string_field, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "io/smallrye/config/SysPropConfigSource",
        "getValue",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| native_smallrye_sysprop_get_system_property(ctx, args, 1), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "io/smallrye/config/SysPropConfigSource",
        "getSystemProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| native_smallrye_sysprop_get_system_property(ctx, args, 0), cratonvm_native_api::NativeKind::Bridge,
    );

    fn native_keycloak_transform_datasource_to(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let input_obj: ObjectRef = match args.first() {
            Some(Value::Object(Some(s))) => *s,
            _ => return Ok(Some(Value::Object(None))),
        };
        let input = ctx.read_string(input_obj).unwrap_or_default();
        if input.trim().is_empty() {
            return Ok(Some(Value::Object(None)));
        }
        let output = if let Some(rest) = input.strip_prefix("quarkus.datasource.") {
            format!("quarkus.datasource.\"<datasource>\".{rest}")
        } else if input.starts_with("kc.db-") {
            format!("{input}-<datasource>")
        } else {
            input
        };
        Ok(Some(Value::Object(Some(ctx.create_string(&output)))))
    }

    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/DatabasePropertyMappers$Datasources",
        "transformDatasourceTo",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_keycloak_transform_datasource_to, cratonvm_native_api::NativeKind::Bridge,
    );

    fn native_keycloak_property_mapping_has_inferred_value(
        _ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(Value::Int(0)))
    }

    fn native_keycloak_false_boolean(
        _ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(Value::Int(0)))
    }

    fn keycloak_cli_args_property(ctx: &mut dyn NativeContext) -> String {
        let key = ctx.create_string("kc.config.args");
        match ctx.invoke(
            "java/lang/System",
            "getProperty",
            "(Ljava/lang/String;)Ljava/lang/String;",
            &[Value::Object(Some(key))],
        ) {
            Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        }
    }

    fn keycloak_cli_args(ctx: &mut dyn NativeContext) -> Vec<String> {
        let raw = keycloak_cli_args_property(ctx);
        if raw.is_empty() {
            return Vec::new();
        }
        let mut result = Vec::new();
        let mut escaped = false;
        let mut arg = String::new();
        for ch in raw.chars() {
            if ch == ',' {
                if escaped {
                    arg.push(ch);
                }
                escaped = !escaped;
            } else if ch == ' ' {
                if escaped {
                    result.push(std::mem::take(&mut arg));
                    escaped = false;
                } else {
                    arg.push(ch);
                }
            } else {
                arg.push(ch);
            }
        }
        if !arg.is_empty() || !result.is_empty() {
            result.push(arg);
        }
        result
    }

    fn keycloak_cli_value(ctx: &mut dyn NativeContext, key: &str) -> Option<String> {
        let prefix = format!("--{}=", key);
        keycloak_cli_args(ctx)
            .into_iter()
            .rev()
            .find_map(|arg| arg.strip_prefix(&prefix).map(str::to_string))
    }

    fn keycloak_cli_bool(ctx: &mut dyn NativeContext, key: &str) -> Option<bool> {
        keycloak_cli_value(ctx, key).map(|v| v.eq_ignore_ascii_case("true"))
    }

    fn keycloak_log_handler_enabled(ctx: &mut dyn NativeContext, handler: &str) -> bool {
        let handlers = keycloak_cli_value(ctx, "log").unwrap_or_else(|| "console".to_string());
        handlers
            .split(',')
            .any(|h| h.trim().eq_ignore_ascii_case(handler))
    }

    fn keycloak_log_async_enabled(
        ctx: &mut dyn NativeContext,
        handler: &str,
        handler_key: &str,
    ) -> bool {
        keycloak_log_handler_enabled(ctx, handler)
            && keycloak_cli_bool(ctx, handler_key)
                .or_else(|| keycloak_cli_bool(ctx, "log-async"))
                .unwrap_or(false)
    }

    fn keycloak_log_output_json(
        ctx: &mut dyn NativeContext,
        handler: &str,
        output_key: &str,
    ) -> bool {
        keycloak_log_handler_enabled(ctx, handler)
            && keycloak_cli_value(ctx, output_key)
                .map(|v| v.eq_ignore_ascii_case("json"))
                .unwrap_or(false)
    }

    fn native_keycloak_log_console_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(Value::Int(
            if keycloak_log_handler_enabled(ctx, "console") {
                1
            } else {
                0
            },
        )))
    }

    fn native_keycloak_log_file_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(Value::Int(
            if keycloak_log_handler_enabled(ctx, "file") {
                1
            } else {
                0
            },
        )))
    }

    fn native_keycloak_log_syslog_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(Value::Int(
            if keycloak_log_handler_enabled(ctx, "syslog") {
                1
            } else {
                0
            },
        )))
    }

    fn native_keycloak_log_console_async_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(Value::Int(
            if keycloak_log_async_enabled(ctx, "console", "log-console-async") {
                1
            } else {
                0
            },
        )))
    }

    fn native_keycloak_log_file_async_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(Value::Int(
            if keycloak_log_async_enabled(ctx, "file", "log-file-async") {
                1
            } else {
                0
            },
        )))
    }

    fn native_keycloak_log_syslog_async_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(Value::Int(
            if keycloak_log_async_enabled(ctx, "syslog", "log-syslog-async") {
                1
            } else {
                0
            },
        )))
    }

    fn native_keycloak_log_console_json_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(Value::Int(
            if keycloak_log_output_json(ctx, "console", "log-console-output") {
                1
            } else {
                0
            },
        )))
    }

    fn native_keycloak_log_file_json_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(Value::Int(
            if keycloak_log_output_json(ctx, "file", "log-file-output") {
                1
            } else {
                0
            },
        )))
    }

    fn native_keycloak_log_syslog_json_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(Value::Int(
            if keycloak_log_output_json(ctx, "syslog", "log-syslog-output") {
                1
            } else {
                0
            },
        )))
    }

    fn native_keycloak_log_file_rotation_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(Value::Int(
            if keycloak_log_handler_enabled(ctx, "file")
                && keycloak_cli_bool(ctx, "log-file-rotation-enabled").unwrap_or(false)
            {
                1
            } else {
                0
            },
        )))
    }

    fn native_keycloak_log_mdc_active(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(Value::Int(
            if keycloak_cli_bool(ctx, "log-mdc-enabled").unwrap_or(false) {
                1
            } else {
                0
            },
        )))
    }

    fn keycloak_boolean_result(enabled: bool) -> Value {
        Value::Int(if enabled { 1 } else { 0 })
    }

    fn keycloak_metrics_enabled(ctx: &mut dyn NativeContext) -> bool {
        keycloak_cli_bool(ctx, "metrics-enabled").unwrap_or(false)
    }

    fn keycloak_tracing_enabled(ctx: &mut dyn NativeContext) -> bool {
        keycloak_cli_bool(ctx, "tracing-enabled").unwrap_or(false)
    }

    fn keycloak_cache_set_to_infinispan(ctx: &mut dyn NativeContext) -> bool {
        if keycloak_cli_value(ctx, "cache-remote-host").is_some() {
            return false;
        }
        keycloak_cli_value(ctx, "cache")
            .map(|v| v.eq_ignore_ascii_case("ispn"))
            .unwrap_or(true)
    }

    fn keycloak_telemetry_logs_enabled(ctx: &mut dyn NativeContext) -> bool {
        keycloak_cli_bool(ctx, "telemetry-logs-enabled").unwrap_or(false)
    }

    fn keycloak_telemetry_metrics_enabled(ctx: &mut dyn NativeContext) -> bool {
        keycloak_metrics_enabled(ctx)
            && keycloak_cli_bool(ctx, "telemetry-metrics-enabled").unwrap_or(false)
    }

    fn keycloak_telemetry_enabled(ctx: &mut dyn NativeContext) -> bool {
        keycloak_telemetry_logs_enabled(ctx)
            || keycloak_telemetry_metrics_enabled(ctx)
            || keycloak_tracing_enabled(ctx)
    }

    fn native_keycloak_metrics_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(keycloak_boolean_result(keycloak_metrics_enabled(ctx))))
    }

    fn native_keycloak_cache_set_to_infinispan(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(keycloak_boolean_result(
            keycloak_cache_set_to_infinispan(ctx),
        )))
    }

    fn native_keycloak_tracing_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(keycloak_boolean_result(keycloak_tracing_enabled(ctx))))
    }

    fn native_keycloak_tracing_infinispan_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(keycloak_boolean_result(
            keycloak_tracing_enabled(ctx) && keycloak_cache_set_to_infinispan(ctx),
        )))
    }

    fn native_keycloak_telemetry_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(keycloak_boolean_result(keycloak_telemetry_enabled(
            ctx,
        ))))
    }

    fn native_keycloak_telemetry_logs_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(keycloak_boolean_result(
            keycloak_telemetry_logs_enabled(ctx),
        )))
    }

    fn native_keycloak_telemetry_metrics_enabled(
        ctx: &mut dyn NativeContext,
        _args: &[Value],
    ) -> MethodCallResult {
        Ok(Some(keycloak_boolean_result(
            keycloak_telemetry_metrics_enabled(ctx),
        )))
    }

    fn native_jaxrs_multivalued_map_add(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        let key = args.get(1).copied().unwrap_or(Value::Object(None));
        let value = args.get(2).copied().unwrap_or(Value::Object(None));
        let store = match ctx.get_field_by_name(this, "store") {
            Value::Object(Some(o)) => o,
            _ => return Ok(None),
        };

        let base_pin = ctx.pin_native_root(this);
        let store_pin = ctx.pin_native_root(store);
        let key_pin = match key {
            Value::Object(Some(o)) => Some(ctx.pin_native_root(o)),
            _ => None,
        };
        let value_pin = match value {
            Value::Object(Some(o)) => Some(ctx.pin_native_root(o)),
            _ => None,
        };

        let current_key = match (key, key_pin) {
            (Value::Object(Some(o)), Some(pin)) => Value::Object(Some(ctx.read_native_pin(pin, o))),
            _ => key,
        };
        let current_store = ctx.read_native_pin(store_pin, store);
        let list_value = ctx.invoke_virtual(
            current_store,
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[current_key],
        )?;

        let list = match list_value {
            Some(Value::Object(Some(o))) => o,
            _ => {
                let new_list =
                    match ctx.new_object_initialized("java/util/LinkedList", "()V", &[])? {
                        Some(Value::Object(Some(o))) => o,
                        _ => {
                            ctx.unpin_native_roots(base_pin);
                            return Ok(None);
                        }
                    };
                let list_pin = ctx.pin_native_root(new_list);
                let current_key = match (key, key_pin) {
                    (Value::Object(Some(o)), Some(pin)) => {
                        Value::Object(Some(ctx.read_native_pin(pin, o)))
                    }
                    _ => key,
                };
                let current_store = ctx.read_native_pin(store_pin, store);
                let current_list = ctx.read_native_pin(list_pin, new_list);
                let _ = ctx.invoke_virtual(
                    current_store,
                    "put",
                    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                    &[current_key, Value::Object(Some(current_list))],
                )?;
                ctx.read_native_pin(list_pin, new_list)
            }
        };
        let list_pin = ctx.pin_native_root(list);

        let current_value = match (value, value_pin) {
            (Value::Object(Some(o)), Some(pin)) => Value::Object(Some(ctx.read_native_pin(pin, o))),
            _ => value,
        };
        let current_list = ctx.read_native_pin(list_pin, list);
        if matches!(current_value, Value::Object(None)) {
            let current_this = ctx.read_native_pin(base_pin, this);
            let _ = ctx.invoke_virtual(
                current_this,
                "addNull",
                "(Ljava/util/List;)V",
                &[Value::Object(Some(current_list))],
            )?;
        } else {
            let _ = ctx.invoke_virtual(
                current_list,
                "add",
                "(Ljava/lang/Object;)Z",
                &[current_value],
            )?;
        }
        ctx.unpin_native_roots(base_pin);
        Ok(None)
    }

    fn picocli_annotation_desc_is_member(desc: &str) -> bool {
        matches!(
            desc,
            "Lpicocli/CommandLine$Option;"
                | "Lpicocli/CommandLine$Parameters;"
                | "Lpicocli/CommandLine$ArgGroup;"
                | "Lpicocli/CommandLine$Unmatched;"
                | "Lpicocli/CommandLine$Mixin;"
                | "Lpicocli/CommandLine$Spec;"
                | "Lpicocli/CommandLine$ParentCommand;"
        )
    }

    fn native_picocli_typed_member_is_annotated(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let element = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let element_class = ctx.class_name_of_id(ctx.class_id_of_object(element));
        match element_class.as_deref() {
            Some("java/lang/reflect/Field") => {
                if let Some((class_id, field_name)) =
                    crate::lang_class::field_class_and_name(ctx, element)
                {
                    let present = ctx
                        .field_annotations(class_id, &field_name)
                        .iter()
                        .any(|ann| picocli_annotation_desc_is_member(&ann.type_descriptor));
                    return Ok(Some(Value::Int(if present { 1 } else { 0 })));
                }
            }
            Some("java/lang/reflect/Method") | Some("java/lang/reflect/Constructor") => {
                if let Some((class_id, method_name, method_desc)) =
                    crate::lang_class::method_class_name_desc(ctx, element)
                {
                    let present = ctx
                        .method_annotations(class_id, &method_name, &method_desc)
                        .iter()
                        .any(|ann| picocli_annotation_desc_is_member(&ann.type_descriptor));
                    return Ok(Some(Value::Int(if present { 1 } else { 0 })));
                }
            }
            _ => {}
        }

        let annotation_classes = [
            "picocli/CommandLine$Option",
            "picocli/CommandLine$Parameters",
            "picocli/CommandLine$ArgGroup",
            "picocli/CommandLine$Unmatched",
            "picocli/CommandLine$Mixin",
            "picocli/CommandLine$Spec",
            "picocli/CommandLine$ParentCommand",
        ];
        let element_pin = ctx.pin_native_root(element);
        for annotation_class in annotation_classes {
            let mirror = match ctx.ensure_class_initialized(annotation_class) {
                Ok(class_id) => ctx.get_class_mirror(class_id),
                Err(_) => continue,
            };
            let mirror_pin = ctx.pin_native_root(mirror);
            let current_element = ctx.read_native_pin(element_pin, element);
            let current_mirror = ctx.read_native_pin(mirror_pin, mirror);
            let result = ctx.invoke_virtual(
                current_element,
                "isAnnotationPresent",
                "(Ljava/lang/Class;)Z",
                &[Value::Object(Some(current_mirror))],
            )?;
            if matches!(result, Some(Value::Int(v)) if v != 0) {
                ctx.unpin_native_roots(element_pin);
                return Ok(Some(Value::Int(1)));
            }
        }
        ctx.unpin_native_roots(element_pin);
        Ok(Some(Value::Int(0)))
    }

    fn native_picocli_usage_interpolate_string(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(args.get(1).copied()),
        };
        let text: ObjectRef = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let value = ctx.read_string(text).unwrap_or_default();
        if !value.contains("${") {
            return Ok(Some(Value::Object(Some(text))));
        }
        let interpolator = match ctx.get_field_by_name(this, "interpolator") {
            Value::Object(Some(o)) => o,
            _ => return Ok(Some(Value::Object(Some(text)))),
        };
        let text_pin = ctx.pin_native_root(text);
        let interpolator_pin = ctx.pin_native_root(interpolator);
        let current_interpolator = ctx.read_native_pin(interpolator_pin, interpolator);
        let current_text = ctx.read_native_pin(text_pin, text);
        let result = ctx.invoke_virtual(
            current_interpolator,
            "interpolate",
            "(Ljava/lang/String;)Ljava/lang/String;",
            &[Value::Object(Some(current_text))],
        );
        ctx.unpin_native_roots(text_pin);
        result
    }

    fn native_picocli_model_is_non_default(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let value = args.first().copied().unwrap_or(Value::Object(None));
        let default_value = args.get(1).copied().unwrap_or(Value::Object(None));
        if value == default_value {
            return Ok(Some(Value::Int(0)));
        }
        let default_obj: ObjectRef = match default_value {
            Value::Object(Some(o)) => o,
            _ => return Ok(Some(Value::Int(1))),
        };
        if let (Value::Object(Some(value_obj)), Some(default_string)) =
            (value, ctx.read_string(default_obj))
        {
            if let Some(value_string) = ctx.read_string(value_obj) {
                return Ok(Some(Value::Int(if value_string != default_string {
                    1
                } else {
                    0
                })));
            }
        }

        let default_pin = ctx.pin_native_root(default_obj);
        let value_pin = match value {
            Value::Object(Some(o)) => Some(ctx.pin_native_root(o)),
            _ => None,
        };
        let current_default = ctx.read_native_pin(default_pin, default_obj);
        let current_value = match (value, value_pin) {
            (Value::Object(Some(o)), Some(pin)) => Value::Object(Some(ctx.read_native_pin(pin, o))),
            _ => value,
        };
        let equals = ctx.invoke_virtual(
            current_default,
            "equals",
            "(Ljava/lang/Object;)Z",
            &[current_value],
        )?;
        ctx.unpin_native_roots(default_pin);
        Ok(Some(Value::Int(match equals {
            Some(Value::Int(v)) if v != 0 => 0,
            _ => 1,
        })))
    }

    fn native_picocli_model_array_is_non_default(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let value = args.first().copied().unwrap_or(Value::Object(None));
        let default_value = args.get(1).copied().unwrap_or(Value::Object(None));
        if value == default_value {
            return Ok(Some(Value::Int(0)));
        }
        if matches!(default_value, Value::Object(None)) {
            return Ok(Some(Value::Int(1)));
        }
        let equals = ctx.invoke(
            "java/util/Arrays",
            "equals",
            "([Ljava/lang/Object;[Ljava/lang/Object;)Z",
            &[default_value, value],
        )?;
        Ok(Some(Value::Int(match equals {
            Some(Value::Int(v)) if v != 0 => 0,
            _ => 1,
        })))
    }

    fn keycloak_similarity_bigram_frequency(s: &str) -> rustc_hash::FxHashMap<(u16, u16), i32> {
        let units: Vec<u16> = s.encode_utf16().collect();
        let mut freq = rustc_hash::FxHashMap::default();
        if units.len() < 2 {
            return freq;
        }
        for pair in units.windows(2) {
            *freq.entry((pair[0], pair[1])).or_insert(0) += 1;
        }
        freq
    }

    fn keycloak_similarity_dot(
        left: &rustc_hash::FxHashMap<(u16, u16), i32>,
        right: &rustc_hash::FxHashMap<(u16, u16), i32>,
    ) -> f64 {
        left.iter()
            .map(|(key, value)| (*value as f64) * (*right.get(key).unwrap_or(&0) as f64))
            .sum()
    }

    fn keycloak_cosine_similarity(
        lower_input_freq: &rustc_hash::FxHashMap<(u16, u16), i32>,
        candidate: &str,
    ) -> f64 {
        let candidate_lower = candidate.to_lowercase();
        let candidate_freq = keycloak_similarity_bigram_frequency(&candidate_lower);
        let dot = keycloak_similarity_dot(lower_input_freq, &candidate_freq);
        let norm_input = keycloak_similarity_dot(lower_input_freq, lower_input_freq);
        let norm_candidate = keycloak_similarity_dot(&candidate_freq, &candidate_freq);
        let denominator = (norm_input * norm_candidate).sqrt();
        if denominator == 0.0 {
            0.0
        } else {
            dot / denominator
        }
    }

    fn keycloak_similarity_result_list(
        ctx: &mut dyn NativeContext,
        input: String,
        candidates: ObjectRef,
        max_suggestions: usize,
        min_similarity: f64,
    ) -> MethodCallResult {
        let lower_input = input.to_lowercase();
        let lower_input_freq = keycloak_similarity_bigram_frequency(&lower_input);
        let count = picocli_list_size(ctx, candidates).min(100_000);
        let mut scored: Vec<(f64, usize, String)> = Vec::new();
        for idx in 0..count {
            let Some(candidate_obj) = picocli_list_get(ctx, candidates, idx) else {
                continue;
            };
            let Some(candidate) = ctx.read_string(candidate_obj) else {
                continue;
            };
            let score = keycloak_cosine_similarity(&lower_input_freq, &candidate);
            if score >= min_similarity {
                scored.push((score, idx, candidate));
            }
        }
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
        });

        let list = match ctx.new_object_initialized("java/util/ArrayList", "()V", &[])? {
            Some(Value::Object(Some(list))) => list,
            _ => return Ok(Some(Value::Object(None))),
        };
        let list_pin = ctx.pin_native_root(list);
        for (_, _, candidate) in scored.into_iter().take(max_suggestions) {
            let candidate_string = ctx.create_string(&candidate);
            let string_pin = ctx.pin_native_root(candidate_string);
            let current_list = ctx.read_native_pin(list_pin, list);
            let current_string = ctx.read_native_pin(string_pin, candidate_string);
            let _ = ctx.invoke_virtual(
                current_list,
                "add",
                "(Ljava/lang/Object;)Z",
                &[Value::Object(Some(current_string))],
            )?;
            ctx.unpin_native_roots(string_pin);
        }
        let list = ctx.read_native_pin(list_pin, list);
        ctx.unpin_native_roots(list_pin);
        Ok(Some(Value::Object(Some(list))))
    }

    fn native_keycloak_similarity_find_similar_default(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let input = match args.first() {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        let candidates = match args.get(1) {
            Some(Value::Object(Some(list))) => *list,
            _ => return keycloak_empty_array_list(ctx),
        };
        keycloak_similarity_result_list(ctx, input, candidates, 5, 0.4)    }

    fn native_keycloak_similarity_find_similar(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let input = match args.first() {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        let candidates = match args.get(1) {
            Some(Value::Object(Some(list))) => *list,
            _ => return keycloak_empty_array_list(ctx),
        };
        let max_suggestions = match args.get(2) {
            Some(Value::Int(v)) if *v > 0 => *v as usize,
            _ => 0,
        };
        let min_similarity = match args.get(3) {
            Some(Value::Double(v)) => *v,
            Some(Value::Float(v)) => *v as f64,
            _ => 0.0,
        };
        keycloak_similarity_result_list(ctx, input, candidates, max_suggestions, min_similarity)
    }

    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/PropertyMapper",
        "getEnabledWhen",
        "()Ljava/util/Optional;",
        native_keycloak_property_mapper_get_enabled_when, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/PropertyMapper",
        "getRequiredWhen",
        "()Ljava/util/Optional;",
        native_keycloak_property_mapper_get_required_when, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/LoggingPropertyMappers",
        "isConsoleEnabled",
        "()Z",
        native_keycloak_log_console_enabled, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/LoggingPropertyMappers",
        "isFileEnabled",
        "()Z",
        native_keycloak_log_file_enabled, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/LoggingPropertyMappers",
        "isSyslogEnabled",
        "()Z",
        native_keycloak_log_syslog_enabled, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/LoggingPropertyMappers",
        "isConsoleAsyncEnabled",
        "()Z",
        native_keycloak_log_console_async_enabled, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/LoggingPropertyMappers",
        "isFileAsyncEnabled",
        "()Z",
        native_keycloak_log_file_async_enabled, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/LoggingPropertyMappers",
        "isSyslogAsyncEnabled",
        "()Z",
        native_keycloak_log_syslog_async_enabled, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/LoggingPropertyMappers",
        "isConsoleJsonEnabled",
        "()Z",
        native_keycloak_log_console_json_enabled, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/LoggingPropertyMappers",
        "isFileJsonEnabled",
        "()Z",
        native_keycloak_log_file_json_enabled, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/LoggingPropertyMappers",
        "isSyslogJsonEnabled",
        "()Z",
        native_keycloak_log_syslog_json_enabled, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/LoggingPropertyMappers",
        "isFileRotationEnabled",
        "()Z",
        native_keycloak_log_file_rotation_enabled, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/MetricsPropertyMappers",
        "metricsEnabled",
        "()Z",
        native_keycloak_metrics_enabled, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/CachingPropertyMappers",
        "cacheSetToInfinispan",
        "()Z",
        native_keycloak_cache_set_to_infinispan, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/TelemetryPropertyMappers",
        "isTelemetryEnabled",
        "()Z",
        native_keycloak_telemetry_enabled, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/TelemetryPropertyMappers",
        "isTelemetryLogsEnabled",
        "()Z",
        native_keycloak_telemetry_logs_enabled, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/mappers/TelemetryPropertyMappers",
        "isTelemetryMetricsEnabled",
        "()Z",
        native_keycloak_telemetry_metrics_enabled, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "jakarta/ws/rs/core/AbstractMultivaluedMap",
        "add",
        "(Ljava/lang/Object;Ljava/lang/Object;)V",
        native_jaxrs_multivalued_map_add, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "picocli/CommandLine$Model$TypedMember",
        "isAnnotated",
        "(Ljava/lang/reflect/AnnotatedElement;)Z",
        native_picocli_typed_member_is_annotated, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "picocli/CommandLine$Model$UsageMessageSpec",
        "interpolate",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_picocli_usage_interpolate_string, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "picocli/CommandLine$Model",
        "isNonDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
        native_picocli_model_is_non_default, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "picocli/CommandLine$Model",
        "isNonDefault",
        "([Ljava/lang/Object;[Ljava/lang/Object;)Z",
        native_picocli_model_array_is_non_default, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        "org/keycloak/quarkus/runtime/configuration/SimilarityUtil",
        "findSimilar",
        "(Ljava/lang/String;Ljava/util/List;)Ljava/util/List;",
        native_keycloak_similarity_find_similar_default, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "org/keycloak/quarkus/runtime/configuration/SimilarityUtil",
        "findSimilar",
        "(Ljava/lang/String;Ljava/util/List;ID)Ljava/util/List;",
        native_keycloak_similarity_find_similar, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- picocli CommandLine$Help$Ansi$Style.fg(String) / .bg(String) ---
    // Round 92: Keycloak's startup banner contains markup like `@|red ...|@`,
    // which picocli parses via `Ansi.string` -> `Style.parse` -> `Style.fg("red")`.
    // The Java code does:
    //   try { return Style.valueOf(name.toLowerCase()); }   // "red" -> IAE
    //   catch (Exception ex) {
    //     try { return Style.valueOf("fg_" + name.toLowerCase()); }  // "fg_red" -> hit
    //     catch (Exception ex2) { return new Palette256Color(true, name); }
    //   }
    // Under CratonVM the inner-most `IllegalArgumentException` thrown by
    // `Enum.valueOf` propagates out of `Style.fg` rather than being caught by
    // the surrounding `catch (Exception)` handler — exception-table walking
    // for nested invocations across this many frames misroutes the unwind.
    // Implementing `fg`/`bg` natively bypasses the try/catch entirely: we
    // resolve the static Style constant by name and return it directly. If
    // neither lookup hits, we return the canonical `reset` Style as a
    // harmless no-op — Keycloak's banner rendering only needs a non-null
    // IStyle that produces no ANSI codes when used with Ansi.OFF.
    //
    // gc-common w15-f: takes the name as TEXT. It took the String object and
    // read it after `ensure_class_initialized`, which can run `Style.<clinit>`
    // (Java) and move it; the callers now read it before the call.
    fn picocli_style_lookup(
        ctx: &mut dyn NativeContext,
        prefix: &str,
        raw_name: &str,
    ) -> cratonvm_types::Value {
        let style_cls = "picocli/CommandLine$Help$Ansi$Style";
        let name = raw_name.to_lowercase();
        let class_id = match ctx.ensure_class_initialized(style_cls) {
            Ok(id) => id,
            Err(_) => {
                if crate::nbflags().dbg_picocli_style {
                    eprintln!("[picocli-style] ensure_class_initialized failed");
                }
                return cratonvm_types::Value::Object(None);
            }
        };
        let dbg = crate::nbflags().dbg_picocli_style;
        // Round 93: Style constants are STATIC enum fields — must use
        // `static_field_index_by_name`, not `resolve_field_index` (which
        // only walks INSTANCE fields). Round 92's fallback was reaching
        // the `Object(None)` branch on every call, leaking null IStyle
        // to `Ansi.string`, which then NPEs in `Style.on()`.
        // Try the plain name first (e.g. "reset", "bold").
        if let Some(idx) = ctx.static_field_index_by_name(class_id, &name) {
            let v = ctx.get_static_field(class_id, idx);
            if dbg {
                eprintln!(
                    "[picocli-style] plain {} -> idx={} val_null={}",
                    name,
                    idx,
                    matches!(v, cratonvm_types::Value::Object(None))
                );
            }
            if !matches!(v, cratonvm_types::Value::Object(None)) {
                return v;
            }
        } else if dbg {
            eprintln!("[picocli-style] plain {} -> no static field", name);
        }
        // Then the prefixed form (e.g. "fg_red", "bg_blue").
        let prefixed = format!("{}{}", prefix, name);
        if let Some(idx) = ctx.static_field_index_by_name(class_id, &prefixed) {
            let v = ctx.get_static_field(class_id, idx);
            if dbg {
                eprintln!(
                    "[picocli-style] prefixed {} -> idx={} val_null={}",
                    prefixed,
                    idx,
                    matches!(v, cratonvm_types::Value::Object(None))
                );
            }
            if !matches!(v, cratonvm_types::Value::Object(None)) {
                return v;
            }
        } else if dbg {
            eprintln!("[picocli-style] prefixed {} -> no static field", prefixed);
        }
        // Fallback: return the always-present `reset` style. Any IStyle is
        // legal here — Ansi.OFF.string strips markup wholesale anyway.
        if let Some(idx) = ctx.static_field_index_by_name(class_id, "reset") {
            let v = ctx.get_static_field(class_id, idx);
            if dbg {
                eprintln!(
                    "[picocli-style] reset fallback idx={} val_null={}",
                    idx,
                    matches!(v, cratonvm_types::Value::Object(None))
                );
            }
            return v;
        }
        if dbg {
            eprintln!("[picocli-style] reset static field not found, returning null");
        }
        cratonvm_types::Value::Object(None)
    }
    r.register(
        "picocli/CommandLine$Help$Ansi$Style",
        "fg",
        "(Ljava/lang/String;)Lpicocli/CommandLine$Help$Ansi$IStyle;",
        |ctx, args| {
            let name_obj = match args.first() {
                Some(Value::Object(Some(s))) => *s,
                _ => return Ok(Some(Value::Object(None))),
            };
            let name = ctx.read_string(name_obj).unwrap_or_default();
            Ok(Some(picocli_style_lookup(ctx, "fg_", &name)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "picocli/CommandLine$Help$Ansi$Style",
        "bg",
        "(Ljava/lang/String;)Lpicocli/CommandLine$Help$Ansi$IStyle;",
        |ctx, args| {
            let name_obj = match args.first() {
                Some(Value::Object(Some(s))) => *s,
                _ => return Ok(Some(Value::Object(None))),
            };
            let name = ctx.read_string(name_obj).unwrap_or_default();
            Ok(Some(picocli_style_lookup(ctx, "bg_", &name)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    fn picocli_regex_word_hyphen(s: &str) -> bool {
        !s.is_empty()
            && s.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    }

    fn picocli_regex_transform(input: &str, synopsis: bool) -> String {
        if let Some(rest) = input.strip_prefix("--no-") {
            if picocli_regex_word_hyphen(rest) {
                return if synopsis {
                    format!("--[no-]{}", rest)
                } else {
                    format!("--{}", rest)
                };
            }
        }
        if let Some(rest) = input.strip_prefix("--") {
            if picocli_regex_word_hyphen(rest) {
                return if synopsis {
                    format!("--[no-]{}", rest)
                } else {
                    format!("--no-{}", rest)
                };
            }
        }

        let (prefix, rest) = if let Some(rest) = input.strip_prefix("--") {
            ("--", rest)
        } else if let Some(rest) = input.strip_prefix('-') {
            ("-", rest)
        } else {
            return input.to_string();
        };
        let Some(colon) = rest.find(':') else {
            return input.to_string();
        };
        if !rest[..colon]
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        {
            return input.to_string();
        }
        let after_colon = &rest[colon + 1..];
        let (replacement_sign, value) = if let Some(value) = after_colon.strip_prefix('+') {
            ('-', value)        } else if let Some(value) = after_colon.strip_prefix('-') {
            ('+', value)        } else {
            return input.to_string();
        };
        if !picocli_regex_word_hyphen(value) {
            return input.to_string();
        }
        if synopsis {
            format!("{}{}:(+|-){}", prefix, &rest[..colon], value)
        } else {
            format!("{}{}:{}{}", prefix, &rest[..colon], replacement_sign, value)
        }
    }

    fn picocli_new_regex_transformer(ctx: &mut dyn NativeContext) -> MethodCallResult {
        let obj = try_alloc_concurrent_synthetic(ctx, "picocli/CommandLine$RegexTransformer", 2)?;
        // Pin across the emptyMap invoke below — a moving young GC there would
        // relocate the fresh transformer (native stale-local family).
        let obj_pin = ctx.pin_native_root(obj);
        if let Ok(Some(empty_map)) = ctx.invoke(
            "java/util/Collections",
            "emptyMap",
            "()Ljava/util/Map;",
            &[],
        ) {
            let obj = ctx.read_native_pin(obj_pin, obj);
            ctx.set_field_by_name(obj, "replacements", empty_map);
            ctx.set_field_by_name(obj, "synopsis", empty_map);
        }
        let obj = ctx.read_native_pin(obj_pin, obj);
        ctx.unpin_native_roots(obj_pin);
        Ok(Some(Value::Object(Some(obj))))
    }

    fn native_picocli_regex_transform_negative(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let input = match args.get(1) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        let out = ctx.create_string(&picocli_regex_transform(&input, false));
        Ok(Some(Value::Object(Some(out))))
    }

    fn native_picocli_regex_transform_synopsis(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let input = match args.get(1) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        let out = ctx.create_string(&picocli_regex_transform(&input, true));
        Ok(Some(Value::Object(Some(out))))
    }

    r.register(
        "picocli/CommandLine$RegexTransformer",
        "createDefault",
        "()Lpicocli/CommandLine$RegexTransformer;",
        |ctx, _args| picocli_new_regex_transformer(ctx), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "picocli/CommandLine$RegexTransformer",
        "createCaseInsensitive",
        "()Lpicocli/CommandLine$RegexTransformer;",
        |ctx, _args| picocli_new_regex_transformer(ctx), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "picocli/CommandLine$RegexTransformer",
        "makeNegative",
        "(Ljava/lang/String;Lpicocli/CommandLine$Model$CommandSpec;)Ljava/lang/String;",
        native_picocli_regex_transform_negative, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "picocli/CommandLine$RegexTransformer",
        "makeSynopsis",
        "(Ljava/lang/String;Lpicocli/CommandLine$Model$CommandSpec;)Ljava/lang/String;",
        native_picocli_regex_transform_synopsis, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        "java/util/Arrays$ArrayList",
        "<init>",
        "([Ljava/lang/Object;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let array = args.get(1).copied().unwrap_or(Value::Object(None));
            // The real constructor is `a = Objects.requireNonNull(array)`: a
            // bare NullPointerException. This registration shadows that
            // constructor (under `--compatible` the `Arrays` family's
            // `JdkArraysBodies` row retires `Arrays.asList` to the JDK body,
            // which lands here), so it must keep the check or
            // `Arrays.asList((Object[]) null)` answers a list over a null
            // array (`ArrayListShadowSweep` row 125).
            if matches!(array, Value::Object(None)) {
                return Err(RuntimeError::NullPointerException { message: None }.into());
            }
            ctx.set_field_by_name(this, "a", array);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "java/util/Arrays$ArrayList",
        "toArray",
        "()[Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let backing = match ctx.get_field_by_name(this, "a") {
                Value::Object(Some(arr)) => arr,
                _ => {
                    let empty = ctx.new_ref_array(ClassId::new(0), 0);
                    return Ok(Some(Value::Object(Some(empty))));
                }
            };
            let len = ctx.array_length(backing);
            // gc-common w15-f: the backing array is rooted across the copy's
            // allocation and read back through its root; every element used to
            // be read from its pre-allocation address.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let backing_h = scope.root(backing);
            let out = scope.new_ref_array(ClassId::new(0), len);
            let backing = scope.get(&backing_h);
            for i in 0..len {
                let value = scope.get_array_element(backing, i);
                scope.set_array_element(out, i, value);
            }
            Ok(Some(Value::Object(Some(out))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        "picocli/CommandLine$Model$CaseAwareLinkedMap",
        "containsKey",
        "(Ljava/lang/Object;)Z",
        |ctx, args| {
            // gc-common w15-f: the receiver and its map are read BEFORE the
            // lower-cased key String is allocated, and the map is rooted
            // across that allocation. Both reads of `this` used to come after
            // it. Every early exit answers `false`, as before.
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let case_insensitive =
                matches!(ctx.get_field_by_name(this, "caseInsensitive"), Value::Int(v) if v != 0);
            let map_field = if case_insensitive {
                "keyMap"
            } else {
                "targetMap"
            };
            let map = match picocli_obj_field(ctx, this, map_field) {
                Some(map) => map,
                None => return Ok(Some(Value::Int(0))),
            };
            let lowered = match key {
                _ if !case_insensitive => None,
                Value::Object(Some(key_obj))
                    if ctx
                        .class_name_arc_of_id(ctx.class_id_of_object(key_obj))
                        .as_deref()
                        == Some("java/lang/String") =>
                {
                    Some(
                        ctx.read_string(key_obj)
                            .unwrap_or_default()
                            .to_ascii_lowercase(),
                    )
                }
                Value::Object(None) => None,
                _ => return Ok(Some(Value::Int(0))),
            };
            let Some(lowered) = lowered else {
                return ctx.invoke_virtual(map, "containsKey", "(Ljava/lang/Object;)Z", &[key]);
            };
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let map_h = scope.root(map);
            let lowered = scope.create_string(&lowered);
            let map = scope.get(&map_h);
            scope.invoke_virtual(
                map,
                "containsKey",
                "(Ljava/lang/Object;)Z",
                &[Value::Object(Some(lowered))],
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- Files extras ---
    reg_files! { r.register(files,
        "isDirectory",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z",
        |ctx, args| {
            obj_arg(args, 0)?;
            Ok(Some(Value::Int(i32::from(p57_files_is_directory_impl(
                ctx, args,
            )))))
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    reg_files! { r.register(files,
        "isRegularFile",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z",
        |ctx, args| {
            obj_arg(args, 0)?;
            Ok(Some(Value::Int(i32::from(p57_files_is_regular_file_impl(
                ctx, args,
            )))))
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    reg_files! { r.register(files,
        "isSymbolicLink",
        "(Ljava/nio/file/Path;)Z",
        |ctx, args| {
            let mut path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, &mut path_obj);
            let is_link = jrtfs_decode(&p)
                .and_then(|(java_home, entry)| {
                    jrt_image(&java_home).and_then(|image| {
                        jrt_package_link_target(&image, &entry.strip_prefix("packages/")?)
                    })
                })
                .is_some()
                || std::path::Path::new(&p).is_symlink();
            Ok(Some(Value::Int(if is_link { 1 } else { 0 })))
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    reg_files! { r.register(files,
        "readSymbolicLink",
        "(Ljava/nio/file/Path;)Ljava/nio/file/Path;",
        // Host symlinks and jrt package links both live in
        // `p57_read_symbolic_link`; this used to handle only the jrt case and
        // answered `NoSuchFileException` for every real symbolic link.
        |ctx, args| {
            let mut path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, &mut path_obj);
            p57_read_symbolic_link(ctx, &p)
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    reg_files! { r.register(files,
        "isReadable",
        "(Ljava/nio/file/Path;)Z",
        |ctx, args| {
            let mut path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, &mut path_obj);
            let readable = match vfs_classify(&p) {
                Some(kind) => !matches!(kind, JarFsKind::Absent),
                None => std::path::Path::new(&p).exists(),
            };
            Ok(Some(Value::Int(if readable { 1 } else { 0 })))
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    reg_files! { r.register(files,
        "isWritable",
        "(Ljava/nio/file/Path;)Z",
        |ctx, args| {
            let mut path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, &mut path_obj);
            // Was existence-only (ignored real permission bits entirely), so
            // `Files.isWritable` never reflected `Files.setPosixFilePermissions`/
            // `PosixFileAttributeView.setPermissions` clearing the write bits —
            // H2's `FilePathDisk.canWrite()` calls this directly, so
            // `TestFileSystem.testSetReadOnly`'s `assertFalse(canWrite(...))`
            // would still fail even once `setReadOnly()` itself stopped
            // throwing. Mirror `File.canWrite()`'s real permission check
            // (same `Permissions::readonly()` query) instead.
            let writable = std::fs::metadata(&p)
                .map(|m| !m.permissions().readonly())
                .unwrap_or(false);
            Ok(Some(Value::Int(if writable { 1 } else { 0 })))
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    reg_files! { r.register(files, "size", "(Ljava/nio/file/Path;)J", p59_files_size, cratonvm_native_api::NativeKind::Bridge) }

    // `Files.list` / `Files.walk` return a `Stream<Path>` built (in the real
    // JDK) by wrapping `newDirectoryStream(dir).iterator()` in a
    // `Spliterators.spliteratorUnknownSize` Stream. On CratonVM that real
    // bytecode path does not reach the synthetic `newDirectoryStream` provider
    // native for non-default (jar/jrt) filesystems, so the resulting Stream is
    // empty — which broke in-process javac (it enumerates the runtime image's
    // packages via `Files.list`/`Files.walk` and saw zero, reporting "Unable to
    // find package java.lang in platform classes"). Register these directly so
    // they return a fully-functional eager synthetic Stream over the listing,
    // uniformly for host / jar / jrt paths.
    reg_files! { r.register(files,
        "list",
        "(Ljava/nio/file/Path;)Ljava/util/stream/Stream;",
        |ctx, args| {
            let mut path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, &mut path_obj);
            // `Files.list` is `newDirectoryStream(dir)` wrapped in a Stream, and
            // it inherits that method's refusals: a missing path is a
            // `NoSuchFileException` and a regular file a `NotDirectoryException`,
            // both raised HERE, at construction, before any element is produced
            // (measured on HotSpot 25.0.3+9 — `Files.list(missing)` throws from
            // the `Files.list(...)` call itself, not from the terminal op).
            // `vfs_or_host_list` answers BOTH conditions with an empty Vec, so
            // this used to hand back an empty Stream and the caller's
            // `.forEach(...)` did nothing at all, successfully.
            // G4-1-the-io-and-nio-fabricated-success-sweep-measured-20260816.md
            if let Some(refused) = p57_dir_listing_refusal(ctx, &p, true)? {
                return Err(refused);
            }
            let entries = vfs_or_host_list(&p);
            p57_paths_stream(ctx, &entries)
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }
    /// A `Stream<Path>` of `entries`, one fresh `Path` each (gc-common w18-f).
    ///
    /// Every `Path` is rooted from its allocation until the stream has rooted
    /// its elements (`make_stream_from_elements` pins its input first), and
    /// handed over at its CURRENT address. `Files.list` / `Files.walk` used to
    /// collect the raw addresses into a `Vec` across every later path's
    /// allocation, so a moving collection mid-walk left the stream holding the
    /// vacated addresses of the earlier paths.
    fn p57_paths_stream(ctx: &mut dyn NativeContext, entries: &[String]) -> MethodCallResult {
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let mut handles = Vec::with_capacity(entries.len());
        for e in entries {
            let ep = p57_alloc_path(&mut *scope, e)?;
            handles.push(scope.root(ep));
        }
        let vals: Vec<Value> = handles
            .iter()
            .map(|h| Value::Object(Some(scope.get(h))))
            .collect();
        cratonvm_native_collections::make_stream_from_elements(&mut *scope, &vals)
    }
    // Files.walk(Path, FileVisitOption...) and the maxDepth overload.
    fn files_walk_stream(
        ctx: &mut dyn NativeContext,
        mut path_obj: ObjectRef,
        max_depth: usize,
        follow_links: bool,
    ) -> MethodCallResult {
        let p = p57_read_path(ctx, &mut path_obj);
        // `Files.walk(missing)` throws `NoSuchFileException` at CONSTRUCTION on
        // HotSpot (measured 2026-08-16: both `Files.walk(missing)` and
        // `Files.walk(missing).count()` throw it) because `FileTreeWalker`
        // reads the start element's attributes before yielding anything. This
        // walker instead pushed the start path unconditionally
        // (`out.push(p.to_string())`) and then found no children, so a walk over
        // a path that is not there returned a one-element Stream containing the
        // path that is not there. A regular file IS a legal start element and
        // yields exactly itself (measured: `Files.walk(<file>).count() == 1`),
        // so the directory requirement is off here.
        // G4-1-the-io-and-nio-fabricated-success-sweep-measured-20260816.md
        if let Some(refused) = p57_dir_listing_refusal(ctx, &p, false)? {
            return Err(refused);
        }
        let mut paths = Vec::new();
        vfs_or_host_walk(&p, 0, max_depth, follow_links, &mut paths);
        p57_paths_stream(ctx, &paths)
    }
    reg_files! { r.register(files,
        "walk",
        "(Ljava/nio/file/Path;[Ljava/nio/file/FileVisitOption;)Ljava/util/stream/Stream;",
        |ctx, args| {
            // gc-common w21-b: the ORDER here is not what keeps `path_obj`
            // current -- a native's `args` slice is a snapshot no collection
            // rewrites, so an `ObjectRef` read out of it AFTER Java is exactly
            // as stale as one read before (`common-w20d`). It is safe because
            // this descriptor's options are the varargs ARRAY, which
            // `p57_visit_options_follow_links` answers by length and kind
            // without calling Java (only a `Set` reaches its `isEmpty()`
            // probe). A caller whose option probe CAN run Java lifts and roots
            // every argument first (see `walkFileTree`'s Set form).
            let follow = p57_visit_options_follow_links(ctx, args.get(1));
            let path_obj = obj_arg(args, 0)?;
            files_walk_stream(ctx, path_obj, usize::MAX, follow)
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }
    reg_files! { r.register(files,
        "walk",
        "(Ljava/nio/file/Path;I[Ljava/nio/file/FileVisitOption;)Ljava/util/stream/Stream;",
        |ctx, args| {
            let follow = p57_visit_options_follow_links(ctx, args.get(2));
            let path_obj = obj_arg(args, 0)?;
            // A negative depth is the one value the JDK refuses, and the arm
            // below promoted it to `usize::MAX` — an UNBOUNDED walk where the
            // caller asked for a bounded one. See `p57_max_depth_refusal`.
            if let Some(refused) = p57_max_depth_refusal(args.get(1)) {
                return Err(refused);
            }
            let max_depth = match args.get(1) {
                Some(Value::Int(n)) if *n >= 0 => *n as usize,
                _ => usize::MAX,
            };
            files_walk_stream(ctx, path_obj, max_depth, follow)
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }
    reg_files! { r.register(files,
        "find",
        "(Ljava/nio/file/Path;ILjava/util/function/BiPredicate;[Ljava/nio/file/FileVisitOption;)Ljava/util/stream/Stream;",
        |ctx, args| {
            // The option array never calls Java -- see the `walk` registration
            // above. The Path read can (a foreign Path's `toString()`), and the
            // `BiPredicate` is read from `args` after it, so the read hands
            // `args` back current (gc-common w21-b, `common-w20d`).
            let follow = p57_visit_options_follow_links(ctx, args.get(3));
            let (p, args) = p57_read_path_arg(ctx, args, 0)?;
            let args = &*args;
            // Same two refusals as `walk` — `Files.find` is `walk` with a
            // predicate and shares both contracts (measured 2026-08-16:
            // `Files.find(missing,1,…)` -> NoSuchFileException,
            // `Files.find(dir,-1,…)` -> IllegalArgumentException).
            if let Some(refused) = p57_max_depth_refusal(args.get(1)) {
                return Err(refused);
            }
            if let Some(refused) = p57_dir_listing_refusal(ctx, &p, false)? {
                return Err(refused);
            }
            let max_depth = match args.get(1) {
                Some(Value::Int(n)) if *n >= 0 => *n as usize,
                _ => usize::MAX,
            };
            let matcher = match args.get(2).copied().unwrap_or(Value::Object(None)) {
                Value::Object(Some(o)) => Some(o),
                _ => None,
            };
            let mut paths = Vec::new();
            vfs_or_host_walk(&p, 0, max_depth, follow, &mut paths);
            // gc-common w18-f: one handle scope for the whole walk. The
            // `BiPredicate` (read from `args` before the first allocation) and
            // every KEPT path are rooted until the stream has its elements, and
            // re-read before each use. The predicate used to be held raw across
            // every path allocation and every `test` call (Java), and the kept
            // paths were collected into a `Vec` at addresses that the next
            // iteration's allocation or predicate call could vacate.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let matcher_h = matcher.map(|m| scope.root(m));
            // Without FOLLOW_LINKS the JDK's walker reads each entry's
            // attributes with NOFOLLOW_LINKS, which is what lets a `BiPredicate`
            // see `attrs.isSymbolicLink()`. `p59_files_read_attributes` reads
            // that request off a non-empty `LinkOption[]` (the enum has one
            // constant), so hand it a 1-element array when not following.
            let nofollow_h = if follow {
                None
            } else {
                let arr = scope.new_array(cratonvm_types::ArrayElementType::Reference, 1);
                Some(scope.root(arr))
            };
            let mut kept = Vec::new();
            for e in &paths {
                let ep = p57_alloc_path(&mut *scope, e)?;
                let ep_h = scope.root(ep);
                let opts = match &nofollow_h {
                    Some(h) => Value::Object(Some(scope.get(h))),
                    None => Value::Object(None),
                };
                let ep = scope.get(&ep_h);
                let attrs =
                    match p59_files_read_attributes(&mut *scope, &[Value::Object(Some(ep)), opts])? {
                        Some(Value::Object(Some(attrs))) => attrs,
                        _ => continue,
                    };
                // Nothing allocates between here and the uses of `attrs`.
                let ep = scope.get(&ep_h);
                let keep = match &matcher_h {
                    Some(h) => {
                        let matcher = scope.get(h);
                        match scope.invoke_virtual(
                            matcher,
                            "test",
                            "(Ljava/lang/Object;Ljava/lang/Object;)Z",
                            &[Value::Object(Some(ep)), Value::Object(Some(attrs))],
                        )? {
                            Some(Value::Int(v)) => v != 0,
                            _ => false,
                        }
                    }
                    None => !matches!(scope.get_field(attrs, 3), Value::Int(v) if v != 0),
                };
                if keep {
                    kept.push(ep_h);
                }
            }
            let vals: Vec<Value> = kept
                .iter()
                .map(|h| Value::Object(Some(scope.get(h))))
                .collect();
            cratonvm_native_collections::make_stream_from_elements(&mut *scope, &vals)
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    reg_files! { r.register(files,
        "readString",
        "(Ljava/nio/file/Path;)Ljava/lang/String;",
        |ctx, args| {
            let mut path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, &mut path_obj);
            match p57_read_to_string_strict(&p) {
                Ok(Ok(content)) => {
                    let s = ctx.create_string(&content);
                    Ok(Some(Value::Object(Some(s))))
                }
                // Malformed UTF-8 is a decoding REFUSAL, not a substitution —
                // see `p57_malformed_input`.
                Ok(Err(len)) => Err(p57_malformed_input(ctx, len as i32).unwrap_or_else(|| {
                    RuntimeError::IOException {
                        message: "MalformedInputException".into(),
                    }
                    .into()
                })),
                // NIO contract: missing file → NoSuchFileException (see
                // newByteChannel above), not a bare IOException — callers like
                // FileSystemResource.getContentAsString() catch
                // NoSuchFileException and translate it to FileNotFoundException.
                Err(e) => Err(p57_fs_error(ctx, &e, &p)),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    reg_files! { r.register(files,
        "readString",
        "(Ljava/nio/file/Path;Ljava/nio/charset/Charset;)Ljava/lang/String;",
        |ctx, args| {
            // The Charset is read from `args` after the Path: current args.
            let (p, args) = p57_read_path_arg(ctx, args, 0)?;
            let args = &*args;
            // `readString(Path, Charset)` DECODES WITH THE CHARSET it is
            // given -- see `p57_decode_with_charset`, and the measurement of
            // what ignoring it cost.
            if matches!(args.get(1), Some(Value::Object(None))) {
                return Err(RuntimeError::NullPointerException { message: None }.into());
            }
            let raw = match p57_read_bytes(&p) {
                Ok(b) => b,
                Err(e) => return Err(p57_fs_error(ctx, &e, &p)),
            };
            let cs = match args.get(1) {
                Some(Value::Object(Some(c))) => Some(*c),
                _ => None,
            };
            match p57_decode_with_charset(ctx, &raw, cs) {
                Ok(Ok(content)) => {
                    let s = ctx.create_string(&content);
                    Ok(Some(Value::Object(Some(s))))
                }
                Ok(Err(len)) => Err(p57_malformed_input(ctx, len as i32).unwrap_or_else(|| {
                    RuntimeError::IOException {
                        message: "MalformedInputException".into(),
                    }
                    .into()
                })),
                // The charset could not be identified: keep the UTF-8 answer
                // this overload gave before rather than guess a different one.
                Err(()) => match p57_read_to_string_strict(&p) {
                    Ok(Ok(content)) => {
                        let s = ctx.create_string(&content);
                        Ok(Some(Value::Object(Some(s))))
                    }
                    Ok(Err(len)) => {
                        Err(p57_malformed_input(ctx, len as i32).unwrap_or_else(|| {
                            RuntimeError::IOException {
                                message: "MalformedInputException".into(),
                            }
                            .into()
                        }))
                    }
                    Err(e) => Err(p57_fs_error(ctx, &e, &p)),
                },
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    reg_files! { r.register(files,
        "writeString",
        "(Ljava/nio/file/Path;Ljava/lang/CharSequence;[Ljava/nio/file/OpenOption;)Ljava/nio/file/Path;",
        files_write_string_impl, cratonvm_native_api::NativeKind::Bridge,
    ) }

    fn read_all_lines_impl(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> cratonvm_types::error::MethodCallResult {
        // The Charset is read from `args` after the Path: current args.
        let (p, args) = p57_read_path_arg(ctx, args, 0)?;
        let args = &*args;
        // `readAllLines` had NO NotFound arm at all, so a missing file was a
        // bare `IOException` where every sibling raises `NoSuchFileException`.
        //
        // ONE BODY SERVES BOTH DESCRIPTORS -- `(Path)` and `(Path, Charset)` --
        // so the charset has to be read here or the two-argument overload
        // ignores it, which is the defect `Files.readString(Path, Charset)`
        // carried until `p57_decode_with_charset` was written. Fixing one of a
        // pair and not the other is the shape this campaign keeps finding; the
        // pair is the same function here, which makes it cheaper to get right
        // and no less easy to miss.
        //
        // `args[1]` is a `Charset` ONLY on the two-argument form; the
        // one-argument form has no second argument at all, and an explicit
        // null is the JDK's NPE.
        if matches!(args.get(1), Some(Value::Object(None))) {
            return Err(RuntimeError::NullPointerException { message: None }.into());
        }
        let charset = match args.get(1) {
            Some(Value::Object(Some(c))) => Some(*c),
            _ => None,
        };
        let content = if charset.is_some() {
            match p57_read_bytes(&p) {
                Ok(raw) => match p57_decode_with_charset(ctx, &raw, charset) {
                    Ok(Ok(c)) => Ok(c),
                    Ok(Err(len)) => {
                        return Err(p57_malformed_input(ctx, len as i32).unwrap_or_else(|| {
                            RuntimeError::IOException {
                                message: "MalformedInputException".into(),
                            }
                            .into()
                        }))
                    }
                    // Unidentifiable charset: keep the UTF-8 answer.
                    Err(()) => match p57_read_to_string_strict(&p) {
                        Ok(Ok(c)) => Ok(c),
                        Ok(Err(len)) => {
                            return Err(p57_malformed_input(ctx, len as i32).unwrap_or_else(|| {
                                RuntimeError::IOException {
                                    message: "MalformedInputException".into(),
                                }
                                .into()
                            }))
                        }
                        Err(e) => Err(e),
                    },
                },
                Err(e) => Err(e),
            }
        } else {
            match p57_read_to_string_strict(&p) {
                Ok(Ok(c)) => Ok(c),
                Ok(Err(len)) => {
                    return Err(p57_malformed_input(ctx, len as i32).unwrap_or_else(|| {
                        RuntimeError::IOException {
                            message: "MalformedInputException".into(),
                        }
                        .into()
                    }))
                }
                Err(e) => Err(e),
            }
        };
        match content {
            Ok(content) => {
                let lines: Vec<&str> = content.lines().collect();
                // Resolve real-JDK ArrayList layout: elementData / size slots
                // can be at (1,2) when AbstractList.modCount occupies slot 0.
                // Fall back to synthetic (0,1) layout when the class isn't
                // available via the resolver.
                let data_slot = ctx
                    .resolve_field_index("java/util/ArrayList", "elementData")
                    .unwrap_or(0);
                let size_slot = ctx
                    .resolve_field_index("java/util/ArrayList", "size")
                    .unwrap_or(1);
                let n_fields = std::cmp::max(data_slot, size_slot) + 1;
                let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", n_fields)?;
                // Pin across the array/string allocs below — a moving young GC
                // there would relocate the fresh list/array (native
                // stale-local family).
                let list_pin = ctx.pin_native_root(list);
                use cratonvm_types::ArrayElementType;
                let arr = ctx.new_array(ArrayElementType::Reference, lines.len());
                let arr_pin = ctx.pin_native_root(arr);
                for (i, line) in lines.iter().enumerate() {
                    let s = ctx.create_string(line);
                    let arr = ctx.read_native_pin(arr_pin, arr);
                    ctx.set_array_element(arr, i, Value::Object(Some(s)));
                }
                let list = ctx.read_native_pin(list_pin, list);
                let arr = ctx.read_native_pin(arr_pin, arr);
                ctx.set_field(list, data_slot, Value::Object(Some(arr)));
                ctx.set_field(list, size_slot, Value::Int(lines.len() as i32));
                ctx.unpin_native_roots(list_pin);
                Ok(Some(Value::Object(Some(list))))
            }
            Err(e) => Err(p57_fs_error(ctx, &e, &p)),
        }
    }
    reg_files! { r.register(files,
        "readAllLines",
        "(Ljava/nio/file/Path;)Ljava/util/List;",
        read_all_lines_impl, cratonvm_native_api::NativeKind::Bridge,
    ) }
    reg_files! { r.register(files,
        "readAllLines",
        "(Ljava/nio/file/Path;Ljava/nio/charset/Charset;)Ljava/util/List;",
        read_all_lines_impl, cratonvm_native_api::NativeKind::Bridge,
    ) }

    reg_files! { r.register(files,
        "lines",
        "(Ljava/nio/file/Path;)Ljava/util/stream/Stream;",
        |ctx, args| {
            let mut path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, &mut path_obj);
            // `lines` is the third of the three decoding readers and the last
            // one still answering a bare `IOException` for a missing file and
            // silently substituting U+FFFD for malformed input. The other two
            // moved in batch 1; this one is here so all three agree.
            let read = match p57_read_to_string_strict(&p) {
                Ok(Ok(c)) => Ok(c),
                Ok(Err(len)) => {
                    return Err(p57_malformed_input(ctx, len as i32).unwrap_or_else(|| {
                        RuntimeError::IOException {
                            message: "MalformedInputException".into(),
                        }
                        .into()
                    }))
                }
                Err(e) => Err(e),
            };
            match read {
                Ok(content) => {
                    let lines: Vec<&str> = content.lines().collect();
                    use cratonvm_types::ArrayElementType;
                    let arr = ctx.new_array(ArrayElementType::Reference, lines.len());
                    // Pin across the string/stream allocs below — a moving
                    // young GC there would relocate the fresh array (native
                    // stale-local family).
                    let arr_pin = ctx.pin_native_root(arr);
                    for (i, line) in lines.iter().enumerate() {
                        let s = ctx.create_string(line);
                        let arr = ctx.read_native_pin(arr_pin, arr);
                        ctx.set_array_element(arr, i, Value::Object(Some(s)));
                    }
                    // gc-common w15-f: typed so the stale-handle audit sees the
                    // binding; the allocation is the last GC point before the store.
                    let stream: ObjectRef =
                        try_alloc_concurrent_synthetic(ctx, "java/util/stream/Stream", 1)?;
                    let arr = ctx.read_native_pin(arr_pin, arr);
                    ctx.set_field(stream, 0, Value::Object(Some(arr)));
                    ctx.unpin_native_roots(arr_pin);
                    Ok(Some(Value::Object(Some(stream))))
                }
                // The READ was moved to the strict decoder above and this arm
                // was left behind, so `Files.lines(<missing>)` kept answering a
                // bare `IOException` while its two siblings had already moved.
                // MEASURED after the first fix: `readAllLines` and `readString`
                // both raised `NoSuchFileException` and `lines` did not — which
                // is exactly the half-fixed shape this campaign keeps finding,
                // reached this time inside a single function rather than across
                // a duplicate pair.
                Err(e) => Err(p57_fs_error(ctx, &e, &p)),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    reg_files! { r.register(files, "createTempFile", "(Ljava/lang/String;Ljava/lang/String;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/file/Path;", |ctx, args| {
        let prefix_ref = obj_arg(args, 0)?;
        let prefix = ctx.read_string(prefix_ref).unwrap_or_default();
        let suffix = if let Value::Object(Some(s)) = args[1] {
            ctx.read_string(s).unwrap_or_else(|| ".tmp".to_string())
        } else {
            ".tmp".to_string()
        };
        let temp_dir = jdk_temp_dir(crate::lang_system::saved_system_property(
            ctx,
            "java.io.tmpdir",
        ));
        let path_str = jdk_create_temp_file(&temp_dir, &prefix, &suffix)?;
        // Real JDK: `TempFileHelper.create` creates temp FILES 0600 by
        // default, and honors an explicit `FileAttribute` above that. (The
        // `java.io.File.createTempFile` natives keep the umask default —
        // that API has no such contract.)
        apply_unix_mode(
            &path_str,
            posix_mode_from_file_attributes(ctx, args.get(2)).or(Some(0o600)),
        );
        let result = p57_alloc_path(ctx, &path_str)?;
        Ok(Some(Value::Object(Some(result))))
    }, cratonvm_native_api::NativeKind::Bridge) }

    reg_files! { r.register(files,
        "createTempDirectory",
        "(Ljava/lang/String;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/file/Path;",
        |ctx, args| {
            let prefix_ref = obj_arg(args, 0)?;
            let prefix = ctx.read_string(prefix_ref).unwrap_or_default();
            let temp_dir = jdk_temp_dir(crate::lang_system::saved_system_property(
                ctx,
                "java.io.tmpdir",
            ));
            let name = format!(
                "{}{}",
                prefix,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            );
            let full_path = temp_dir.join(name);
            std::fs::create_dir_all(&full_path).map_err(|e| p57_io_error(&e))?;
            let path_str = full_path.to_string_lossy().to_string();
            // Real JDK: `TempFileHelper.create` creates temp directories 0700
            // by default, and honors an explicit `FileAttribute` above that.
            apply_unix_mode(
                &path_str,
                posix_mode_from_file_attributes(ctx, args.get(1)).or(Some(0o700)),
            );
            let result = p57_alloc_path(ctx, &path_str)?;
            Ok(Some(Value::Object(Some(result))))
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    // `Files.delete` reports failure. It used to discard every error, which
    // made "the file is still there" indistinguishable from "deleted" — see
    // `p57_delete_path_checked` for the case H2 depends on.
    reg_files! { r.register(files,
        "delete",
        "(Ljava/nio/file/Path;)V",
        native_files_delete, cratonvm_native_api::NativeKind::Bridge,
    ) }

    reg_files! { r.register(files,
        "deleteIfExists",
        "(Ljava/nio/file/Path;)Z",
        |ctx, args| {
            let mut path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, &mut path_obj);
            if vfs_classify(&p).is_some() {
                p57_delete_path(&p);
                return Ok(Some(Value::Int(1)));
            }
            // `Path::exists()` follows links, so a DANGLING symlink read as
            // "does not exist" and was left on disk. The link itself is what
            // `deleteIfExists` is being asked about.
            if std::fs::symlink_metadata(&p).is_err() {
                return Ok(Some(Value::Int(0)));
            }
            // Absence is the only failure `deleteIfExists` swallows; every
            // other one it reports, exactly as `delete` does.
            match p57_delete_path_checked(&p) {
                Ok(()) => Ok(Some(Value::Int(1))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Some(Value::Int(0))),
                Err(e) => Err(p57_delete_error(ctx, &p, &e)?),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    reg_files! { r.register(files,
        "copy",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/CopyOption;)Ljava/nio/file/Path;",
        native_files_copy, cratonvm_native_api::NativeKind::Bridge,
    ) }

    reg_files! { r.register(files,
        "move",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/CopyOption;)Ljava/nio/file/Path;",
        native_files_move, cratonvm_native_api::NativeKind::Bridge,
    ) }
    reg_files! { r.register(files,
        "createDirectories",
        "(Ljava/nio/file/Path;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/file/Path;",
        |ctx, args| {
            let mut path_obj = obj_arg(args, 0)?;
            // gc-common w20-d: see `native_files_create_directory`.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let (path_h, attrs_h) = p57_root_path_and_attrs(&mut scope, path_obj, args.get(1));
            let ctx = &mut *scope;
            let p = p57_read_path(ctx, &mut path_obj);
            if let Some((jar, entry)) = jarfs_decode(&p) {
                return match jarfs_create_dir_entry(&jar, &entry) {
                    Ok(()) => Ok(Some(Value::Object(Some(scope.get(&path_h))))),
                    Err(e) => Err(p57_io_error(&e)),
                };
            }
            // Same `FileAttribute` contract as `createDirectory`. The real
            // JDK applies the mode to each directory it creates; applying it
            // to the leaf covers every caller we have (the parents are
            // usually pre-existing).
            let attrs = attrs_h.as_ref().map(|h| Value::Object(Some(scope.get(h))));
            let mode = posix_mode_from_file_attributes(&mut *scope, attrs.as_ref());
            let path_obj = scope.get(&path_h);
            let ctx = &mut *scope;
            match std::fs::create_dir_all(&p) {
                Ok(()) => {
                    apply_unix_mode(&p, mode);
                    Ok(Some(Value::Object(Some(path_obj))))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Err(p57_no_such_file(ctx, &p)?)
                }
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    Err(p57_access_denied(ctx, &p)?)
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    Err(p57_file_already_exists_synthetic(ctx, &p)?)
                }
                Err(e) => Err(p57_io_error(&e)),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    reg_files! { r.register(files,
        "createDirectory",
        "(Ljava/nio/file/Path;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/file/Path;",
        native_files_create_directory, cratonvm_native_api::NativeKind::Bridge,
    ) }

    // FileVisitResult enum values
    let fvr = "java/nio/file/FileVisitResult";

    // StandardOpenOption enum
    let soo = "java/nio/file/StandardOpenOption";

    // --- FileSystemProvider file I/O methods ---
    r.register(
        fsp,
        "newByteChannel",
        "(Ljava/nio/file/Path;Ljava/util/Set;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/channels/SeekableByteChannel;",
        |ctx, args| {
            retired_receiver_yield!(
                ctx,
                args,
                "newByteChannel",
                "(Ljava/nio/file/Path;Ljava/util/Set;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/channels/SeekableByteChannel;"
            );
            // gc-common w20-d: every object argument is pinned at entry and
            // read back through its pin. The option scan below calls
            // `toString()` on the `Set` (Java, and a `HashSet`'s allocates),
            // and so does the missing-file pre-check; the pre-check then read
            // the Set out of `args` again, and the delegation at the bottom
            // forwarded `args` itself -- provider, Path, Set and attributes at
            // their pre-collection addresses (a collection does not rewrite a
            // native's argument slice). Pins are released when the native
            // returns.
            let pinned: Vec<Option<(usize, ObjectRef)>> = args
                .iter()
                .map(|v| match v {
                    Value::Object(Some(o)) => Some((ctx.pin_native_root(*o), *o)),
                    _ => None,
                })
                .collect();
            let current_args = |ctx: &dyn NativeContext| -> Vec<Value> {
                args.iter()
                    .zip(&pinned)
                    .map(|(v, pin)| match pin {
                        Some((h, o)) => Value::Object(Some(ctx.read_native_pin(*h, *o))),
                        None => *v,
                    })
                    .collect()
            };
            // Every arm releases the argument pins before it returns: the
            // body runs as one closure and the pins drop to their floor after
            // it (interpreter round i2 wave 62, lane L4; the funnel's
            // truncation is a backstop, and `[pin-balance] LEAK` reported this
            // native leaving four).
            let pin_floor = pinned.iter().flatten().map(|(h, _)| *h).min();
            let result = (|| -> MethodCallResult {
                let mut path_obj = obj_arg(args, 1)?;
                let p = p57_read_path(ctx, &mut path_obj);
                // Jar-filesystem entries are not real OS files (`fd_table` cannot
                // open them), so keep the in-memory snapshot path for them. (These
                // remain read-only via the methodless stub — unchanged behaviour.)
                if let Some(read) = vfs_read(&p) {
                    return match read {
                        Ok(data) => {
                            // Prefer a real FileChannelImpl over the entry's
                            // bytes (see `vfs_entry_as_real_channel`) — its
                            // read/size/position/close resolve to real bytecode.
                            // Fall back to the in-memory `P57_VFS_CHANNEL_SLOTS`
                            // channel below only if the temp-file spill itself
                            // fails (e.g. disk full); that fallback is ALSO
                            // natively backed (`register_vfs_seekable_byte_channel_natives`),
                            // not the old methodless interface stub, so a
                            // fd/disk problem degrades to a working read-only
                            // channel rather than to `AbstractMethodError`.
                            if let Some(real) = vfs_entry_as_real_channel(ctx, &data) {
                                return Ok(Some(real));
                            }
                            // `P57_VFS_CHANNEL_SLOTS`: 0=data, 1=position, 2=size,
                            // 3=closed. See that constant's own doc for why this
                            // is allocated AS the bare `SeekableByteChannel`
                            // interface and what makes that safe now.
                            let channel = try_alloc_concurrent_synthetic(
                                ctx,
                                "java/nio/channels/SeekableByteChannel",
                                P57_VFS_CHANNEL_SLOTS,
                            )?;
                            // Pin across the array alloc below — a moving young GC
                            // there would relocate the fresh channel (native
                            // stale-local family).
                            let channel_pin = ctx.pin_native_root(channel);
                            use cratonvm_types::ArrayElementType;
                            let arr = ctx.new_array(ArrayElementType::Byte, data.len());
                            ctx.write_byte_array_from(arr, 0, &data);
                            let channel = ctx.read_native_pin(channel_pin, channel);
                            ctx.unpin_native_roots(channel_pin);
                            ctx.set_field(channel, 0, Value::Object(Some(arr))); // data
                            ctx.set_field(channel, 1, Value::Int(0)); // position
                            ctx.set_field(channel, 2, Value::Int(data.len() as i32)); // size
                            ctx.set_field(channel, 3, Value::Int(0)); // closed
                            Ok(Some(Value::Object(Some(channel))))
                        }
                        // NIO contract: a missing file must surface as
                        // `java.nio.file.NoSuchFileException`, NOT a bare `IOException`.
                        // Frameworks treat config sources as OPTIONAL by catching
                        // NoSuchFileException (e.g. SmallRye Config loading Keycloak's
                        // profile-specific `keycloak-<profile>.conf`); a generic
                        // IOException escapes that catch and aborts boot (SRCFG00035).
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            Err(p57_no_such_file(ctx, &p)?)
                        }
                        Err(e) => Err(p57_io_error(&e)),
                    };
                }
                // NOFOLLOW_LINKS has to be answered BEFORE the missing-file
                // pre-check below, because the two disagree about a *dangling*
                // symlink: `Path::exists()` is a `stat`, so it reports the link as
                // absent, and without this the pre-check would return
                // `NoSuchFileException` — which callers legitimately catch and
                // recover from — where the kernel's `O_NOFOLLOW` would have said
                // `ELOOP`. The delegation to `newFileChannel` below carries the same
                // check for every other case; this one has to be here.
                let opts_now = current_args(&*ctx).get(2).copied();
                let open_opts = fsp_scan_open_options(ctx, opts_now);
                if open_opts.nofollow {
                    if let Some(refused) = p57_nofollow_reject(&p) {
                        return Err(refused);
                    }
                }
                // CREATE_NEW MEANS FAIL IF IT EXISTS, and this door did not check.
                // `fsp_new_output_stream` has the check; the real
                // `FileSystemProvider.newOutputStream` bytecode does not call that
                // native, it calls THIS one — so the guarantee held only while our
                // own `newOutputStream` answered:
                //
                //   Files.createFile(<existing>)                 no throw, was FAEE
                //   Files.newOutputStream(<existing>, CREATE_NEW) no throw, was FAEE
                //   Files.copy(in, <existing>)                    no throw, was FAEE
                //
                // Three rows, two dial scopes, one missing check. CREATE_NEW is the
                // option callers use to mean "I must be the one who creates this" —
                // an exclusive-create that quietly opens the existing file instead
                // is a lost-update, not a wrong exception.
                //
                // MEASURED with the shadow dial armed over
                // `java/nio/file/spi/FileSystemProvider` and `java/nio/file/Files`.
                if open_opts.create_new && std::path::Path::new(&p).exists() {
                    return Err(p57_file_already_exists(ctx, &p));
                }
                // NOT REFUSED HERE, and the attempt to is worth recording.
                //
                // `Files.readAllBytes(<a directory>)` raises `OutOfMemoryError` on
                // this VM where HotSpot raises `IOException` — found with the dial
                // armed over `java/nio/file/Files`. Refusing a directory at open
                // looked like the fix and is wrong twice over:
                //
                //   open READ  on a dir   Linux ALLOWS it; the failure is EISDIR at
                //                         the first read, which is why HotSpot's
                //                         answer is a plain `IOException`
                //   open WRITE on a dir   already correct here — HotSpot raises
                //                         `java.nio.file.FileSystemException` and so
                //                         did this VM, until a blanket refusal
                //                         downgraded it to a bare `IOException`
                //
                // That blanket check turned a green row red (`L4FilesSweep`,
                // "newByteChannel on dir for write") — the same right-behaviour
                // wrong-type defect this same commit fixes three of. The real
                // source of the OOME is whatever `readAllBytes` sizes its buffer
                // from, one layer up, and it is left OPEN rather than papered over
                // from here.
                // Preserve the NIO missing-file contract: opening a non-existent
                // path for READ — or for WRITE without CREATE/CREATE_NEW — must throw
                // `java.nio.file.NoSuchFileException`, which frameworks catch to treat
                // a config source as OPTIONAL (SmallRye loading Keycloak's
                // `keycloak-<profile>.conf`; SRCFG00035). `newFileChannel`'s fd open
                // would surface a generic `IOException` here, so pre-check.
                if !std::path::Path::new(&p).exists() {
                    let creates = match current_args(&*ctx).get(2).copied() {
                        Some(Value::Object(Some(set))) => ctx
                            .invoke_virtual(set, "toString", "()Ljava/lang/String;", &[])
                            .ok()
                            .flatten()
                            .and_then(|v| match v {
                                Value::Object(Some(s)) => ctx.read_string(s),
                                _ => None,
                            })
                            .map(|s| s.contains("CREATE"))
                            .unwrap_or(false),
                        _ => false,
                    };
                    if !creates {
                        return Err(p57_no_such_file(ctx, &p)?);
                    }
                }
                // Real file: return a working `FileChannel` (which implements
                // `SeekableByteChannel`) so `read`/`write`/`position`/`size`/`close`
                // resolve to the fd_table-backed `FileChannel` natives. Previously
                // this returned a synthetic object allocated AS the bare
                // `SeekableByteChannel` interface with no method bodies, so
                // `channel.write(buf)` / `channel.read(buf)` dispatched to the
                // abstract interface method → `AbstractMethodError:
                // WritableByteChannel.write … has no Code attribute`
                // (SC-resource-io-family Cause A). Delegate to the sibling
                // `newFileChannel` shim, which parses the `OpenOption` Set
                // (READ/WRITE/APPEND/CREATE/TRUNCATE_EXISTING) and opens the fd.
                // NB: inline the class-name literal (not the `fsp` binding) so this
                // closure stays non-capturing — `r.register` takes a bare `fn`.
                let now = current_args(&*ctx);
                ctx.invoke(
                    "java/nio/file/spi/FileSystemProvider",
                    "newFileChannel",
                    "(Ljava/nio/file/Path;Ljava/util/Set;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/channels/FileChannel;",
                    &now,
                )
            })();
            if let Some(floor) = pin_floor {
                ctx.unpin_native_roots(floor);            }
            result
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    register_vfs_seekable_byte_channel_natives(r);

    r.register(
        fsp,
        "newInputStream",
        "(Ljava/nio/file/Path;[Ljava/nio/file/OpenOption;)Ljava/io/InputStream;",
        // args[0] = this (the provider), args[1] = the Path.
        |ctx, args| fsp_new_input_stream(ctx, args, 1), cratonvm_native_api::NativeKind::Bridge,
    );

    // FileSystemProvider.newOutputStream — JDK's default impl at
    // `FileSystemProvider.java:426` calls `newByteChannel(path, opts)` which
    // our minimal `newByteChannel` registration handles as READ-ONLY via
    // `std::fs::read(&p)`. For a NEW file (the whole point of an output
    // stream) `std::fs::read` ENOENTs, the channel allocation fails, and the
    // caller sees `IOException: file not found`. This breaks any code that
    // uses `Files.newOutputStream` / `Files.write(Path, byte[])` to *create*
    // a file — most prominently Felix's `BundleArchive.writeBundleInfo`,
    // which is invoked once per installed bundle. Without it the entire
    // Felix auto-deploy directory fails to install (every `installBundle`
    // throws `BundleException: Unable to cache bundle`), no Gogo shell
    // bundles activate, no shell-reader thread spawns, and `felix.jar` hangs
    // forever in `Felix.waitForStop(0)` because nothing ever calls
    // `framework.stop()`.
    //
    // The fix: register `newOutputStream` directly so we never fall through
    // to the JDK default. We open the path for writing through `fd_table`
    // (honouring APPEND/CREATE/etc. via `open_options_*` helpers below) and
    // hand back a real `java.io.FileOutputStream` instance with the fd
    // wired onto its `FileDescriptor` — that way every downstream
    // `write`/`flush`/`close` native (already registered against
    // `java/io/FileOutputStream`) just works without any new shim type.
    r.register(
        fsp,
        "newOutputStream",
        "(Ljava/nio/file/Path;[Ljava/nio/file/OpenOption;)Ljava/io/OutputStream;",
        |ctx, args| fsp_new_output_stream(ctx, args), cratonvm_native_api::NativeKind::Bridge,
    );

    // `checkAccess` is `Files.isReadable`/`isWritable`/`isExecutable`'s only
    // implementation — those three have no `Files`-level native, so their real
    // JDK bytecode calls straight through to here. This used to answer "does
    // the path exist?" and ignore the `AccessMode` array entirely, so
    // `Files.isExecutable` said `true` for a plain 0644 file on Linux (HotSpot:
    // `false`). `fs_check_access` is the same `access(2)` call `File.canWrite`
    // already routes through — one implementation for both entry points.
    r.register(
        fsp,
        "checkAccess",
        "(Ljava/nio/file/Path;[Ljava/nio/file/AccessMode;)V",
        |ctx, args| {
            retired_receiver_yield!(
                ctx,
                args,
                "checkAccess",
                "(Ljava/nio/file/Path;[Ljava/nio/file/AccessMode;)V"
            );
            // The AccessMode array is read from `args` after the Path.
            let (p, args) = p57_read_path_arg(ctx, args, 1)?;
            let args = &*args;
            let exists = match vfs_classify(&p) {
                Some(kind) => !matches!(kind, JarFsKind::Absent),
                None => std::path::Path::new(&p).exists(),
            };
            if !exists {
                // THE TYPE, not a message naming the type. This threw a bare
                // `IOException` whose text was "NoSuchFileException: <path>",
                // and `Files.createDirectories` walks up its parents with
                //
                //     try { provider(parent).checkAccess(parent); break; }
                //     catch (NoSuchFileException x) { }
                //
                // A bare `IOException` is not caught by that, so it escaped the
                // walk and `createDirectories("a/b")` failed outright naming
                // the PARENT it was probing:
                //
                //   Files.createDirectories("lvl1/lvl2")
                //     HotSpot   creates both
                //     this VM   IOException: NoSuchFileException: <cwd>/lvl1
                //
                // Third time this lane has found a refusal with the right
                // message and the wrong class (`Buffer.reset`'s
                // `InvalidMarkException`, `Files.copy`'s `IllegalStateException`).
                // `p57_no_such_file` has existed the whole time; this site just
                // did not call it.
                //
                // MEASURED with the shadow dial armed over `java/nio/file/Files`,
                // which is what makes the real `createDirectories` bytecode run
                // instead of our own native -- but the wrong type is thrown in
                // BOTH modes and any caller doing `catch (NoSuchFileException)`
                // misses it unarmed too.
                return Err(p57_no_such_file(ctx, &p)?);
            }
            // A jar-FS entry has no host permissions to consult; existence is
            // the whole of what the archive can answer.
            if vfs_classify(&p).is_some() {
                return Ok(None);
            }
            for mode in access_modes_requested(ctx, args.get(2)) {
                if !fs_check_access(&p, mode) {
                    return Err(p57_access_denied(ctx, &p)?);                }            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        fsp,
        "readAttributes",
        "(Ljava/nio/file/Path;Ljava/lang/Class;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/BasicFileAttributes;",
        |ctx, args| {
            retired_receiver_yield!(
                ctx,
                args,
                "readAttributes",
                "(Ljava/nio/file/Path;Ljava/lang/Class;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/BasicFileAttributes;"
            );
            #[cfg(windows)]
            {
                // See the `Files.readAttributes` registration: only a PRESENT,
                // NAMEABLE `Class` argument states a requested type. Absent or
                // unresolvable means "no request" and falls back to the
                // declared return type, `BasicFileAttributes`.
                if let Some(Value::Object(Some(class))) = args.get(2) {
                    let requested_type =
                        crate::lang_class::mirror_class_name(ctx, *class).unwrap_or_default();
                    if !requested_type.is_empty()
                        && !windows_supports_file_attributes_type(&requested_type)
                    {
                        return Err(RuntimeError::UnsupportedOperationException {
                            message: format!(
                                "File attribute type {requested_type} is not supported on Windows"
                            ),
                        }
                        .into());
                    }
                }
            }
            // A NULL `Class` IS AN NPE. The JDK reaches
            // `type == BasicFileAttributes.class` and then `type ==
            // PosixFileAttributes.class` before falling through to
            // `throw new UnsupportedOperationException()`, and every route
            // there dereferences the argument -- `readAttributes(p, null)`
            // raises `NullPointerException`, it does not quietly answer the
            // basic view.
            //
            // Answering something for a caller who asked for nothing is the
            // fabricated-success shape this lane keeps finding: the caller gets
            // attributes it never requested a type for, and learns nothing
            // about its own bug.
            //
            // The `#[cfg(windows)]` block above deliberately treats an ABSENT
            // or UNNAMEABLE class as "no request"; an EXPLICIT null is a
            // different thing and is refused here. MEASURED with the dial armed
            // over `java/nio/file/Files`.
            if matches!(args.get(2), Some(Value::Object(None))) {
                return Err(RuntimeError::NullPointerException { message: None }.into());
            }
            let path_obj = obj_arg(args, 1)?;
            let options = args.get(3).copied().unwrap_or(Value::Object(None));
            let attrs =
                p59_files_read_attributes(ctx, &[Value::Object(Some(path_obj)), options]);
            // Same narrowing as the `Files` entry point above; this is the one
            // `Files.readAttributes` routes through on a real provider.
            let requested = args.get(2).copied();
            narrow_to_basic_view(ctx, requested.as_ref(), attrs)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // `FileSystemProvider.readAttributes(Path, String, LinkOption...)` — the
    // name-keyed sibling of the `Class`-keyed form above.
    //
    // In the real JDK this is implemented on `sun.nio.fs.AbstractFileSystemProvider`
    // and every concrete provider inherits it; `java.nio.file.spi.FileSystemProvider`
    // itself only declares it, abstract. CratonVM's default-filesystem provider
    // object is stamped with that abstract class (`FileSystems.getDefault()
    // .provider()` reports a concrete name only through `getClass()` display
    // remapping), so real-JDK bytecode calling this method resolved the abstract
    // declaration and died with
    // `AbstractMethodError: ... readAttributes ... has no Code attribute`.
    //
    // That is not a corner: `Files.readAttributes(path, "basic:*")`,
    // `Files.getAttribute`, and everything layered on them route here. It is
    // also why `com.sun.tools.attach.VirtualMachine.list()` threw
    // `InternalError` — jvmstat's `PlatformSupportImpl` reads `unix:dev` on the
    // temp directory during container detection, and the `AbstractMethodError`
    // came back out wrapped two deep.
    //
    // Args: `[this, path, attributes, options]`.
}
