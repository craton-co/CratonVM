#[allow(unused_imports)]
use super::*;

pub fn register_p70_file_attributes(r: &mut NativeMethodRegistry) {
    // FileTime — millis stored in the real `long value` field (by name) so
    // descriptor coercion preserves it; see `filetime_alloc`.
    let ft = "java/nio/file/attribute/FileTime";
    r.register(
        ft,
        "fromMillis",
        "(J)Ljava/nio/file/attribute/FileTime;",
        |ctx, args| {
            let millis = match args.first() {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let obj = filetime_alloc(ctx, millis)?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ft,
        "from",
        "(JLjava/util/concurrent/TimeUnit;)Ljava/nio/file/attribute/FileTime;",
        |ctx, args| {
            let val = match args.first() {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            // Honor the TimeUnit (was ignored → SECONDS/etc. stored as raw
            // millis → wrong dates). Convert to millis via unit.toMillis(val).
            let millis = match args.get(1) {
                Some(Value::Object(Some(unit))) => {
                    match ctx.invoke_virtual(*unit, "toMillis", "(J)J", &[Value::Long(val)])? {
                        Some(Value::Long(m)) => m,
                        _ => val,
                    }
                }
                _ => val,
            };
            let obj = filetime_alloc(ctx, millis)?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ft,
        "from",
        "(Ljava/time/Instant;)Ljava/nio/file/attribute/FileTime;",
        |ctx, args| {
            // Preserve the Instant's value (was dropped → stored 0, so every
            // FileTime.from(Instant) round-tripped to epoch 0). Read the real
            // java.time.Instant's named fields (seconds/nanos).
            //
            // A NULL Instant is an NPE (`Objects.requireNonNull(instant)`), not
            // a FileTime of epoch 0 — which is a real timestamp, and one that
            // compares, formats and sorts like any other. MEASURED with
            // `L4CensusTail.java`.
            if matches!(args.first(), Some(Value::Object(None))) {
                return Err(RuntimeError::NullPointerException { message: None }.into());
            }
            let millis = match args.first() {
                Some(Value::Object(Some(inst))) => {
                    let secs = match ctx.get_field_by_name(*inst, "seconds") {
                        Value::Long(v) => v,
                        _ => 0,
                    };
                    let nanos = match ctx.get_field_by_name(*inst, "nanos") {
                        Value::Int(v) => v,
                        _ => 0,
                    };
                    secs.saturating_mul(1000) + (nanos as i64) / 1_000_000                }
                _ => 0,
            };
            let obj = filetime_alloc(ctx, millis)?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(ft, "toMillis", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Long(filetime_read_millis(ctx, this))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ft, "toInstant", "()Ljava/time/Instant;", |ctx, args| {
        // Was hardcoded to null → every `FileTime.toInstant().getEpochSecond()`
        // NPE'd (Spring Boot buildpack tar-layer timestamps). Build a real
        // java.time.Instant from the stored millis via its real factory.
        let this = obj_arg(args, 0)?;
        let millis = filetime_read_millis(ctx, this);
        ctx.invoke(
            "java/time/Instant",
            "ofEpochMilli",
            "(J)Ljava/time/Instant;",
            &[Value::Long(millis)],
        )
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        ft,
        "compareTo",
        "(Ljava/nio/file/attribute/FileTime;)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let a = filetime_read_millis(ctx, this);
            let b = filetime_read_millis(ctx, other);
            Ok(Some(Value::Int(a.cmp(&b) as i32)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(ft, "toString", "()Ljava/lang/String;", |ctx, args| {
        // Was "{millis}ms"; real FileTime.toString is the ISO-8601 instant.
        let this = obj_arg(args, 0)?;
        let millis = filetime_read_millis(ctx, this);
        let inst = match ctx.invoke(
            "java/time/Instant",
            "ofEpochMilli",
            "(J)Ljava/time/Instant;",
            &[Value::Long(millis)],
        )? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                let s = ctx.create_string("");
                return Ok(Some(Value::Object(Some(s))));
            }
        };
        ctx.invoke_virtual(inst, "toString", "()Ljava/lang/String;", &[])
    }, cratonvm_native_api::NativeKind::Bridge);

    // PosixFilePermission.values() — REMOVED here, and this note is the whole
    // reason to read the code rather than the comment. It was:
    //
    //     let arr = ctx.new_array(Reference, 9);
    //     // Can't iterate/capture — just return the array (elements are null
    //     // but array exists)
    //
    // i.e. nine NULLS with the difficulty stated as the excuse. Anything that
    // walked it — `EnumSet.allOf`, `Class.getEnumConstants`, a `for (var p :
    // values())` loop — got nine nulls, and every `p.name()` on one NPEs. The
    // replacement is `posix_file_permission_values`, registered from
    // `register_posix_file_permission_stub_clinit` at the bottom of this
    // function alongside `valueOf`; it reads the nine statics, so it answers
    // in BOTH modes. Both rows named the same triple, so this was also a
    // last-write-wins race that the fabricated one only lost by ordering.

    // PosixFilePermissions utility
    let pfps = "java/nio/file/attribute/PosixFilePermissions";
    // `PFP_PAT` is positionally paired with `POSIX_FILE_PERMISSION_CONSTANTS`:
    // the rwx char expected at each position of a canonical "rwxrwxrwx"
    // string. The name list used to be a third private copy here (after the
    // `<clinit>`'s and `posix_permission_bits_from_set`'s); it is now the one
    // shared constant, so the pairing cannot drift.
    const PFP_PAT: [char; 9] = ['r', 'w', 'x', 'r', 'w', 'x', 'r', 'w', 'x'];
    r.register(
        pfps,
        "toString",
        "(Ljava/util/Set;)Ljava/lang/String;",
        |ctx, args| {
            // Was a stub that always returned "rwxr-xr-x". Build the real
            // "rwxrwxrwx" string from the actual set membership.
            let set = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(Some(ctx.create_string("---------"))))),
            };
            let pfp = POSIX_FILE_PERMISSION;
            let _ = ctx.ensure_class_initialized(pfp);
            let cid = ctx.class_id_by_name(pfp);
            let mut out = String::with_capacity(9);
            // GC-safety: `Set.contains` below is a virtual dispatch into the
            // caller's own set, run once per permission constant, and `set` is
            // carried in from outside the loop.
            let set_pin = ctx.pin_native_root(set);
            for i in 0..9 {
                let set = ctx.read_native_pin(set_pin, set);
                let present = if let Some(c) = cid {
                    match ctx.static_field_index_by_name(c, POSIX_FILE_PERMISSION_CONSTANTS[i]) {
                        Some(slot) => {
                            let constant = ctx.get_static_field(c, slot);
                            matches!(
                                ctx.invoke_virtual(
                                    set,
                                    "contains",
                                    "(Ljava/lang/Object;)Z",
                                    &[constant]
                                ),
                                Ok(Some(Value::Int(1)))
                            )
                        }
                        None => false,
                    }
                } else {
                    false
                };
                out.push(if present { PFP_PAT[i] } else { '-' });
            }
            Ok(Some(Value::Object(Some(ctx.create_string(&out)))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        pfps,
        "fromString",
        "(Ljava/lang/String;)Ljava/util/Set;",
        |ctx, args| {
            // Was a stub returning an EMPTY set (ignored the input). Parse the
            // real "rwxrwxrwx" permission string into a HashSet of the singleton
            // PosixFilePermission constants.
            let perms = match args.first() {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let set = match ctx.new_object("java/util/HashSet") {
                Ok(Some(Value::Object(Some(o)))) => o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let _ = ctx.invoke(
                "java/util/HashSet",
                "<init>",
                "()V",
                &[Value::Object(Some(set))],
            );
            let chars: Vec<char> = perms.chars().collect();
            let pfp = POSIX_FILE_PERMISSION;
            // Pin across the clinit / add() invokes below — a moving young GC
            // there would relocate the fresh set (native stale-local family).
            let set_pin = ctx.pin_native_root(set);
            let _ = ctx.ensure_class_initialized(pfp);
            let cid = ctx.class_id_by_name(pfp);
            for i in 0..9 {
                if chars.get(i).copied() == Some(PFP_PAT[i]) {
                    if let Some(c) = cid {
                        if let Some(slot) =
                            ctx.static_field_index_by_name(c, POSIX_FILE_PERMISSION_CONSTANTS[i])
                        {
                            let constant = ctx.get_static_field(c, slot);
                            if matches!(constant, Value::Object(Some(_))) {
                                let set = ctx.read_native_pin(set_pin, set);
                                let _ = ctx.invoke_virtual(
                                    set,
                                    "add",
                                    "(Ljava/lang/Object;)Z",
                                    &[constant],
                                );
                            }
                        }
                    }
                }
            }
            let set = ctx.read_native_pin(set_pin, set);
            ctx.unpin_native_roots(set_pin);
            Ok(Some(Value::Object(Some(set))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // Bridges NSM when the method is missing from the resolved class table.
    // This native SHADOWS the real JDK's anonymous-class implementation even
    // in real-JDK mode, so returning `null` here (the previous behaviour) was
    // not a harmless fallback: it silently erased the requested mode from
    // every `Files.createDirectory(dir, PosixFilePermissions
    // .asFileAttribute(<700>))` call, which then created the directory with
    // the process umask (0755). Spring Boot's `ApplicationTemp` rejected its
    // OWN temp directory on the next call ("Existing directory ... does not
    // have the permissions [OWNER_READ, OWNER_WRITE, OWNER_EXECUTE]") —
    // `AbstractServletWebServerFactoryTests#persistSession` /
    // `#getValidSessionStoreWhenSessionStoreNotSet`. Hand back a two-field
    // synthetic carrier instead; `posix_mode_from_file_attributes` reads it,
    // and `name()`/`value()` below cover anyone who calls the interface.
    r.register(
        pfps,
        "asFileAttribute",
        "(Ljava/util/Set;)Ljava/nio/file/attribute/FileAttribute;",
        |ctx, args| {
            // JVMS 6.5 (nio21 close): over a real JDK the JDK's own body answers
            // an instance of its anonymous `PosixFilePermissions$1` (a class
            // `new` could produce, HotSpot's answer, with its `Set.copyOf`-style
            // copy and null check), and `posix_mode_from_file_attributes` reads
            // it through `name()` / `value()` below. The two-field carrier is
            // now the synthetic image's alone.
            if !ctx.is_class_synthetic_stub("java/nio/file/attribute/PosixFilePermissions") {
                return ctx.invoke_special_bytecode_only(
                    "java/nio/file/attribute/PosixFilePermissions",
                    "asFileAttribute",
                    "(Ljava/util/Set;)Ljava/nio/file/attribute/FileAttribute;",
                    args,
                );
            }
            let set = match args.first() {
                Some(Value::Object(Some(s))) => *s,
                _ => return Ok(Some(Value::Object(None))),
            };
            // Pin across the alloc/create_string below — a moving young GC
            // there would relocate them (native stale-local family).
            //
            // gc-common w20-d: rooted to the END. Both pins were released
            // right after the name String, before the copy's allocation and
            // `LinkedHashSet.<init>` (Java): the set was handed to `<init>`
            // and the attribute written and returned through their addresses
            // from before them.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let set_h = scope.root(set);
            let fa = try_alloc_concurrent_synthetic(&mut *scope, FILE_ATTRIBUTE_CLASS, 2)?;
            let fa_h = scope.root(fa);
            let name = scope.create_string("posix:permissions");
            let fa = scope.get(&fa_h);
            scope.set_field(fa, 0, Value::Object(Some(name)));
            // A COPY, not the caller's set. The JDK's `asFileAttribute` closes
            // over `Set.copyOf(perms)`, so the attribute's value is fixed at
            // the moment it is built. Storing the caller's own set means a
            // later `perms.add(...)` retroactively changes the mode an already
            // constructed attribute will request -- and, because
            // `Files.createFile(p, attr)` reads the value at creation time, the
            // change lands on a file created afterwards with no visible cause.
            //
            // MEASURED with `L4BridgeSweep.java`
            // (`fa.value() == perms` -- HotSpot false, this VM true).
            //
            // If the copy cannot be made, keep the original: an aliased value
            // is worse than a fresh one but far better than a null attribute.
            let stored = match scope.new_object("java/util/LinkedHashSet") {
                Ok(Some(Value::Object(Some(copy)))) => {
                    let copy_h = scope.root(copy);
                    let set = scope.get(&set_h);
                    let ok = scope
                        .invoke(
                            "java/util/LinkedHashSet",
                            "<init>",
                            "(Ljava/util/Collection;)V",
                            &[Value::Object(Some(copy)), Value::Object(Some(set))],
                        )
                        .is_ok();
                    let copy = scope.get(&copy_h);
                    if ok {
                        copy
                    } else {
                        scope.get(&set_h)
                    }
                }
                _ => scope.get(&set_h),
            };
            let fa = scope.get(&fa_h);
            scope.set_field(fa, 1, Value::Object(Some(stored)));
            Ok(Some(Value::Object(Some(fa))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        FILE_ATTRIBUTE_CLASS,
        "name",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        FILE_ATTRIBUTE_CLASS,
        "value",
        "()Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    register_posix_file_permission_stub_clinit(r);
}


// =============================================================================
// Files stream/IO bridge
// =============================================================================

pub fn register_p71_files_bridge(r: &mut NativeMethodRegistry) {
            let f = "java/nio/file/Files";

    // Files.newInputStream — a lazy stream over the file. NB: this is the
    // THIRD registration of this exact (class, name, descriptor); registration
    // is last-writer-wins, so whichever of them runs last is the one that
    // dispatches. They all route to the same helper now, which is why that no
    // longer matters.
    r.register(
        f,
        "newInputStream",
        "(Ljava/nio/file/Path;[Ljava/nio/file/OpenOption;)Ljava/io/InputStream;",
        // args[0] = the Path (static method).
        |ctx, args| fsp_new_input_stream(ctx, args, 0), cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        f,
        "readAllBytes",
        "(Ljava/nio/file/Path;)[B",
        |ctx, args| {
            let p = p57_read_path(ctx, &mut obj_arg(args, 0)?);
            match std::fs::read(&p) {
                Ok(bytes) => {
                    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
                    for (i, &b) in bytes.iter().enumerate() {
                        ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
                    }
                    Ok(Some(Value::Object(Some(arr))))
                }
                // NIO contract: missing file → NoSuchFileException, not a bare
                // IOException/IllegalStateException (see newByteChannel above).
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Err(p57_no_such_file(ctx, &p)?)
                }
                Err(e) => Err(RuntimeError::IllegalStateException {
                    message: format!("IOException: {}", e),
                }
                .into()),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        f,
        "write",
        "(Ljava/nio/file/Path;[B[Ljava/nio/file/OpenOption;)Ljava/nio/file/Path;",
        files_write_bytes_impl, cratonvm_native_api::NativeKind::Bridge,
    );
    // Both `Files.copy` stream overloads returned 0 without moving a single
    // byte. The caller was told "copied 0 bytes" — a legal-looking answer for
    // an empty source — so an unpacked resource, a saved upload, or a streamed
    // download silently produced nothing, and no exception ever surfaced. They
    // now perform the copy and report the real byte count.
    r.register(
        f,
        "copy",
        "(Ljava/io/InputStream;Ljava/nio/file/Path;[Ljava/nio/file/CopyOption;)J",
        |ctx, args| {
            let src = match args.first() {
                Some(Value::Object(Some(s))) => *s,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("Files.copy: source stream is null".into()),
                    }
                    .into())
                }
            };
            let target = obj_arg(args, 1)?;
            // gc-common w21-b: the stream, the target and the option array are
            // rooted at ENTRY. Each option probe below calls `toString()` on
            // the options (Java), so the stream was drained, the second probe
            // scanned the array, and `target` was pinned, all through their
            // entry addresses -- `args` is a snapshot no collection rewrites.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let src_h = scope.root(src);
            let target_h = scope.root(target);
            let opts_h = match args.get(2) {
                Some(Value::Object(Some(o))) => Some(scope.root(*o)),
                _ => None,
            };
            let opts = |scope: &cratonvm_native_api::NativeHandleScope<'_>| {
                opts_h.as_ref().map(|h| Value::Object(Some(scope.get(h))))
            };
            // The option list is refused BEFORE the source is drained, exactly
            // as the JDK refuses it before opening anything: an unsupported
            // option must not consume the caller's stream on its way to
            // throwing. Every option but REPLACE_EXISTING is unsupported here.
            let o = opts(&scope);
            if let Some(bad) = copy_options_unsupported(&mut *scope, o.as_ref()) {
                return Err(RuntimeError::UnsupportedOperationException {
                    message: format!("{bad} not supported"),
                }
                .into());
            }
            let o = opts(&scope);
            let replace = copy_options_replace_existing(&mut *scope, o.as_ref());
            let src = scope.get(&src_h);
            let bytes = zip_streams::drain_input_stream_bulk(&mut *scope, src);
            let mut target = scope.get(&target_h);
            let p = p57_read_path(&mut *scope, &mut target);
            drop(scope);
            // NIO contract: without REPLACE_EXISTING an existing target is a
            // FileAlreadyExistsException, not an overwrite.
            if !replace && std::fs::symlink_metadata(&p).is_ok() {
                return Err(p57_file_already_exists(ctx, &p));
            }
            match std::fs::write(&p, &bytes) {
                Ok(()) => Ok(Some(Value::Long(bytes.len() as i64))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Err(p57_no_such_file(ctx, &p)?)
                }
                Err(e) => Err(p57_io_error(&e)),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        f,
        "copy",
        "(Ljava/nio/file/Path;Ljava/io/OutputStream;)J",
        |ctx, args| {
            let mut src = obj_arg(args, 0)?;
            let out = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("Files.copy: target stream is null".into()),
                    }
                    .into())
                }
            };
            // gc-common w21-b: the sink is pinned from BEFORE the Path read (a
            // foreign Path's `toString()` is Java) to the `write` call; it
            // used to be pinned only after the read, at its entry address.
            let out_pin = ctx.pin_native_root(out);
            let p = p57_read_path(ctx, &mut src);
            let bytes = match std::fs::read(&p) {
                Ok(b) => b,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    ctx.unpin_native_roots(out_pin);
                    return Err(p57_no_such_file(ctx, &p)?);
                }
                Err(e) => {
                    ctx.unpin_native_roots(out_pin);
                    return Err(p57_io_error(&e));
                }
            };
            // Hand the whole payload over in ONE bulk `write([BII)` rather
            // than a per-byte loop; the sink stays pinned across the array.
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
            ctx.write_byte_array_from(arr, 0, &bytes);
            let out = ctx.read_native_pin(out_pin, out);
            let res = ctx.invoke_virtual(
                out,
                "write",
                "([BII)V",
                &[
                    Value::Object(Some(arr)),
                    Value::Int(0),
                    Value::Int(bytes.len() as i32),
                ],
            );
            ctx.unpin_native_roots(out_pin);
            res?;
            Ok(Some(Value::Long(bytes.len() as i64)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `Files.readAttributes(path, "view:attrs", options)` — the name-keyed form.
    //
    // This used to hand back an EMPTY HashMap for every call, which is the
    // shape of answer that is worse than an exception: the contract is that a
    // requested attribute is present or the call throws
    // (`IllegalArgumentException` for an unknown name,
    // `UnsupportedOperationException` for an unknown view), so every caller
    // dereferences `map.get(name)` unconditionally and got `null`.
    // `Files.getAttribute` is a thin wrapper over exactly this and returned
    // null for every attribute of every file.
    //
    // Args (static): `[path, attributes, options]`.
    r.register(
        f,
        "readAttributes",
        "(Ljava/nio/file/Path;Ljava/lang/String;[Ljava/nio/file/LinkOption;)Ljava/util/Map;",
        |ctx, args| {
            let path_obj = obj_arg(args, 0)?;
            let spec = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let nofollow =
                matches!(args.get(2), Some(Value::Object(Some(a))) if ctx.array_length(*a) > 0);
            read_named_attributes(ctx, path_obj, &spec, nofollow)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `Files.getAttribute(path, "view:name", options)` — one attribute, by name.
    //
    // The real body is `readAttributes(path, name, options).get(name)`; register
    // it explicitly so synthetic-JDK mode (where there is no `Files` bytecode to
    // run) answers the same as real-JDK mode rather than falling through to a
    // missing method.
    r.register(
        f,
        "getAttribute",
        "(Ljava/nio/file/Path;Ljava/lang/String;[Ljava/nio/file/LinkOption;)Ljava/lang/Object;",
        |ctx, args| {
            let path_obj = obj_arg(args, 0)?;
            let spec = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let nofollow =
                matches!(args.get(2), Some(Value::Object(Some(a))) if ctx.array_length(*a) > 0);
            let (_, attr) = split_attribute_spec(&spec);
            // `getAttribute` takes a SINGLE name; `*` and comma lists are only
            // legal in `readAttributes`.
            if attr == "*" || attr.contains(',') {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("'{spec}' not recognized"),
                }
                .into());
            }
            let map = read_named_attributes(ctx, path_obj, &spec, nofollow)?;
            let map_obj = match map {
                Some(Value::Object(Some(m))) => m,
                _ => return Ok(Some(Value::Object(None))),
            };
            let map_pin = ctx.pin_native_root(map_obj);
            let key = ctx.create_string(attr);
            let map_obj = ctx.read_native_pin(map_pin, map_obj);
            let got = ctx.invoke_virtual(
                map_obj,
                "get",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[Value::Object(Some(key))],
            );
            ctx.unpin_native_roots(map_pin);
            Ok(Some(got?.unwrap_or(Value::Object(None))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // Was null. `Files.getFileStore` never returns null in the real JDK, so
    // every caller dereferences the result unconditionally (ES's `ESFileStore`
    // wrapper does) — a guaranteed NPE. Hand back the same synthetic
    // `FileStore` the `FileSystemProvider.getFileStore` native above builds.
    r.register(
        f,
        "getFileStore",
        "(Ljava/nio/file/Path;)Ljava/nio/file/FileStore;",
        |ctx, args| {
            let mut path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, &mut path_obj);
            // NIO contract: the path must exist.
            if std::fs::symlink_metadata(&p).is_err() {
                return Err(p57_no_such_file(ctx, &p)?);
            }
            Ok(Some(Value::Object(Some(p57_alloc_file_store(ctx, &p)?))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // Was `null`. `Files.getOwner` is specified to return a principal or throw
    // (`UnsupportedOperationException` when the path's provider has no
    // `FileOwnerAttributeView`, `IOException` on failure) — never null — so
    // every caller dereferences it. Same delegation as
    // `PosixFileAttributeView.getOwner`: the real
    // `UnixFileAttributes.owner()` bytecode turns the `st_uid` we record into a
    // proper `UnixUserPrincipal` via the wired `getpwuid` native.
    r.register(
        f,
        "getOwner",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/UserPrincipal;",
        |ctx, args| {
            let path_value = args.first().copied().unwrap_or(Value::Object(None));
            nio_owner_principal(ctx, path_value)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        f,
        "setOwner",
        "(Ljava/nio/file/Path;Ljava/nio/file/attribute/UserPrincipal;)Ljava/nio/file/Path;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))), cratonvm_native_api::NativeKind::Bridge,
    );
    // Accessors for the carrier `nio_owner_principal` returns on Windows.
    // `getName()` is the whole of `UserPrincipal`; `toString`/`equals`/`hashCode`
    // mirror `sun.nio.fs.WindowsUserPrincipals$User`, which compares on the SID
    // string (two principals for the same SID are equal even when the account
    // name renders differently) and prints `account (type)`.
    {
        let wup = "sun/nio/fs/WindowsUserPrincipals$User";
        r.register(wup, "getName", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field_by_name(this, "accountName")))
        }, cratonvm_native_api::NativeKind::Bridge);
        r.register(wup, "toString", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name = match ctx.get_field_by_name(this, "accountName") {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            // `SidType` values from Win32 `SID_NAME_USE`, in declaration order.
            let kind = match ctx.get_field_by_name(this, "sidType") {
                Value::Int(1) => "USER",
                Value::Int(2) => "GROUP",
                Value::Int(3) => "DOMAIN",
                Value::Int(4) => "ALIAS",
                Value::Int(5) => "WELL_KNOWN_GROUP",
                Value::Int(6) => "DELETED_ACCOUNT",
                Value::Int(7) => "INVALID",
                Value::Int(9) => "COMPUTER",
                _ => "UNKNOWN",
            };
            let s = ctx.create_string(&format!("{name} ({kind})"));
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge);
        r.register(wup, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let Some(Value::Object(Some(other))) = args.get(1).copied() else {
                return Ok(Some(Value::Int(0)));
            };
            if ctx.class_id_of_object(other) != ctx.class_id_of_object(this) {
                return Ok(Some(Value::Int(0)));
            }
            let read = |ctx: &mut dyn NativeContext, o| match ctx.get_field_by_name(o, "sidString")
            {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let a = read(ctx, this);
            let b = read(ctx, other);
            Ok(Some(Value::Int(i32::from(!a.is_empty() && a == b))))
        }, cratonvm_native_api::NativeKind::Bridge);
        r.register(wup, "hashCode", "()I", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let sid = match ctx.get_field_by_name(this, "sidString") {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            // `String.hashCode()`, so the value matches what the real carrier
            // (which hashes the same field) would produce.
            let mut hash: i32 = 0;
            for ch in sid.encode_utf16() {
                hash = hash.wrapping_mul(31).wrapping_add(i32::from(ch));
            }
            Ok(Some(Value::Int(hash)))
        }, cratonvm_native_api::NativeKind::Bridge);
    }
    // These three used to return `args[0]` — the link path — without creating
    // or reading anything, i.e. they claimed success for a link that was never
    // made. `Files.createSymbolicLink`/`createLink`/`readSymbolicLink` all have
    // real bytecode that delegates to the provider, so these registrations lose
    // to it and the lie was never observed; the provider natives registered on
    // `java/nio/file/spi/FileSystemProvider` are what actually run. They are
    // kept, and made real, so that any dispatch path that DOES prefer a
    // registration over bytecode gets the same answer as the provider.
    r.register(
        f,
        "createLink",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;)Ljava/nio/file/Path;",
        |ctx, args| {
            let mut link_obj = obj_arg(args, 0)?;
            let mut existing = obj_arg(args, 1)?;
            // `p57_read_path` can call back into `Path.toString()` bytecode for
            // a foreign Path implementation, so it is a GC point: the pair
            // read hands back the link we return (and `existing`) current.
            let (link, existing) = p57_read_path_pair(ctx, &mut link_obj, &mut existing);
            p57_create_hard_link(ctx, &link, &existing)?;
            Ok(Some(Value::Object(Some(link_obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(f, "createSymbolicLink",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/file/Path;",
        |ctx, args| {
            let mut link_obj = obj_arg(args, 0)?;
            let mut target = obj_arg(args, 1)?;
            let (link, target) = p57_read_path_pair(ctx, &mut link_obj, &mut target);
            p57_create_symbolic_link(ctx, &link, &target)?;
            Ok(Some(Value::Object(Some(link_obj))))
        }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        f,
        "readSymbolicLink",
        "(Ljava/nio/file/Path;)Ljava/nio/file/Path;",
        |ctx, args| {
            let mut path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, &mut path_obj);
            p57_read_symbolic_link(ctx, &p)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
}


/// The security-descriptor natives `Files.getOwner` reaches on Windows
/// (`WindowsAclFileAttributeView.getOwner` -> `getFileSecurity`,
/// `GetSecurityDescriptorOwner`, then `WindowsUserPrincipals.fromSid` ->
/// `ConvertSidToStringSid` and `LookupAccountSid`), each the JNI body of
/// `WindowsNativeDispatcher.c` reproduced over advapi32. Spring Boot's
/// `ApplicationTemp` reads the owner of the temp directory, and without these the
/// call ended in `UnsatisfiedLinkError: GetFileSecurity0` (`ApplicationTempTests`).
///
/// `GetFileSecurityW` returns a SELF-RELATIVE descriptor, so the owner SID is at
/// `base + OffsetOwner` and the SID text and length need nothing from the OS; only
/// the file read and the account lookup do.
#[cfg(windows)]
pub fn win32_register_security_natives(r: &mut NativeMethodRegistry) {
    extern "system" {
        fn GetFileSecurityW(
            lpFileName: *const u16,
            requestedInformation: u32,
            pSecurityDescriptor: *mut u8,
            nLength: u32,
            lpnLengthNeeded: *mut u32,
        ) -> i32;
        fn LookupAccountSidW(
            system_name: *const u16,
            sid: *mut std::ffi::c_void,
            name: *mut u16,
            name_len: *mut u32,
            domain: *mut u16,
            domain_len: *mut u32,
            use_: *mut i32,
        ) -> i32;
        fn GetLastError() -> u32;
    }
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
    const ERROR_INVALID_PARAMETER: u32 = 87;
    const SE_SELF_RELATIVE: u16 = 0x8000;

    /// The bytes of the SID at `addr`: its 8-byte header, then `4 * SubAuthorityCount`.
    fn read_sid(ctx: &dyn NativeContext, addr: i64) -> Option<Vec<u8>> {
        let mut header = [0u8; 8];
        if addr == 0 || !ctx.copy_from_native_memory(addr, &mut header) {
            return None;
        }
        let total = 8 + 4 * usize::from(header[1]);
        let mut sid = vec![0u8; total];
        ctx.copy_from_native_memory(addr, &mut sid).then_some(sid)
    }

    // `private static native int GetFileSecurity0(long lpFileName, int
    // requestedInformation, long pSecurityDescriptor, int nLength)`: the length
    // the descriptor needs when the buffer is too small, `nLength` when it fit.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "GetFileSecurity0",
        "(JIJI)I",
        |ctx, args| {
            let wide = win32_wide_arg(ctx, args, 0, "GetFileSecurity0")?;
            let info = match args.get(1) {
                Some(Value::Int(v)) => *v as u32,
                _ => 0,
            };
            let sd_addr = win32_long_arg(args, 2, "GetFileSecurity0")?;
            let n_length = match args.get(3) {
                Some(Value::Int(v)) if *v > 0 => *v as usize,
                _ => 0,
            };
            let mut buf = vec![0u8; n_length];
            let mut needed = 0u32;
            // SAFETY: `wide` is NUL-terminated; `buf` is `n_length` bytes (a null
            // pointer when it is empty, with that length passed).
            let ok = unsafe {
                GetFileSecurityW(
                    wide.as_ptr(),
                    info,
                    if n_length == 0 {
                        std::ptr::null_mut()
                    } else {
                        buf.as_mut_ptr()
                    },
                    n_length as u32,
                    &mut needed,
                )
            };
            if ok == 0 {
                let code = unsafe { GetLastError() };
                if code == ERROR_INSUFFICIENT_BUFFER {
                    return Ok(Some(Value::Int(needed as i32)));
                }
                return Err(win32_exception(ctx, code, "GetFileSecurity0"));
            }
            let _ = ctx.copy_to_native_memory(sd_addr, &buf);
            Ok(Some(Value::Int(n_length as i32)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `static native long GetSecurityDescriptorOwner(long pSecurityDescriptor)`:
    // the address of the owner SID inside the descriptor, 0 when it has none.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "GetSecurityDescriptorOwner",
        "(J)J",
        |ctx, args| {
            let sd_addr = win32_long_arg(args, 0, "GetSecurityDescriptorOwner")?;
            let mut header = [0u8; 20];
            if sd_addr == 0 || !ctx.copy_from_native_memory(sd_addr, &mut header) {
                return Err(win32_exception(
                    ctx,
                    ERROR_INVALID_PARAMETER,
                    "GetSecurityDescriptorOwner",
                ));
            }
            let control = u16::from_le_bytes([header[2], header[3]]);
            if control & SE_SELF_RELATIVE == 0 {
                return Err(win32_exception(
                    ctx,
                    ERROR_INVALID_PARAMETER,
                    "GetSecurityDescriptorOwner",
                ));
            }
            let offset = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
            Ok(Some(Value::Long(if offset == 0 {
                0
            } else {
                sd_addr + i64::from(offset)
            })))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `static native int GetLengthSid(long sidAddress)`.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "GetLengthSid",
        "(J)I",
        |ctx, args| {
            let sid_addr = win32_long_arg(args, 0, "GetLengthSid")?;
            Ok(Some(Value::Int(
                read_sid(ctx, sid_addr).map_or(0, |sid| sid.len() as i32),
            )))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `static native String ConvertSidToStringSid(long sidAddress)`: `S-R-I-S1-S2..`,
    // the identifier authority in decimal while it fits 32 bits, hex otherwise.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "ConvertSidToStringSid",
        "(J)Ljava/lang/String;",
        |ctx, args| {
            let sid_addr = win32_long_arg(args, 0, "ConvertSidToStringSid")?;
            let Some(sid) = read_sid(ctx, sid_addr) else {
                return Err(win32_exception(
                    ctx,
                    ERROR_INVALID_PARAMETER,
                    "ConvertSidToStringSid",
                ));
            };
            let authority = sid[2..8]
                .iter()
                .fold(0u64, |acc, b| (acc << 8) | u64::from(*b));
            let mut text = format!("S-{}-", sid[0]);
            if authority < (1 << 32) {
                text.push_str(&authority.to_string());
            } else {
                text.push_str(&format!("0x{authority:012X}"));
            }
            for sub in sid[8..].chunks_exact(4) {
                text.push('-');
                text.push_str(&u32::from_le_bytes([sub[0], sub[1], sub[2], sub[3]]).to_string());
            }
            let s = ctx.create_string_uninterned(&text);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `private static native void LookupAccountSid0(long sidAddress, Account
    // account)`: fills `Account.domain` / `name` / `use`.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "LookupAccountSid0",
        "(JLsun/nio/fs/WindowsNativeDispatcher$Account;)V",
        |ctx, args| {
            let sid_addr = win32_long_arg(args, 0, "LookupAccountSid0")?;
            let account = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("LookupAccountSid0: null account".to_string()),
                    }
                    .into())
                }
            };
            let Some(sid) = read_sid(ctx, sid_addr) else {
                return Err(win32_exception(
                    ctx,
                    ERROR_INVALID_PARAMETER,
                    "LookupAccountSid0",
                ));
            };
            let mut name = vec![0u16; 256];
            let mut domain = vec![0u16; 256];
            let mut name_len = name.len() as u32;
            let mut domain_len = domain.len() as u32;
            let mut sid_use = 0i32;
            // SAFETY: `sid` is a whole SID; both buffers are the lengths passed.
            let ok = unsafe {
                LookupAccountSidW(
                    std::ptr::null(),
                    sid.as_ptr() as *mut std::ffi::c_void,
                    name.as_mut_ptr(),
                    &mut name_len,
                    domain.as_mut_ptr(),
                    &mut domain_len,
                    &mut sid_use,
                )
            };
            if ok == 0 {
                let code = unsafe { GetLastError() };
                return Err(win32_exception(ctx, code, "LookupAccountSid0"));
            }
            let name_text = String::from_utf16_lossy(&name[..name_len as usize]);
            let domain_text = String::from_utf16_lossy(&domain[..domain_len as usize]);
            let account_pin = ctx.pin_native_root(account);
            let name_obj = ctx.create_string_uninterned(&name_text);
            let name_pin = ctx.pin_native_root(name_obj);
            let domain_obj = ctx.create_string_uninterned(&domain_text);
            let account = ctx.read_native_pin(account_pin, account);
            let name_obj = ctx.read_native_pin(name_pin, name_obj);
            ctx.set_field_by_name(account, "domain", Value::Object(Some(domain_obj)));
            ctx.set_field_by_name(account, "name", Value::Object(Some(name_obj)));
            ctx.set_field_by_name(account, "use", Value::Int(sid_use));
            ctx.unpin_native_roots(account_pin);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
}

/// Token and privilege handles the `WindowsSecurity` natives hand to Java.
///
/// `WindowsNativeDispatcher.CloseHandle` closes an fd_table id (`CreateFile0`
/// absorbs the real OS handle into that table), so a raw token `HANDLE` returned
/// as-is could collide with a live descriptor id and close the wrong thing.
/// Tokens therefore travel as minted ids (far above any fd id) that this table
/// maps back to the OS handle. A `LUID` (`LookupPrivilegeValue0`) is a JDK-side
/// `LocalAlloc` block Java frees with `LocalFree`; here it is an id as well, so
/// no native memory is involved.
#[cfg(windows)]
pub struct Win32SecurityHandles {
    pub next: i64,
    pub tokens: std::collections::BTreeMap<i64, isize>,
    pub luids: std::collections::BTreeMap<i64, [u8; 8]>,
}

#[cfg(windows)]
pub fn win32_security_handles(
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<Win32SecurityHandles> {
    static TABLE: cratonvm_types::lock_order::OrderedPlMutex<Win32SecurityHandles> =
        cratonvm_types::lock_order::OrderedPlMutex::new(
            Win32SecurityHandles {
                next: 0x5ec0_0000_0000_0000,
                tokens: std::collections::BTreeMap::new(),
                luids: std::collections::BTreeMap::new(),
            },
            cratonvm_types::lock_order::LockLevel::Scratch,
        );
    &TABLE
}

#[cfg(windows)]
pub fn win32_mint_token_handle(raw: isize) -> i64 {
    let mut table = win32_security_handles().lock();
    table.next += 1;
    let id = table.next;
    table.tokens.insert(id, raw);
    id
}

/// The OS handle behind a minted token id; `0` stays `0` (no token).
#[cfg(windows)]
pub fn win32_token_handle(id: i64) -> Option<isize> {
    if id == 0 {
        return Some(0);
    }
    win32_security_handles().lock().tokens.get(&id).copied()
}

/// `CloseHandle` for a minted token id. `false` when `id` is not one, so the
/// caller falls through to the fd_table.
#[cfg(windows)]
pub fn win32_close_token_handle(id: i64) -> bool {
    extern "system" {
        fn CloseHandle(h: *mut std::ffi::c_void) -> i32;
    }
    let raw = win32_security_handles().lock().tokens.remove(&id);
    match raw {
        Some(raw) => {
            // SAFETY: `raw` was returned by OpenProcessToken/OpenThreadToken/
            // DuplicateTokenEx and is released exactly once (removed above).
            unsafe { CloseHandle(raw as *mut std::ffi::c_void) };
            true
        }
        None => false,
    }
}

/// The self-relative security descriptor at `addr`, read whole: its header gives
/// the offsets of the owner, group, SACL and DACL, each of which carries its own
/// length (SID: 8 + 4 * sub-authority count; ACL: `AclSize`).
#[cfg(windows)]
pub fn win32_read_self_relative_descriptor(ctx: &dyn NativeContext, addr: i64) -> Option<Vec<u8>> {
    const SE_SELF_RELATIVE: u16 = 0x8000;
    let mut header = [0u8; 20];
    if addr == 0 || !ctx.copy_from_native_memory(addr, &mut header) {
        return None;
    }
    if u16::from_le_bytes([header[2], header[3]]) & SE_SELF_RELATIVE == 0 {
        return None;
    }
    let offset = |at: usize| {
        u32::from_le_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]]) as i64
    };
    let mut end = 20i64;
    for (at, is_sid) in [(4usize, true), (8, true), (12, false), (16, false)] {
        let off = offset(at);
        if off == 0 {
            continue;
        }
        let mut head = [0u8; 8];
        if !ctx.copy_from_native_memory(addr + off, &mut head) {
            return None;
        }
        let len = if is_sid {
            8 + 4 * i64::from(head[1])
        } else {
            i64::from(u16::from_le_bytes([head[2], head[3]]))
        };
        end = end.max(off + len);
    }
    let mut whole = vec![0u8; end as usize];
    ctx.copy_from_native_memory(addr, &mut whole)
        .then_some(whole)
}

/// `WindowsNativeDispatcher`'s process/thread token and privilege natives, each
/// the JNI body of `WindowsNativeDispatcher.c` over advapi32/kernel32. They are
/// what `WindowsSecurity`'s class initialiser and `enablePrivilege` call, so
/// `Files.copy(source, target, COPY_ATTRIBUTES)` on Windows reaches them through
/// `WindowsFileCopy.copySecurityAttributes`; before them that copy ended in
/// `UnsatisfiedLinkError: GetCurrentProcess()` (the Quarkus fast-jar builder
/// copies dependencies this way).
#[cfg(windows)]
pub fn win32_register_token_natives(r: &mut NativeMethodRegistry) {
    extern "system" {
        fn GetCurrentProcess() -> *mut std::ffi::c_void;
        fn GetCurrentThread() -> *mut std::ffi::c_void;
        fn OpenProcessToken(process: isize, access: u32, token: *mut isize) -> i32;
        fn OpenThreadToken(thread: isize, access: u32, open_as_self: i32, token: *mut isize)
            -> i32;
        fn DuplicateTokenEx(
            existing: isize,
            access: u32,
            attributes: *mut std::ffi::c_void,
            impersonation_level: i32,
            token_type: i32,
            new_token: *mut isize,
        ) -> i32;
        fn SetThreadToken(thread: *mut isize, token: isize) -> i32;
        fn AdjustTokenPrivileges(
            token: isize,
            disable_all: i32,
            new_state: *const u8,
            buffer_length: u32,
            previous_state: *mut u8,
            return_length: *mut u32,
        ) -> i32;
        fn LookupPrivilegeValueW(system_name: *const u16, name: *const u16, luid: *mut u8) -> i32;
        fn SetFileSecurityW(file_name: *const u16, information: u32, descriptor: *const u8) -> i32;
        fn MapGenericMask(access_mask: *mut u32, mapping: *const u32);
        fn AccessCheck(
            descriptor: *const u8,
            client_token: isize,
            desired_access: u32,
            mapping: *const u32,
            privileges: *mut u32,
            privileges_length: *mut u32,
            granted_access: *mut u32,
            access_status: *mut i32,
        ) -> i32;
        fn GetLastError() -> u32;
        fn SetLastError(code: u32);
    }
    const ERROR_NO_TOKEN: u32 = 1008;
    const ERROR_INVALID_HANDLE: u32 = 6;
    const ERROR_INVALID_PARAMETER: u32 = 87;
    const SECURITY_IMPERSONATION: i32 = 2;
    const TOKEN_IMPERSONATION: i32 = 2;

    fn int_arg(args: &[Value], i: usize) -> u32 {
        match args.get(i) {
            Some(Value::Int(v)) => *v as u32,
            _ => 0,
        }
    }

    // `static native long GetCurrentProcess()` / `GetCurrentThread()`: the
    // kernel's pseudo-handles, valid only in the calling process/thread.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "GetCurrentProcess",
        "()J",
        |_ctx, _args| {
            // SAFETY: no arguments; returns a pseudo-handle.
            Ok(Some(Value::Long(
                unsafe { GetCurrentProcess() } as isize as i64
            )))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "GetCurrentThread",
        "()J",
        |_ctx, _args| {
            // SAFETY: no arguments; returns a pseudo-handle.
            Ok(Some(Value::Long(
                unsafe { GetCurrentThread() } as isize as i64
            )))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `static native long OpenProcessToken(long hProcess, int desiredAccess)`.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "OpenProcessToken",
        "(JI)J",
        |ctx, args| {
            let process = win32_long_arg(args, 0, "OpenProcessToken")? as isize;
            let mut token = 0isize;
            // SAFETY: `token` is a live out-pointer.
            let ok = unsafe { OpenProcessToken(process, int_arg(args, 1), &mut token) };
            if ok == 0 {
                let code = unsafe { GetLastError() };
                return Err(win32_exception(ctx, code, "OpenProcessToken"));
            }
            Ok(Some(Value::Long(win32_mint_token_handle(token))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `static native long OpenThreadToken(long hThread, int desiredAccess,
    // boolean openAsSelf)`: `0` when the thread has no token of its own
    // (`ERROR_NO_TOKEN`), which is the ordinary case before impersonation.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "OpenThreadToken",
        "(JIZ)J",
        |ctx, args| {
            let thread = win32_long_arg(args, 0, "OpenThreadToken")? as isize;
            let mut token = 0isize;
            // SAFETY: `token` is a live out-pointer.
            let ok = unsafe {
                OpenThreadToken(
                    thread,
                    int_arg(args, 1),
                    (int_arg(args, 2) != 0) as i32,
                    &mut token,
                )
            };
            if ok == 0 {
                let code = unsafe { GetLastError() };
                if code == ERROR_NO_TOKEN {
                    return Ok(Some(Value::Long(0)));
                }
                return Err(win32_exception(ctx, code, "OpenThreadToken"));
            }
            Ok(Some(Value::Long(win32_mint_token_handle(token))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `static native long DuplicateTokenEx(long hToken, int desiredAccess)`: an
    // impersonation token at `SecurityImpersonation`, as the JNI body asks for.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "DuplicateTokenEx",
        "(JI)J",
        |ctx, args| {
            let id = win32_long_arg(args, 0, "DuplicateTokenEx")?;
            let Some(existing) = win32_token_handle(id).filter(|h| *h != 0) else {
                return Err(win32_exception(
                    ctx,
                    ERROR_INVALID_HANDLE,
                    "DuplicateTokenEx",
                ));
            };
            let mut token = 0isize;
            // SAFETY: `existing` is a live token handle from this table; `token`
            // is a live out-pointer; null attributes are legal.
            let ok = unsafe {
                DuplicateTokenEx(
                    existing,
                    int_arg(args, 1),
                    std::ptr::null_mut(),
                    SECURITY_IMPERSONATION,
                    TOKEN_IMPERSONATION,
                    &mut token,
                )
            };
            if ok == 0 {
                let code = unsafe { GetLastError() };
                return Err(win32_exception(ctx, code, "DuplicateTokenEx"));
            }
            Ok(Some(Value::Long(win32_mint_token_handle(token))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `static native void SetThreadToken(long thread, long hToken)`: thread `0`
    // is the calling thread, token `0` ends impersonation.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "SetThreadToken",
        "(JJ)V",
        |ctx, args| {
            let thread = win32_long_arg(args, 0, "SetThreadToken")? as isize;
            let id = win32_long_arg(args, 1, "SetThreadToken")?;
            let Some(token) = win32_token_handle(id) else {
                return Err(win32_exception(ctx, ERROR_INVALID_HANDLE, "SetThreadToken"));
            };
            let mut thread_handle = thread;
            let thread_ptr: *mut isize = if thread == 0 {
                std::ptr::null_mut()
            } else {
                &mut thread_handle
            };
            // SAFETY: a null thread pointer means the calling thread; otherwise
            // it points at a live local.
            if unsafe { SetThreadToken(thread_ptr, token) } == 0 {
                let code = unsafe { GetLastError() };
                return Err(win32_exception(ctx, code, "SetThreadToken"));            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `private static native long LookupPrivilegeValue0(long lpName)`: the LUID
    // as a minted id (see `Win32SecurityHandles`).
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "LookupPrivilegeValue0",
        "(J)J",
        |ctx, args| {
            let name = win32_wide_arg(ctx, args, 0, "LookupPrivilegeValue0")?;
            let mut luid = [0u8; 8];
            // SAFETY: `name` is NUL-terminated; `luid` is the 8-byte LUID.
            let ok = unsafe {
                LookupPrivilegeValueW(std::ptr::null(), name.as_ptr(), luid.as_mut_ptr())
            };
            if ok == 0 {
                let code = unsafe { GetLastError() };
                return Err(win32_exception(ctx, code, "LookupPrivilegeValue0"));
            }
            let mut table = win32_security_handles().lock();
            table.next += 1;
            let id = table.next;
            table.luids.insert(id, luid);
            Ok(Some(Value::Long(id)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `static native void LocalFree(long address)`: releases a LUID id minted
    // above. The only blocks Java frees this way that this VM hands out; any
    // other value is not ours to free.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "LocalFree",
        "(J)V",
        |_ctx, args| {
            if let Some(Value::Long(id)) = args.first() {
                win32_security_handles().lock().luids.remove(id);            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `static native void AdjustTokenPrivileges(long token, long luid, int
    // attributes)`: one privilege, enabled or (attributes 0) removed. Like the
    // JNI body it reports `ERROR_NOT_ALL_ASSIGNED` as a failure.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "AdjustTokenPrivileges",
        "(JJI)V",
        |ctx, args| {
            let token_id = win32_long_arg(args, 0, "AdjustTokenPrivileges")?;
            let luid_id = win32_long_arg(args, 1, "AdjustTokenPrivileges")?;
            let attributes = int_arg(args, 2);
            let token = win32_token_handle(token_id).filter(|h| *h != 0);
            let luid = win32_security_handles().lock().luids.get(&luid_id).copied();
            let (Some(token), Some(luid)) = (token, luid) else {
                return Err(win32_exception(
                    ctx,
                    ERROR_INVALID_HANDLE,
                    "AdjustTokenPrivileges",
                ));
            };
            // TOKEN_PRIVILEGES with PrivilegeCount = 1: count, LUID, attributes.
            let mut state = [0u8; 16];
            state[0..4].copy_from_slice(&1u32.to_le_bytes());
            state[4..12].copy_from_slice(&luid);
            state[12..16].copy_from_slice(&attributes.to_le_bytes());
            // SAFETY: `state` is a whole one-entry TOKEN_PRIVILEGES; the
            // previous-state outputs are legitimately null.
            let ok = unsafe {
                SetLastError(0);
                AdjustTokenPrivileges(
                    token,
                    0,
                    state.as_ptr(),
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            let code = unsafe { GetLastError() };
            if ok == 0 || code != 0 {
                return Err(win32_exception(
                    ctx,
                    if code == 0 {
                        ERROR_INVALID_PARAMETER
                    } else {
                        code
                    },
                    "AdjustTokenPrivileges",
                ));
            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `static native boolean AccessCheck(long token, long securityInfo, int
    // accessMask, int genericRead, int genericWrite, int genericExecute, int
    // genericAll)`: the JNI body of `WindowsNativeDispatcher.c`. `token` is a
    // minted impersonation-token id (`DuplicateTokenEx`), `securityInfo` the
    // self-relative descriptor `GetFileSecurity0` left in a `NativeBuffer`.
    // `WindowsSecurity.checkAccessMask` (reached by `Files.isReadable` /
    // `isWritable` / `isExecutable` and `FileSystemProvider.checkAccess` for
    // anything but a plain existence probe) asks it whether the user may do
    // `accessMask`. Like the original: the generic bits are mapped first
    // (`MapGenericMask`), the answer is `AccessStatus`, and a failing
    // `AccessCheck` throws `WindowsException(GetLastError())`. JFR's
    // `Repository.createRealBasePath` reaches it (`Files.isWritable`) on every
    // recording start, so before it every JFR recording on Windows ended in
    // `UnsatisfiedLinkError: AccessCheck` (gci i19/ob rows; the gci round's own
    // registration of this triple, also a Bridge, was folded into this one at
    // the 2026-10-15 merge of dev).
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "AccessCheck",
        "(JJIIIII)Z",
        |ctx, args| {
            const ERROR_INVALID_SECURITY_DESCR: u32 = 1338;
            let token_id = win32_long_arg(args, 0, "AccessCheck")?;
            let sd_addr = win32_long_arg(args, 1, "AccessCheck")?;
            let Some(token) = win32_token_handle(token_id) else {
                return Err(win32_exception(ctx, ERROR_INVALID_HANDLE, "AccessCheck"));
            };
            let Some(descriptor) = win32_read_self_relative_descriptor(ctx, sd_addr) else {
                return Err(win32_exception(
                    ctx,
                    ERROR_INVALID_SECURITY_DESCR,
                    "AccessCheck",
                ));
            };
            // The descriptor is handed to advapi32 from an 8-aligned buffer: a
            // `Vec<u8>` carries no alignment promise and the ACL entries inside
            // are DWORD-aligned relative to the descriptor.
            let mut aligned = vec![0u64; descriptor.len().div_ceil(8)];
            // SAFETY: `aligned` holds at least `descriptor.len()` bytes and the
            // two buffers cannot overlap.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    descriptor.as_ptr(),
                    aligned.as_mut_ptr() as *mut u8,
                    descriptor.len(),
                );
            }
            // GENERIC_MAPPING { Read, Write, Execute, All }.
            let mut mapping = [0u32; 4];
            for (i, slot) in mapping.iter_mut().enumerate() {
                *slot = int_arg(args, 3 + i);
            }
            let mut rights = int_arg(args, 2);
            // PRIVILEGE_SET {Count, Control, one LUID_AND_ATTRIBUTES} = 20 bytes,
            // zeroed as the JNI body does (`PRIVILEGE_SET privileges = {0}`).
            let mut privileges = [0u32; 5];
            let mut privileges_length = std::mem::size_of_val(&privileges) as u32;
            let mut granted = 0u32;
            let mut status = 0i32;
            // SAFETY: every pointer is a live local of the type the call expects;
            // `aligned` is a whole self-relative descriptor; `token` is a table
            // handle (or 0, which `AccessCheck` rejects with an error we throw).
            let ok = unsafe {
                MapGenericMask(&mut rights, mapping.as_ptr());
                AccessCheck(
                    aligned.as_ptr() as *const u8,
                    token,
                    rights,
                    mapping.as_ptr(),
                    privileges.as_mut_ptr(),
                    &mut privileges_length,
                    &mut granted,
                    &mut status,
                )
            };
            if ok == 0 {
                let code = unsafe { GetLastError() };
                return Err(win32_exception(ctx, code, "AccessCheck"));
            }
            Ok(Some(Value::Int((status != 0) as i32)))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    // `static native void SetFileSecurity0(long lpFileName, int
    // securityInformation, long pSecurityDescriptor)`: the descriptor is the
    // self-relative one `GetFileSecurity0` filled, read whole from its buffer.
    r.register(
        "sun/nio/fs/WindowsNativeDispatcher",
        "SetFileSecurity0",
        "(JIJ)V",
        |ctx, args| {
            let wide = win32_wide_arg(ctx, args, 0, "SetFileSecurity0")?;
            let info = int_arg(args, 1);
            let sd_addr = win32_long_arg(args, 2, "SetFileSecurity0")?;
            let Some(descriptor) = win32_read_self_relative_descriptor(ctx, sd_addr) else {
                return Err(win32_exception(
                    ctx,
                    ERROR_INVALID_PARAMETER,
                    "SetFileSecurity0",
                ));
            };
            // SAFETY: `wide` is NUL-terminated; `descriptor` is a whole
            // self-relative descriptor that outlives the call.
            if unsafe { SetFileSecurityW(wide.as_ptr(), info, descriptor.as_ptr()) } == 0 {
                let code = unsafe { GetLastError() };
                return Err(win32_exception(ctx, code, "SetFileSecurity0"));            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
}

