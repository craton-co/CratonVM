#[allow(unused_imports)]
use super::*;

pub fn register_phase57_nio_file_c(r: &mut NativeMethodRegistry, __path_rows_before: usize, __fs_rows_before: usize, __fsp_rows_before: usize) {
    let path = "java/nio/file/Path";
    let _paths = "java/nio/file/Paths";
    let files = "java/nio/file/Files";
    let fs_class = "java/nio/file/FileSystem";
    let fsp = "java/nio/file/spi/FileSystemProvider";
    r.register(
        fsp,
        "readAttributes",
        "(Ljava/nio/file/Path;Ljava/lang/String;[Ljava/nio/file/LinkOption;)Ljava/util/Map;",
        |ctx, args| {
            retired_receiver_yield!(
                ctx,
                args,
                "readAttributes",
                "(Ljava/nio/file/Path;Ljava/lang/String;[Ljava/nio/file/LinkOption;)Ljava/util/Map;"
            );
            let path_obj = obj_arg(args, 1)?;
            let spec = match args.get(2) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let nofollow =
                matches!(args.get(3), Some(Value::Object(Some(a))) if ctx.array_length(*a) > 0);
            read_named_attributes(ctx, path_obj, &spec, nofollow)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // `FileSystemProvider.setAttribute(Path, String, Object, LinkOption...)` —
    // the write-side twin of the reader above, missing for exactly the same
    // reason and left behind when that one was added.
    //
    // `Files.setAttribute` is how everything name-keyed writes: H2's
    // `FilePathDisk.setReadOnly` takes this branch whenever the FileStore
    // reports DOS rather than POSIX attributes (i.e. on Windows), which made
    // `org.h2.test.unit.TestFileSystem` die at `testSetReadOnly` on EVERY
    // filesystem prefix it exercises. It is not a Windows defect: the Linux
    // witness fails identically, H2 just reaches
    // `Files.setPosixFilePermissions` there instead.
    //
    // Args: `[this, path, attribute, value, options]`.
    r.register(
        fsp,
        "setAttribute",
        "(Ljava/nio/file/Path;Ljava/lang/String;Ljava/lang/Object;[Ljava/nio/file/LinkOption;)V",
        |ctx, args| {
            retired_receiver_yield!(
                ctx,
                args,
                "setAttribute",
                "(Ljava/nio/file/Path;Ljava/lang/String;Ljava/lang/Object;[Ljava/nio/file/LinkOption;)V"
            );
            let path_obj = obj_arg(args, 1)?;
            let spec = match args.get(2) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let value = match args.get(3) {
                Some(Value::Object(v)) => *v,
                _ => None,
            };
            let nofollow =
                matches!(args.get(4), Some(Value::Object(Some(a))) if ctx.array_length(*a) > 0);
            write_named_attribute(ctx, path_obj, &spec, value, nofollow)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // The rest of `FileSystemProvider`'s abstract surface. Every one of these
    // has a working `java/nio/file/Files` native in front of it, which is why
    // none of them showed up before `setAttribute` did — `Files.delete` never
    // reaches the provider, so the missing provider method was invisible. It
    // stops being invisible the moment anything holds a `FileSystemProvider`
    // and calls it directly (`Files.walkFileTree`'s own internals, NIO code
    // written against the SPI, and every `AbstractFileSystemProvider` caller
    // in the JDK do exactly that), and the failure is the same
    // `AbstractMethodError: ... has no Code attribute`.
    //
    // These delegate to the `Files` natives rather than re-implementing them:
    // one body per operation is what keeps the two entry points from drifting
    // (the `Files.move` native, for instance, carries a
    // `FileAlreadyExistsException` contract that H2 depends on by type).
    //
    // Direct function calls, NOT `ctx.invoke("java/nio/file/Files", ...)` —
    // same reason as `native_files_copy`/`native_files_move` above. `Files.delete`/
    // `createDirectory` have real bytecode and a `Bridge` registration, so
    // under `--real-jdk` a `ctx.invoke` bounce back up to them would prefer
    // that real bytecode again, which calls straight back into this exact
    // provider method: unconditional `StackOverflowError` on every
    // `Files.delete`/`createDirectory` call under `--real-jdk`, the identical
    // shape `native_files_copy` fixed for `copy`/`move` (MEASURED: `Files.
    // delete` was the very next call in a plain copy+move+delete sequence to
    // hit this once `copy`/`move` were fixed, at `Files.java:1052`).
    r.register(fsp, "delete", "(Ljava/nio/file/Path;)V", |ctx, args| {
        retired_receiver_yield!(ctx, args, "delete", "(Ljava/nio/file/Path;)V");
        let path = obj_arg(args, 1)?;
        // Calls the extracted body directly (not ctx.invoke) -- see the
        // note above `native_files_delete`.
        native_files_delete(ctx, &[Value::Object(Some(path))])?;
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        fsp,
        "createDirectory",
        "(Ljava/nio/file/Path;[Ljava/nio/file/attribute/FileAttribute;)V",
        |ctx, args| {
            retired_receiver_yield!(
                ctx,
                args,
                "createDirectory",
                "(Ljava/nio/file/Path;[Ljava/nio/file/attribute/FileAttribute;)V"
            );
            let path = obj_arg(args, 1)?;
            let attrs = args.get(2).copied().unwrap_or(Value::Object(None));
            // Calls the extracted body directly (not ctx.invoke) -- see the
            // note above `native_files_delete`.
            native_files_create_directory(ctx, &[Value::Object(Some(path)), attrs])?;
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `NativeCallback` is a plain `fn` pointer, so these two cannot share one
    // capturing closure over the method name.
    r.register(
        fsp,
        "copy",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/CopyOption;)V",
        |ctx, args| {
            retired_receiver_yield!(
                ctx,
                args,
                "copy",
                "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/CopyOption;)V"
            );
            fsp_delegate_two_path_copy(ctx, args, "copy")
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        fsp,
        "move",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/CopyOption;)V",
        |ctx, args| {
            retired_receiver_yield!(
                ctx,
                args,
                "move",
                "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/CopyOption;)V"
            );
            fsp_delegate_two_path_copy(ctx, args, "move")
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `Files.isHidden` has NO `Files`-level native, so this one was reachable
    // and did fail: `AbstractMethodError: FileSystemProvider.isHidden(Path)Z`.
    // `dos_attr_flag` is the same rule the DOS view reads and writes — the real
    // `FILE_ATTRIBUTE_HIDDEN` bit on Windows, the dot-file convention
    // elsewhere.
    r.register(fsp, "isHidden", "(Ljava/nio/file/Path;)Z", |ctx, args| {
        retired_receiver_yield!(ctx, args, "isHidden", "(Ljava/nio/file/Path;)Z");
        let mut path_obj = obj_arg(args, 1)?;
        let path = p57_read_path(ctx, &mut path_obj);
        if std::fs::symlink_metadata(&path).is_err() && jarfs_decode(&path).is_none() {
            return Err(p57_no_such_file(ctx, &path)?);
        }
        Ok(Some(Value::Int(i32::from(dos_attr_flag(
            &path,
            DOS_ATTR_HIDDEN,
        )))))
    }, cratonvm_native_api::NativeKind::Bridge);

    r.register(
        fsp,
        "newDirectoryStream",
        "(Ljava/nio/file/Path;Ljava/nio/file/DirectoryStream$Filter;)Ljava/nio/file/DirectoryStream;",
        |ctx, args| {
            retired_receiver_yield!(
                ctx,
                args,
                "newDirectoryStream",
                "(Ljava/nio/file/Path;Ljava/nio/file/DirectoryStream$Filter;)Ljava/nio/file/DirectoryStream;"
            );
            let mut path_obj = obj_arg(args, 1)?;
            let p = p57_read_path(ctx, &mut path_obj);
            if let Some(r) = platform_provider_lists_host_dir(ctx, args, path_obj, &p) {
                return r;
            }
            // TWO fabricated successes used to live in this body, and both
            // reported the operation as having been performed when it had not:
            //
            //   1. the host arm's `Err(_) => vec![]` answered a MISSING
            //      directory, a REGULAR FILE and an unreadable directory with
            //      an empty listing, so every caller was told "this directory
            //      is empty" and proceeded;
            //   2. the `DirectoryStream$Filter` in slot 2 was never read, so a
            //      filtered stream — `Files.newDirectoryStream(dir, "*.txt")`
            //      builds one, and so does every `Files.newDirectoryStream(dir,
            //      filter)` caller — handed back EVERY entry in the directory.
            //      A `for (Path q : ds) Files.delete(q);` over a glob deleted
            //      the files the glob excluded.
            //
            // Refuse before anything is allocated, exactly where the JDK does
            // (measured on Adoptium 25.0.3+9, 2026-08-16:
            // `provider.newDirectoryStream(<missing>, …)` -> NoSuchFileException,
            // `provider.newDirectoryStream(<regular file>, …)` ->
            // NotDirectoryException).
            // G4-1-the-io-and-nio-fabricated-success-sweep-measured-20260816.md
            if let Some(refused) = p57_dir_listing_refusal(ctx, &p, true)? {
                return Err(refused);
            }
            // 3 fields: slot 0 = materialised Object[] of Paths, slot 1 = the
            // closed flag `close()` sets, slot 2 = the "an Iterator has already
            // been handed out" latch (see the `iterator` registration below).
            // Minted AS the concrete `sun.nio.fs.*DirectoryStream`, not as
            // the INTERFACE `java.nio.file.DirectoryStream` (JVMS 6.5; see
            // `DIR_STREAM_IMPLS`) — but ONLY for a REAL host directory.
            //
            // A jar/jrt-mounted directory has no real Windows find-handle at
            // all, and real `WindowsDirectoryStream` keeps chasing MORE of
            // them every time one is backfilled: first `closeLock`, then
            // `isOpen`, then a NativeBuffer field (`findDataBuffer`) real
            // `close()`/`hasNext()` release via a real `FindClose` Win32
            // call — a losing game for content that was never a real
            // directory to begin with. Same fix as `p57_alloc_provider`'s
            // jar/jrt scheme: mint against the ABSTRACT class instead, which
            // has no Code for any `fsp`-registered method to shadow, so
            // dispatch stays on this file's own natives (which read/write
            // ONLY the three appended slots below) for the whole lifetime of
            // a virtual stream.
            //
            // Independently found and fixed twice the same day, from two
            // different bugs (this session via in-process javac's
            // `SystemModulesLocationHandler` walking the HIB-CV-27 jrt
            // filesystem's `/modules` listing; `f23b5f017` via the
            // `ZipFile$CleanableResource` NPE) — see
            // docs/feature-designs/native-dispatch-architecture.md
            // jrt-directorystream-shares-a-retired-shadow-class-with-real-host-streams-FIXED-20260922.md
            // for this session's account.
            let is_vfs = jarfs_decode(&p).is_some() || jrtfs_decode(&p).is_some();
            let candidates: &[&str] = if is_vfs { &[] } else { DIR_STREAM_IMPLS };
            let minted = cratonvm_native_io::concrete_receiver::alloc_concrete(
                ctx,
                candidates,
                "java/nio/file/DirectoryStream",
                DIR_STREAM_SLOTS,
            );
            let stream = minted.obj;
            let ds_base = minted.base;
            // Pin across the array/Path allocs below — a moving young GC there
            // would relocate the fresh stream/array (native stale-local family).
            let stream_pin = ctx.pin_native_root(stream);
            // `closeLock` — real `sun.nio.fs.WindowsDirectoryStream.close()`
            // opens `synchronized (closeLock)`. Minting AS that concrete class
            // (JVMS 6.5, `DIR_STREAM_IMPLS`) means real dispatch finds ITS OWN
            // `close()` Code and never reaches this file's interface-level
            // `close` native below, so its constructor — which this allocator
            // never runs — is the only place that field would normally get
            // set. Same shape, same fix, as `p57_link_provider_to_filesystem`:
            // backfill the one field real bytecode needs by name. Harmless
            // no-op now on the `is_vfs` abstract-class path (no such field to
            // resolve there); kept unconditional so a REAL directory stream
            // still gets it.
            let close_lock = ctx.new_object("java/lang/Object");
            let stream = ctx.read_native_pin(stream_pin, stream);
            if let Ok(Some(lock @ Value::Object(Some(_)))) = close_lock {
                ctx.set_field_by_name(stream, "closeLock", lock);
            }
            // `isOpen` — real `sun.nio.fs.WindowsDirectoryStream` declares
            // `private volatile boolean isOpen = true;`, a FIELD INITIALIZER
            // that only runs as part of `<init>`, which this allocator never
            // calls. Left at Java's zero-value `false`, real `iterator()`
            // (now that `close()`'s `closeLock` fix above lets its dispatch
            // reach real bytecode too) reads it first and throws
            // `IllegalStateException: Directory stream is closed` on a
            // stream that was never actually closed — `Files.list(...)`
            // right after `newDirectoryStream`
            // (`NestedFileSystemZipFileSystemIntegrationTests
            // .nestedZipSplitAndRestore`). `set_field_by_name` is a safe
            // no-op if this concrete class spells the field differently.
            ctx.set_field_by_name(stream, "isOpen", Value::Int(1));
            // Build array of Path entries
            let entries: Vec<String> = if let Some((jar, dir)) = jarfs_decode(&p) {
                jarfs_list_dir(&jar, &dir)
                    .into_iter()
                    .map(|child| jarfs_encode(&jar, &child))
                    .collect()
            } else if let Some((java_home, dir)) = jrtfs_decode(&p) {
                jrtfs_list_dir_classified(&java_home, &dir)
                    .into_iter()
                    .map(|(child, _)| jrtfs_encode(&java_home, &child))
                    .collect()
            } else {
                match std::fs::read_dir(&p) {
                    Ok(rd) => rd
                        .filter_map(|e| e.ok())
                        .map(|e| e.path().to_string_lossy().replace('\\', "/"))
                        .collect(),
                    // `p57_dir_listing_refusal` above has already established
                    // that `p` exists and is a directory, so a failure here is a
                    // genuine I/O condition (permissions, a racing unlink) and
                    // must not be laundered into "the directory is empty".
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                        return Err(p57_access_denied(ctx, &p)?)
                    }
                    Err(e) => return Err(p57_io_error(&e)),
                }
            };
            use cratonvm_types::ArrayElementType;
            let arr = ctx.new_array(ArrayElementType::Reference, entries.len());
            let arr_pin = ctx.pin_native_root(arr);
            for (i, entry) in entries.iter().enumerate() {
                let ep = p57_alloc_path(ctx, entry)?;
                let arr = ctx.read_native_pin(arr_pin, arr);
                ctx.set_array_element(arr, i, Value::Object(Some(ep)));
            }
            // Apply the filter. Nothing is held across `accept` except the two
            // pinned arrays and the pinned filter — every `ObjectRef` is re-read
            // from its pin after the call, because `accept` runs arbitrary
            // bytecode and can move all three. `Files.find`'s `BiPredicate`
            // loop, a few hundred lines above, is the worked example.
            //
            // Deviation stated rather than hidden: the JDK applies the filter
            // LAZILY, in `hasNext()`, and wraps a filter `IOException` in a
            // `DirectoryIteratorException` (measured). This listing is eager —
            // it always has been — so a throwing filter surfaces here, from
            // `newDirectoryStream`, with its own exception rather than the
            // wrapper. Both are loud; neither is the silent "filter ignored"
            // this replaces.
            let filter = match args.get(2).copied() {
                Some(Value::Object(Some(f))) => Some(f),
                _ => None,
            };
            let arr = if let Some(filter) = filter {
                let filter_pin = ctx.pin_native_root(filter);
                let n = {
                    let arr = ctx.read_native_pin(arr_pin, arr);
                    ctx.array_length(arr)
                };
                let mut accepted: Vec<usize> = Vec::with_capacity(n);
                for i in 0..n {
                    let arr = ctx.read_native_pin(arr_pin, arr);
                    let elem = match ctx.get_array_element(arr, i) {
                        Value::Object(Some(e)) => e,
                        _ => continue,
                    };
                    let f = ctx.read_native_pin(filter_pin, filter);
                    let verdict = ctx.invoke_virtual(
                        f,
                        "accept",
                        "(Ljava/lang/Object;)Z",
                        &[Value::Object(Some(elem))],
                    )?;
                    match verdict {
                        Some(Value::Int(v)) => {
                            if v != 0 {
                                accepted.push(i);
                            }
                        }
                        // Answering `false` here — which is what the sibling
                        // `Files.find` matcher does — would DROP the entry, i.e.
                        // shorten the listing on a dispatch fault. That is the
                        // same species this whole body is being repaired for.
                        _ => {
                            ctx.unpin_native_roots(filter_pin);
                            ctx.unpin_native_roots(arr_pin);
                            ctx.unpin_native_roots(stream_pin);
                            return Err(RuntimeError::IOException {
                                message:
                                    "DirectoryStream.Filter.accept did not return a boolean"
                                        .to_string(),
                            }
                            .into());
                        }
                    }
                }
                let filtered = ctx.new_array(ArrayElementType::Reference, accepted.len());
                let filtered_pin = ctx.pin_native_root(filtered);
                for (dst, src) in accepted.iter().enumerate() {
                    let arr = ctx.read_native_pin(arr_pin, arr);
                    let elem = ctx.get_array_element(arr, *src);
                    let filtered = ctx.read_native_pin(filtered_pin, filtered);
                    ctx.set_array_element(filtered, dst, elem);
                }
                // Read back UNDER the pin and hand the ref straight to the
                // `set_field` below without unpinning first — nothing between
                // here and the store allocates, but leaving the pin in place
                // until `stream_pin` is released means that stays true even if
                // something is inserted later.
                ctx.read_native_pin(filtered_pin, filtered)
            } else {
                ctx.read_native_pin(arr_pin, arr)
            };
            let stream = ctx.read_native_pin(stream_pin, stream);
            ctx.set_field(stream, ds_base, Value::Object(Some(arr)));
            ctx.set_field(stream, ds_base + 1, Value::Int(0));
            ctx.set_field(stream, ds_base + 2, Value::Int(0));
            // `stream_pin` is the OUTERMOST watermark, so releasing it also
            // releases `arr_pin`, `filter_pin` and `filtered_pin` above it.
            ctx.unpin_native_roots(stream_pin);
            Ok(Some(Value::Object(Some(stream))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // DirectoryStream is an interface — iterator()/close() are abstract on
    // it. Jetty start.jar (BaseHome.<init> → JettyBaseConfigSource) and
    // Spring Boot's path scanning both walk newDirectoryStream(...) via
    // for-each, which compiles to invokeinterface DirectoryStream.iterator.
    // Without these natives every such walk throws
    // `AbstractMethodError: DirectoryStream.iterator()V has no Code attribute`.
    // Every body below reads the stream's private map through
    // `dir_stream_base`, not at literal 0/1/2: since 2026-08-21 the mint names
    // the concrete `sun.nio.fs.*DirectoryStream` (JVMS 6.5, `DIR_STREAM_IMPLS`)
    // and the map sits above that class's own declared fields.
    let ds = "java/nio/file/DirectoryStream";
    // `--compatible` mints `Files.newDirectoryStream`'s result as the concrete `sun.nio.fs.*DirectoryStream` with
    // the materialised listing in slots the JDK class does not have (`dir_stream_base`); the JDK bodies of
    // `iterator` / `close` and of `UnixSecureDirectoryStream`'s methods read private fields (`ds`, `dfd`) such an
    // object never had, so once real bytes beat a `Bridge` every `Files.walkFileTree` threw
    // `NullPointerException: ... "this.ds" is null` (the Spring Boot test-resource cleanup). These rows ARE the
    // answer over that carrier: reviewed `Intrinsic`s under `--compatible`; under `--real-jdk` the JDK's own streams
    // are real and the retired-shadow tables drop these rows.
    let __ds_rows_before = r.dump_registrations().len();
    r.register(
        ds,
        "iterator",
        "()Ljava/util/Iterator;",
        |ctx, args| {
            if let Some(r) = ds_yield_to_jdk_built(ctx, args, "iterator", "()Ljava/util/Iterator;")
            {
                return r;
            }
            let this = obj_arg(args, 0)?;
            let ds_base = dir_stream_base(ctx, this);
            // Spec: iterating a closed stream is a `ClosedDirectoryStreamException`,
            // which IS an `IllegalStateException` (that is its declared supertype),
            // so this is the same class of failure real `UnixDirectoryStream`
            // raises — not a substitute for it.
            //
            // The MESSAGE is transcribed, not invented: HotSpot 25.0.3+9 says
            // `Directory stream is closed` (measured 2026-08-16). This body said
            // `directory stream is closed`, lower-case, which is the sort of
            // one-character divergence that fails a differential with every
            // assertion passing.
            if matches!(ctx.get_field(this, ds_base + 1), Value::Int(v) if v != 0) {
                return Err(RuntimeError::IllegalStateException {
                    message: "Directory stream is closed".to_string(),
                }
                .into());
            }
            // `DirectoryStream` is single-use: "@throws IllegalStateException if
            // this directory stream is closed or the iterator has already been
            // returned". Measured on HotSpot: a SECOND `iterator()` raises
            // `IllegalStateException: Iterator already obtained`. This body handed
            // out a fresh iterator over the same listing every time, so a caller
            // that iterated twice by mistake silently processed every entry twice
            // and never learned it had done so. The closed test above runs FIRST,
            // matching the JDK (measured: after `close()` the answer is the closed
            // message even when an iterator had already been obtained).
            // G4-1-the-io-and-nio-fabricated-success-sweep-measured-20260816.md
            if matches!(ctx.get_field(this, ds_base + 2), Value::Int(v) if v != 0) {
                return Err(RuntimeError::IllegalStateException {
                    message: "Iterator already obtained".to_string(),
                }
                .into());
            }
            ctx.set_field(this, ds_base + 2, Value::Int(1));
            // gc-common w15-f: typed so the stale-handle audit sees that the
            // `new_array` arm is this binding's own initialiser, not a GC point
            // after it.
            let arr: ObjectRef = match ctx.get_field(this, ds_base) {
                Value::Object(Some(a)) => a,
                _ => ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0),
            };
            let len = ctx.array_length(arr);
            cratonvm_native_collections::make_iterator_from_array(ctx, arr, len)
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    // Was an unconditional no-op, justified by "this stream owns no OS
    // directory handle". It owns something else that `close()` is contracted
    // to release: `newDirectoryStream` above materialises the ENTIRE listing
    // into a Java Object[] held in slot 0, which a closed stream must not keep
    // alive (a walk over a large tree pinned every directory's listing for as
    // long as the stream object was reachable) and must not keep serving.
    // Drop the array and latch the closed flag; `close()` stays idempotent,
    // as the spec requires.
    r.register(ds, "close", "()V", |ctx, args| {
        if let Some(r) = ds_yield_to_jdk_built(ctx, args, "close", "()V") {
            return r;
        }
        let this = obj_arg(args, 0)?;
        let ds_base = dir_stream_base(ctx, this);
        ctx.set_field(this, ds_base, Value::Object(None));
        ctx.set_field(this, ds_base + 1, Value::Int(1));
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    // Was `null`, which is not a legal answer for anything: `DirectoryStream`
    // extends `Iterable`, whose `spliterator()` default body returns
    // `Spliterators.spliteratorUnknownSize(iterator(), 0)` — never null — so
    // `StreamSupport.stream(ds.spliterator(), false)` (and every for-each that
    // the compiler routes through it) NPE'd instead of "falling back to
    // iterator()". `newDirectoryStream` already materialised the whole listing
    // into the Object[] in slot 0, so hand back the same 3-field
    // (array, pos, fence) synthetic Spliterator the `Spliterator.*` natives and
    // `Spliterators.spliteratorUnknownSize` already build.
    //
    // Kind STATED (wave 8b): `Bridge`. It serves the `jar:`/`jrt:` stream
    // minted on the interface, which has no other `spliterator()`.
    r.register(
        ds,
        "spliterator",
        "()Ljava/util/Spliterator;",
        |ctx, args| {
            if let Some(result) = real_directory_stream_bytecode(
                ctx,
                args,
                "spliterator",
                "()Ljava/util/Spliterator;",
            ) {
                return result;
            }
            let this = obj_arg(args, 0)?;
            // JIT round 16 wave 8b (lane collections): run the JDK's own body.
            // `Iterable.spliterator()` is
            // `Spliterators.spliteratorUnknownSize(iterator(), 0)`, and no
            // `DirectoryStream` implementation overrides it. So call this
            // stream's `iterator()` (the native above, which hands out a real
            // `Arrays$ArrayItr` and sets the single-use latch, as HotSpot's
            // `iterator()` call does) and let the real `Spliterators` bytecode
            // build an `IteratorSpliterator`. HotSpot answers the same way:
            // characteristics 0 and an unknown size.
            //
            // The snapshot branch below minted an instance of the INTERFACE
            // `java/util/Spliterator`. Only the interface-triple
            // `Spliterator.*` natives could serve it, and those are a stated
            // `SyntheticStub` that `--real-jdk` drops
            // (`spliterator_interface_shim_kind`). So
            // `ds.spliterator().forEachRemaining(..)` raised
            // `AbstractMethodError: Spliterator.tryAdvance` in default mode
            // (`R16CollectionsDirSpl`).
            // `CRATONVM_DIR_STREAM_SPLITERATOR_JDK_BODY=0` restores the snapshot.
            //
            // What would let this registration go: a `jar:`/`jrt:` stream
            // minted as a class that inherits `Iterable.spliterator()` (it is
            // minted on the `DirectoryStream` interface today; see
            // `newDirectoryStream`). Every receiver could then take the default
            // body itself.
            // JIT round 20 wave 7 (lane revNat): a stream the JDK's provider
            // built has no listing slots for the snapshot to read
            // (`dir_stream_is_jdk_built`), so it takes this arm whatever the
            // switch says (its `iterator()` runs its own JDK body).
            if true || dir_stream_is_jdk_built(ctx, this)
            {
                let it = ctx.invoke_virtual(this, "iterator", "()Ljava/util/Iterator;", &[])?;
                let Some(it @ Value::Object(Some(_))) = it else {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("DirectoryStream.iterator() returned null".to_string()),
                    }
                    .into());
                };
                return ctx.invoke(
                    "java/util/Spliterators",
                    "spliteratorUnknownSize",
                    "(Ljava/util/Iterator;I)Ljava/util/Spliterator;",
                    &[it, Value::Int(0)],
                );
            }
            let ds_base = dir_stream_base(ctx, this);
            let arr = match ctx.get_field(this, ds_base) {
                Value::Object(Some(a)) => a,
                _ => ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0),
            };
            let len = ctx.array_length(arr) as i32;
            // Pin across the allocation below — a moving young GC there would
            // relocate the listing array (native stale-local family).
            let arr_pin = ctx.pin_native_root(arr);
            let spl = try_alloc_concurrent_synthetic(ctx, "java/util/Spliterator", 3)?;
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.set_field(spl, 0, Value::Object(Some(arr)));
            ctx.set_field(spl, 1, Value::Int(0));
            ctx.set_field(spl, 2, Value::Int(len));
            ctx.unpin_native_roots(arr_pin);
            Ok(Some(Value::Object(Some(spl))))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );

    // The registration half: `iterator`/`close`/`spliterator` on the concrete
    // classes the receiver now wears. `H11-1` -- dispatch keys on the receiver.
    for target in DIR_STREAM_IMPLS {
        cratonvm_native_io::concrete_receiver::mirror_class_registrations(
            r,
            __ds_rows_before,
            ds,
            target,
        );
    }

    // `SecureDirectoryStream<Path>` — the extended interface
    // `sun/nio/fs/UnixSecureDirectoryStream` (the ONLY `DIR_STREAM_IMPLS`
    // entry that implements it) declares. Real bytecode for these methods
    // reads the private `dfd`/`ds` fields a REAL `UnixPath`-backed instance
    // has (an open directory fd, an internal `UnixDirectoryStream`); our
    // minted object carries none of that, only the 3-slot materialised
    // listing `DIR_STREAM_SLOTS` describes. Real bytecode's own
    // `getName(Path)` helper tries to unwrap the argument as a
    // `sun.nio.fs.UnixPath` and throws `ProviderMismatchException` against
    // our synthetic Path, which the JDK itself would never construct.
    //
    // `io.quarkus.bootstrap.util.IoUtils.recursiveDeleteSecure` — used by
    // EVERY Quarkus test/bootstrap path that tears down a temp dir,
    // including `ForkedJvmEnvironment.close()`
    // (`ClassLoadingChainAnalyzerTest`'s forked-JVM harness) — casts every
    // `Files.newDirectoryStream(dir)` result to `SecureDirectoryStream` and
    // drives exactly these four methods, so ANY temp-dir cleanup through it
    // hit this `ProviderMismatchException` before ever reaching `close()`'s
    // caller. Implemented here by resolving the entry against the stream's
    // OWN directory (derived from any already-materialised sibling Path,
    // all of which share the same parent) and delegating to the working
    // `Files` natives, the same way `FileSystemProvider.delete` above
    // delegates rather than re-implementing.
    let sds = "sun/nio/fs/UnixSecureDirectoryStream";

    fn sds_own_dir(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<String> {
        let ds_base = dir_stream_base(ctx, this);
        let arr = match ctx.get_field(this, ds_base) {
            Value::Object(Some(a)) => a,
            _ => return None,
        };
        if ctx.array_length(arr) == 0 {
            return None;
        }
        let mut first = match ctx.get_array_element(arr, 0) {
            Value::Object(Some(p)) => p,
            _ => return None,
        };
        let entry = p57_read_path(ctx, &mut first);
        entry.rfind('/').map(|i| entry[..i].to_string())
    }

    fn sds_resolve_entry(
        ctx: &mut dyn NativeContext,
        this: ObjectRef,
        mut entry_obj: ObjectRef,
    ) -> Option<ObjectRef> {
        // gc-common w21-b: `sds_own_dir` reads a sibling Path, which can run
        // Java (a foreign Path's `toString()`); the entry is pinned across it
        // and read through its current address. The callers' pins protected
        // THEIR copies, not the one passed in here by value.
        let entry_pin = ctx.pin_native_root(entry_obj);
        let dir = sds_own_dir(ctx, this);
        entry_obj = ctx.read_native_pin(entry_pin, entry_obj);
        ctx.unpin_native_roots(entry_pin);
        let dir = dir?;
        let name = p57_read_path(ctx, &mut entry_obj);
        // The entry may already be one of the fully-qualified Paths this
        // stream's own `iterator()` handed out (`Path p` in
        // `recursiveDeleteSecure`'s for-each, passed straight through
        // without `getFileName()`), or a bare relative name
        // (`p.getFileName()`, what `recursiveDeleteSecure` actually passes).
        // Only join when it is not already rooted under this directory.
        let resolved = if name.starts_with(&dir) {
            name
        } else {
            format!("{dir}/{name}")
        };
        p57_alloc_path(ctx, &resolved).ok()
    }

    r.register(
        sds,
        "getFileAttributeView",
        "(Ljava/lang/Object;Ljava/lang/Class;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/FileAttributeView;",
        |ctx, args| {
            if let Some(r) = ds_yield_to_jdk_built(
                ctx,
                args,
                "getFileAttributeView",
                "(Ljava/lang/Object;Ljava/lang/Class;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/FileAttributeView;",
            ) {
                return r;
            }
            let this = obj_arg(args, 0)?;
            let entry_obj = match obj_arg(args, 1) {
                Ok(p) => p,
                Err(_) => return Ok(Some(Value::Object(None))),
            };
            let view_name = obj_arg(args, 2)
                .ok()
                .and_then(|m| crate::lang_class::mirror_class_name(ctx, m))
                .unwrap_or_default();
            let is_dos = view_name.ends_with("DosFileAttributeView");
            let is_basic = view_name.ends_with("BasicFileAttributeView")
                || view_name.ends_with("/FileAttributeView")
                || view_name == "java/nio/file/attribute/FileAttributeView";
            let is_posix = !cfg!(windows)
                && (view_name.ends_with("PosixFileAttributeView")
                    || view_name.ends_with("FileOwnerAttributeView"));
            if !(is_dos || is_basic || is_posix) {
                return Ok(Some(Value::Object(None)));
            }
            let vclass = if is_dos {
                "java/nio/file/attribute/DosFileAttributeView"
            } else if is_posix {
                "java/nio/file/attribute/PosixFileAttributeView"
            } else {
                "java/nio/file/attribute/BasicFileAttributeView"
            };
            let this_pin = ctx.pin_native_root(this);
            let entry_pin = ctx.pin_native_root(entry_obj);
            let resolved = sds_resolve_entry(ctx, this, entry_obj);
            ctx.unpin_native_roots(entry_pin);
            ctx.unpin_native_roots(this_pin);
            let resolved = match resolved {
                Some(p) => p,
                None => return Ok(Some(Value::Object(None))),
            };
            let resolved_pin = ctx.pin_native_root(resolved);
            let vclass = p57_concrete_view_class(ctx, vclass);
            let view = try_alloc_concurrent_synthetic(ctx, vclass, 1)?;
            let resolved = ctx.read_native_pin(resolved_pin, resolved);
            ctx.set_field(view, 0, Value::Object(Some(resolved)));
            ctx.unpin_native_roots(resolved_pin);
            Ok(Some(Value::Object(Some(view))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(sds, "deleteFile", "(Ljava/lang/Object;)V", |ctx, args| {
        if let Some(r) = ds_yield_to_jdk_built(ctx, args, "deleteFile", "(Ljava/lang/Object;)V") {
            return r;
        }
        let this = obj_arg(args, 0)?;
        let entry_obj = obj_arg(args, 1)?;
        let this_pin = ctx.pin_native_root(this);
        let entry_pin = ctx.pin_native_root(entry_obj);
        let resolved = sds_resolve_entry(ctx, this, entry_obj);
        ctx.unpin_native_roots(entry_pin);
        ctx.unpin_native_roots(this_pin);
        let resolved = match resolved {
            Some(p) => p,
            None => {
                return Err(RuntimeError::NoSuchFileException {
                    path: String::new(),
                }
                .into())
            }
        };
        native_files_delete(ctx, &[Value::Object(Some(resolved))])?;
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    r.register(
        sds,
        "deleteDirectory",
        "(Ljava/lang/Object;)V",
        |ctx, args| {
            if let Some(r) =
                ds_yield_to_jdk_built(ctx, args, "deleteDirectory", "(Ljava/lang/Object;)V")
            {
                return r;
            }
            let this = obj_arg(args, 0)?;
            let entry_obj = obj_arg(args, 1)?;
            let this_pin = ctx.pin_native_root(this);
            let entry_pin = ctx.pin_native_root(entry_obj);
            let resolved = sds_resolve_entry(ctx, this, entry_obj);
            ctx.unpin_native_roots(entry_pin);
            ctx.unpin_native_roots(this_pin);
            let resolved = match resolved {
                Some(p) => p,
                None => {
                    return Err(RuntimeError::NoSuchFileException {
                        path: String::new(),
                    }
                    .into())
                }
            };
            native_files_delete(ctx, &[Value::Object(Some(resolved))])?;
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        sds,
        "newDirectoryStream",
        "(Ljava/lang/Object;[Ljava/nio/file/LinkOption;)Ljava/nio/file/SecureDirectoryStream;",
        |ctx, args| {
            if let Some(r) = ds_yield_to_jdk_built(
                ctx,
                args,
                "newDirectoryStream",
                "(Ljava/lang/Object;[Ljava/nio/file/LinkOption;)Ljava/nio/file/SecureDirectoryStream;",
            ) {
                return r;
            }
            let this = obj_arg(args, 0)?;
            let entry_obj = obj_arg(args, 1)?;
            let this_pin = ctx.pin_native_root(this);
            let entry_pin = ctx.pin_native_root(entry_obj);
            let resolved = sds_resolve_entry(ctx, this, entry_obj);
            ctx.unpin_native_roots(entry_pin);
            ctx.unpin_native_roots(this_pin);
            let resolved = match resolved {
                Some(p) => p,
                None => {
                    return Err(RuntimeError::NoSuchFileException {
                        path: String::new(),
                    }
                    .into())
                }
            };
            ctx.invoke(
                "java/nio/file/Files",
                "newDirectoryStream",
                "(Ljava/nio/file/Path;)Ljava/nio/file/DirectoryStream;",
                &[Value::Object(Some(resolved))],
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    
    // createSymbolicLink / createLink / readSymbolicLink.
    //
    // `Files.createSymbolicLink(link, target, attrs)` is ordinary (non-native)
    // JDK bytecode: `provider(link).createSymbolicLink(link, target, attrs);
    // return link;`. The default provider this VM hands back is the synthetic
    // instance stamped as the literal `java/nio/file/spi/FileSystemProvider`
    // class (see the `getFileStore` note above), so the invokevirtual lands on
    // `FileSystemProvider`'s OWN concrete body — which, on the abstract base,
    // is an unconditional `throw new UnsupportedOperationException()`. That is
    // exactly what
    // docs/feature-designs/native-dispatch-architecture.md
    // reported: every `Files.createSymbolicLink` call in the VM died with a
    // bare `UnsupportedOperationException` at `FileSystemProvider.java:626`,
    // taking out `ConfigTreePropertySourceTests` (Kubernetes ConfigMap
    // `..data`-symlink shapes) and `FileWatcherTests` (symlink-following
    // watch registration).
    //
    // Registering the three link methods HERE, on `fsp`, is what makes them
    // reachable: a registration on `java/nio/file/Files` (there were three,
    // now real, further down this file) never runs for these calls because
    // `Files.createSymbolicLink` has real bytecode of its own and this VM
    // prefers concrete bytecode over a bridge registration.
    r.register(
        fsp,
        "createSymbolicLink",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/attribute/FileAttribute;)V",
        |ctx, args| {
            let mut link = obj_arg(args, 1)?;
            let mut target = obj_arg(args, 2)?;
            let (link, target) = p57_read_path_pair(ctx, &mut link, &mut target);
            p57_create_symbolic_link(ctx, &link, &target)?;
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        fsp,
        "createLink",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;)V",
        |ctx, args| {
            let mut link = obj_arg(args, 1)?;
            let mut existing = obj_arg(args, 2)?;
            let (link, existing) = p57_read_path_pair(ctx, &mut link, &mut existing);
            p57_create_hard_link(ctx, &link, &existing)?;
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        fsp,
        "readSymbolicLink",
        "(Ljava/nio/file/Path;)Ljava/nio/file/Path;",
        |ctx, args| {
            let mut path_obj = obj_arg(args, 1)?;
            let p = p57_read_path(ctx, &mut path_obj);
            p57_read_symbolic_link(ctx, &p)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // Build a REAL `sun.nio.ch.FileChannelImpl` over an already-open fd
    // (FileDescriptor.handle = fd_table id, exactly how
    // FileInputStream/FileOutputStream back their channels), so
    // `read`/`write`/`size`/`position`/`close` resolve to FileChannelImpl's
    // CONCRETE bytecode instead of a synthetic shim. Shared by
    // `newFileChannel` below (host files) and `vfs_entry_as_real_channel`
    // (jar/nested-zip/jrt entries spilled to a temp file) — extracted so both
    // callers get the identical, already-verified construction rather than a
    // second hand-written copy of it.

    // Spill an in-memory VFS entry (jar / nested-zip / jrt — anything
    // `vfs_read` served from a byte vector rather than a host fd) to a
    // uniquely-named real temp file and open a read-only real
    // `FileChannelImpl` over it via `open_real_filechannel` above.
    //
    // Before this, `FileSystemProvider.newByteChannel`'s jar-entry branch
    // allocated its return value AS the bare `java/nio/channels/
    // SeekableByteChannel` interface (see the `vfs_read` call below) — no
    // concrete class ever overrides `read`/`size`/`position`/`close`, so
    // JUnit's `Files.readAllBytes` on any nested-jar/zip resource dispatched
    // straight to the interface's abstract declarations:
    // `AbstractMethodError: method java/nio/channels/SeekableByteChannel.
    // size()J has no Code attribute`. This is the SAME defect class as
    // `SC-resource-io-family.md` Cause A (fixed for real host files by
    // delegating to `newFileChannel`'s fd_table-backed FileChannelImpl) —
    // that fix explicitly left jar entries on the old stub because they have
    // no host fd to delegate to. Spilling to a temp file gives them one,
    // reusing the identical, already-verified FileChannelImpl construction
    // rather than hand-rolling a second one over an in-memory buffer.
    // Read-only: a jar/zip entry has no writable form through this door.

    // newFileChannel(Path, Set<? extends OpenOption>, FileAttribute[]) -> FileChannel
    // Real JDK delegates to WindowsFileSystemProvider.newFileChannel which
    // overrides this abstract method. We provide a synthetic FileChannel
    // backed by fd_table (same shape as RandomAccessFile.getChannel) so the
    // existing j.n.c.FileChannel native methods can drive it.
    r.register(
        fsp,
        "newFileChannel",
        "(Ljava/nio/file/Path;Ljava/util/Set;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/channels/FileChannel;",
        |ctx, args| {
            // `args` IS A PRE-CALL SNAPSHOT. `safe_native_call_impl` pins every
            // argument, so nothing here can be collected — but it rebuilds the
            // snapshot from those pins only for a collection it runs itself,
            // BEFORE the callback. `p57_read_path` allocates, and any other
            // thread can request a collection at any safepoint, so by the time
            // `args.get(2)` is read below the option set may have MOVED and the
            // slice still names its old address. Reading it here, before the
            // first allocation, and keeping it in a handle is what makes the
            // two `set` dereferences below safe.
            //
            // Same family as the FileDescriptor construction further down, and
            // as the print natives fixed on 2026-09-06 — see
            // `internal/fixed-bugs/native-arg-snapshot-stale-across-java-reentry-FIXED-20260906.md`.
            let path_obj = obj_arg(args, 1)?;
            let set_obj_at_entry = match args.get(2) {
                Some(Value::Object(Some(o))) => Some(*o),
                _ => None,
            };
            let (mut writable, mut create, mut append, mut read_opt, mut truncate) =
                (false, false, false, false, false);
            let by_name = true;
            let attrs_at_entry = match args.get(3) {
                Some(Value::Object(Some(o))) => Some(*o),
                _ => None,
            };
            let mut create_mode: Option<u32> = None;
            let (p, set_obj) = {
                let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
                let path_h = scope.root(path_obj);
                let set_h = set_obj_at_entry.map(|o| scope.root(o));
                let attrs_h = attrs_at_entry.map(|o| scope.root(o));
                let mut path_now = scope.get(&path_h);
                let p = p57_read_path(&mut *scope, &mut path_now);
                // Inspect option set: look for WRITE/APPEND/CREATE/CREATE_NEW/
                // READ/TRUNCATE_EXISTING via toString. JIT round 22 wave 2,
                // lane sync (W3L1-4): with `CRATONVM_COMPAT_OPEN_OPTIONS_BY_NAME`
                // (default ON) the five booleans come from
                // `fsp_scan_open_options`' exact names below instead.
                if let Some(h) = set_h.as_ref().filter(|_| !by_name) {
                    let set = scope.get(h);
                    // Try to iterate by calling toString() on the Set first
                    // (cheap & robust)
                    if let Ok(Some(Value::Object(Some(s)))) =
                        scope.invoke_virtual(set, "toString", "()Ljava/lang/String;", &[])
                    {
                        let s = scope.read_string(s).unwrap_or_default();
                        writable = s.contains("WRITE") || s.contains("APPEND");
                        create = s.contains("CREATE"); // matches CREATE and CREATE_NEW
                        append = s.contains("APPEND");
                        read_opt = s.contains("READ");
                        truncate = s.contains("TRUNCATE_EXISTING");
                    }
                }
                // `toString` ran interpreted bytecode; re-read the set for the
                // `fsp_scan_open_options` call below rather than handing it the
                // address it had before.
                let set_now = set_h.as_ref().map(|h| scope.get(h));
                // A `FileAttribute` mode (`PosixFilePermissions.asFileAttribute`)
                // is the `open(2)` mode of a file this call creates.
                if let Some(h) = attrs_h.as_ref() {
                    let attrs = Value::Object(Some(scope.get(h)));
                    create_mode = posix_mode_from_file_attributes(&mut *scope, Some(&attrs));
                }
                (p, set_now)
            };
            // JIT round 22 wave 3, lane sync (review item 2): under the switch the
            // set is read by identity (`p57_scan_open_options_by_identity`):
            // an unknown option is the JDK's `UnsupportedOperationException`,
            // a null one its NPE, and `DELETE_ON_CLOSE` is honoured below.
            let scanned = if by_name {
                p57_scan_open_options_by_identity(ctx, set_obj)?
            } else {
                fsp_scan_open_options(ctx, set_obj.map(|o| Value::Object(Some(o))))
            };
            if by_name {
                // `UnixChannelFactory.Flags.toFlags`: each option by identity;
                // CREATE_NEW is its own flag, and an
                // open creates for either (`O_CREAT`, plus `O_EXCL` below).
                writable = scanned.write || scanned.append;
                create = scanned.create || scanned.create_new;
                append = scanned.append;
                read_opt = scanned.read;
                truncate = scanned.truncate_existing;
            }
            // JDK FileChannel.open contract: a channel with neither READ nor
            // WRITE is read-only; WRITE without READ is write-only.
            let readable = read_opt || !writable;
            // `UnixChannelFactory.newFileChannel`'s two refusals, before any
            // open (HotSpot 25: `IllegalArgumentException("READ + APPEND not
            // allowed")` / `("APPEND + TRUNCATE_EXISTING not allowed")`).
            // nio20 wave 3, L1.
            if append && read_opt {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "READ + APPEND not allowed".to_string(),
                }
                .into());
            }
            if append && truncate {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "APPEND + TRUNCATE_EXISTING not allowed".to_string(),
                }
                .into());
            }
            // NOFOLLOW_LINKS: refuse a symlink final component before the open.
            // This registration is the funnel for `FileChannel.open`, for
            // `Files.newByteChannel` (which delegates here) and for the JDK's
            // own `newOutputStream`/`newInputStream` defaults, so one check here
            // covers all four. `set_obj`'s toString already told us the answer.
            // (`scanned` is taken above, before the refusals, since JIT round
            // 22 wave 2.)
            if scanned.nofollow {
                if let Some(refused) = p57_nofollow_reject(&p) {
                    return Err(refused);
                }
            }
            // CREATE_NEW is `O_CREAT | O_EXCL`: an existing file is
            // `FileAlreadyExistsException` (nio20 wave 3, L1). `newByteChannel`
            // checked it before delegating here; `FileChannel.open` reaches
            // this door directly and opened the existing file. The `toString`
            // scan above folds CREATE_NEW into `create` and cannot tell.
            if scanned.create_new && std::path::Path::new(&p).exists() {
                return Err(p57_file_already_exists(ctx, &p));
            }
            // `open(2)` applies `mode & ~umask` to a file it creates; the
            // fd-table opener has no mode parameter, so create the file here
            // and let it open the existing one.
            #[cfg(unix)]
            if create && writable && create_mode.is_some() && !std::path::Path::new(&p).exists() {
                use std::os::unix::fs::OpenOptionsExt;
                let _ = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(create_mode.unwrap_or(0o666))
                    .open(&p);
            }
            let _ = create_mode;
            // Real `FileChannel.open` throws `java.nio.file.NoSuchFileException`
            // (not a bare `IOException`) when the target is missing — callers
            // like `FileSystemResource.readableChannel()` explicitly catch
            // `NoSuchFileException` and translate it to `FileNotFoundException`
            // (ResourceTests#resourceCreateRelativeUnknown). Mapping every open
            // failure to a generic `IOException` made that catch miss, so the
            // raw `IOException` propagated to the caller instead.
            // GAP I2: these openers used to call `fd_table()` directly, so the
            // whole `java.nio.file` surface bypassed every path policy in the
            // VM. Route through the capability gate, which runs the check
            // before the fd is reserved and before the syscall. With no policy
            // installed (today's default) this is the same call as before.
            let gated = if append {
                // `O_APPEND` on the descriptor itself (nio20 wave 3, L1): the
                // channel used to be a plain read-write fd seeked to the end
                // once, so `position(0)` + `write` overwrote the head of the
                // file and `transferTo` / `transferFrom` wrote at their own
                // offsets (`Nio20Conf` `append`, `append-transfer`,
                // `append-disk`). Create exactly when asked; never truncate
                // (refused above).
                crate::capability_gate::open_write_options_gated(&*ctx, &p, create, false, true)
            } else if writable {
                crate::capability_gate::open_read_write_gated(&*ctx, &p, create)
            } else {
                // Read-only request: try read+write first (a seekable fd), and
                // fall back to a plain read. A `FileWrite` denial takes the
                // same fallback an `EACCES` would, which is the right answer —
                // the caller only asked to read.
                match crate::capability_gate::open_read_write_gated(&*ctx, &p, false) {
                    Ok(fd) => Ok(fd),
                    Err(_) => crate::capability_gate::open_read_gated(&*ctx, &p),
                }
            };
            let fd_id = match gated {
                Ok(fd) => fd,
                // A refusal is a `SecurityException`. It must NOT become
                // `NoSuchFileException`: callers such as
                // `FileSystemResource.readableChannel()` catch that one and
                // recover, which would silently swallow the policy decision.
                Err(cratonvm_native_api::fd_table::FdCapabilityError::Denied(denied)) => {
                    return Err(denied.into())
                }
                Err(cratonvm_native_api::fd_table::FdCapabilityError::Io(e)) => {
                    // `Files.newByteChannel(<a directory>, WRITE)` funnels
                    // here. On Linux HotSpot answers `FileSystemException:
                    // <path>: Is a directory` (EISDIR); on Windows there is no
                    // EISDIR — opening a directory for write is refused by the
                    // OS itself as access-denied, and HotSpot answers
                    // `AccessDeniedException`. A bare `IOException` loses both
                    // the type and the path either way.
                    return Err(match e.kind() {
                        std::io::ErrorKind::NotFound => {
                            RuntimeError::NoSuchFileException { path: p.clone() }.into()
                        }
                        std::io::ErrorKind::IsADirectory => {
                            p57_filesystem_exception(ctx, &p, None, "Is a directory")?
                        }
                        std::io::ErrorKind::PermissionDenied => p57_access_denied(ctx, &p)?,
                        _ => MethodCallFailed::from(RuntimeError::IOException {
                            message: format!("Cannot open {}: {}", p, e),
                        }),
                    })
                }
            };
            if truncate && writable {
                let _ = ctx.fd_table().rw_set_length(fd_id, 0);
            }
            if append {
                if let Ok(sz) = ctx.fd_table().file_size(fd_id) {
                    let _ = ctx.fd_table().rw_seek(fd_id, std::io::SeekFrom::Start(sz));
                }
            }
            // JIT round 22 wave 3, lane sync (review item 2): `DELETE_ON_CLOSE`.
            // `UnixChannelFactory` unlinks the name right after the open and
            // ignores a failure; the descriptor keeps the file until close. On
            // Windows the fd-table's handle is opened with delete sharing, so the
            // same call marks it delete-pending, removed when the last handle
            // closes -- what `FILE_FLAG_DELETE_ON_CLOSE` gives HotSpot there.
            if scanned.delete_on_close {
                let _ = std::fs::remove_file(&p);
            }

            // RECONCILE-WITH-REAL: build a REAL `sun.nio.ch.FileChannelImpl`
            // over the fd (FileDescriptor.handle = fd_table id, exactly how
            // FileInputStream/FileOutputStream back their channels) and return
            // it. `read/write/size/position/truncate` then resolve to
            // FileChannelImpl's CONCRETE bytecode → the working
            // IOUtil→FileDispatcherImpl native path (which now routes the temp
            // direct-buffer arena handle correctly). Native dispatch is keyed
            // on the resolved method's declaring class, so a concrete-class
            // receiver never hits the abstract-`FileChannel` synthetic shims —
            // unlike the legacy synthetic channel below, whose shims assume a
            // synthetic ByteBuffer layout and corrupt real heap buffers.
            //
            // FileChannelImpl.open(fd, path, readable, writable, sync, direct,
            //   parent) — mirrors FileOutputStream.getChannel's call shape.
            let real_channel = open_real_filechannel(ctx, fd_id, &p, readable, writable, append);
            if let Some(v) = real_channel {
                return Ok(Some(v));
            }

            // Legacy synthetic fallback (only if the real FileChannelImpl
            // construction is unavailable — keeps prior behavior intact).
            //
            // This is the ONE producer of a literal `java/nio/channels/FileChannel`
            // that is live in real-JDK mode: `native_fc_open` is dropped there by
            // the `drop_real_layout_synthetic` filter in `native-api`'s registry,
            // scoped to `open` by name. The width and the slot both come from
            // `synthetic_file_channel` so this fallback and every accessor in both
            // crates share one map — W7-72-ssc-socket-and-filechannel.md. Writing
            // the fd into slot 0 here used to put an `Int` in the real `closeLock`
            // reference, where the descriptor coercion degraded it to null and the
            // fd was lost outright.
            let fc_slots = cratonvm_native_api::synthetic_file_channel::alloc_slots(ctx);
            let fc =
                try_alloc_concurrent_synthetic(ctx, "java/nio/channels/FileChannel", fc_slots)?;
            cratonvm_native_api::synthetic_file_channel::set_fd_value(
                ctx,
                fc,
                Value::Int(fd_id as i32),
            );
            Ok(Some(Value::Object(Some(fc))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // `p57_alloc_provider` now mints a FileSystemProvider stamped as the real
    // concrete platform class (see its own doc), so dispatch on a REAL
    // provider receiver keys on ITS name first (`H11-1`). Mirror every row
    // this block registered under the abstract name onto each concrete class
    // in `FILE_SYSTEM_PROVIDER_IMPLS` -- same `for target in IMPLS` shape
    // `FILE_STORE_IMPLS`'s mirror uses -- or a receiver this crate minted
    // would find neither its own registration nor real bytecode.
    for target in FILE_SYSTEM_PROVIDER_IMPLS {
        cratonvm_native_io::concrete_receiver::mirror_class_registrations(
            r,
            __fsp_rows_before,
            fsp,
            target,
        );
    }

    // Round 63 — Kafka 4.2.0 calls `FileChannel.size()` on the synthetic
    // FileChannel returned by `newFileChannel` above (via
    // `BatchFileReader.build → FileRecords.<init>`). The real JDK
    // FileChannel declares `size()` abstract — without a native here we
    // throw AbstractMethodError and Kafka exits silently through its
    // outer `Throwable` catch + `Exit.exit(1)`. Register a minimal set
    // of FileChannel natives that drive the synthetic via fd_table.
    let fc_cls = "java/nio/channels/FileChannel";
    // THE SEVEN SHARED TRIPLES. This registrar is the one `vm/src/vm/vm_init.rs`
    // calls directly in BOTH shipping arms (`:2217` and `:2763`), so in
    // `--fast-boot` and `--real-jdk` these registrations are the ONLY ones that
    // exist for these seven triples. `register_phase57_file_channel` registers
    // the same seven, later within `register_phase57_natives` (line 21 vs line
    // 36), and that registrar is reachable ONLY from `register_synthetic_overrides`
    // — so it exists at all only under `--synthetic-jdk`, where it therefore
    // WINS by running last.
    //
    // Per mode, stated because getting this backwards is what left W7-8 §3.1's
    // fixes in a registrar neither shipping mode calls:
    //
    //   `--fast-boot`, `--real-jdk`  → only THIS registrar's copies exist.
    //   `--synthetic-jdk`           → both exist; the file-channel one wins.
    //
    // Both now name the SAME functions, so the outcome no longer depends on the
    // ordering at all. That is the fix, and it is why neither registration was
    // deleted: deleting this one would change the shipping modes' behaviour to
    // whatever the synthetic registrar does *only if that registrar existed*,
    // which in a default build it does not; deleting the other would change only
    // `--synthetic-jdk`, which is not the mode that had the defect.
    //
    // The bodies live next to `register_phase57_file_channel` (`p57_fc_*`),
    // beside the five triples that are still synthetic-only.
    r.register(fc_cls, "size", "()J", p57_fc_size, cratonvm_native_api::NativeKind::Bridge);
    r.register(fc_cls, "position", "()J", p57_fc_position, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        fc_cls,
        "position",
        "(J)Ljava/nio/channels/FileChannel;",
        p57_fc_position_set, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(fc_cls, "close", "()V", p57_fc_close, cratonvm_native_api::NativeKind::Bridge);
    r.register(fc_cls, "isOpen", "()Z", p57_fc_is_open, cratonvm_native_api::NativeKind::Bridge);
    // `FileChannel.write` is abstract; cassandra's
    // BufferedDataOutputStreamPlus.doFlush drives a synthetic FileChannel
    // (from `newFileChannel`) through it.
    r.register(fc_cls, "write", "(Ljava/nio/ByteBuffer;)I", p57_fc_write, cratonvm_native_api::NativeKind::Bridge);
    r.register(fc_cls, "read", "(Ljava/nio/ByteBuffer;)I", p57_fc_read, cratonvm_native_api::NativeKind::Bridge);

    // --- Files additional methods ---
    // `Files.probeContentType` has no native and its real bytecode reaches
    // `sun.nio.fs.DefaultFileTypeDetector.create()`, which calls
    // `getFileTypeDetector()` on `DefaultFileSystemProvider.instance()`. That
    // instance is minted by `p57_alloc_provider` as the ABSTRACT
    // `java/nio/file/spi/FileSystemProvider` itself, which declares no such
    // method — so the call died with
    //
    //   NoSuchMethodError: java.nio.file.spi.FileTypeDetector
    //                      java.nio.file.spi.FileSystemProvider.getFileTypeDetector()
    //
    // and took the whole probe run with it. The FABRICATED ABSTRACT PROVIDER is
    // the real defect and is recorded separately (it is the same shape as the
    // `MemorySegment`-as-an-interface row in the roadmap's Phase 1); this
    // registration closes the entry point rather than the cause.
    //
    // The answer is contract-correct rather than a stub: `probeContentType` is
    // specified to return "the content type, or null if the content type
    // cannot be determined", and the JDK's own Linux detector is a
    // `/etc/mime.types` lookup keyed on the file extension. This table is that
    // lookup for the types a program is likely to ask about, and null for
    // everything else — which is exactly what an installed JDK answers on a
    // host with no `mime.types` file.
    reg_files! { r.register(files,
        "probeContentType",
        "(Ljava/nio/file/Path;)Ljava/lang/String;",
        |ctx, args| {
            let mut path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, &mut path_obj);
            let ext = p
                .rsplit(['/', '\\'])
                .next()
                .and_then(|name| name.rsplit_once('.'))
                .map(|(_, e)| e.to_ascii_lowercase())
                .unwrap_or_default();
            let mime = match ext.as_str() {
                "txt" | "text" | "log" | "properties" | "conf" => Some("text/plain"),
                "html" | "htm" => Some("text/html"),
                "css" => Some("text/css"),
                "csv" => Some("text/csv"),
                "xml" => Some("text/xml"),
                "js" | "mjs" => Some("text/javascript"),
                "json" => Some("application/json"),
                "pdf" => Some("application/pdf"),
                "zip" => Some("application/zip"),
                "jar" => Some("application/java-archive"),
                "gz" => Some("application/gzip"),
                "class" => Some("application/octet-stream"),
                "png" => Some("image/png"),
                "jpg" | "jpeg" => Some("image/jpeg"),
                "gif" => Some("image/gif"),
                "svg" => Some("image/svg+xml"),
                _ => None,
            };
            Ok(Some(match mime {
                Some(m) => Value::Object(Some(ctx.create_string(m))),
                None => Value::Object(None),
            }))
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }
    // The `obj_arg` in front of each of these is the null check. Every
    // `Files` method opens with `provider(path)`, i.e. `path.getFileSystem()`,
    // so a null path is an NPE and not an answer. MEASURED: `Files.exists(null)`
    // answered FALSE and `Files.isDirectory(null)` answered FALSE — the shape
    // that makes `if (!Files.exists(p))` take the "create it" branch for a
    // caller whose `p` is null by mistake.
    reg_files! { r.register(files,
        "exists",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z",
        |ctx, args| {
            obj_arg(args, 0)?;
            Ok(Some(Value::Int(i32::from(p57_files_exists_impl(
                ctx, args,
            )))))
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    reg_files! { r.register(files,
        "notExists",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z",
        |ctx, args| {
            obj_arg(args, 0)?;
            Ok(Some(Value::Int(i32::from(!p57_files_exists_impl(
                ctx, args,
            )))))
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    reg_files! { r.register(files,
        "newInputStream",
        "(Ljava/nio/file/Path;[Ljava/nio/file/OpenOption;)Ljava/io/InputStream;",
        // args[0] = the Path (static method).
        |ctx, args| fsp_new_input_stream(ctx, args, 0), cratonvm_native_api::NativeKind::Bridge,
    ) }

    // `Files.newBufferedReader` — RETIRED 2026-08-28 (lane L4), both overloads.
    //
    // The two bodies that stood here read the WHOLE FILE with
    // `p57_read_to_string` and handed back a reader over the resulting string.
    // The real method is
    //
    //     new BufferedReader(new InputStreamReader(Files.newInputStream(path), cs))
    //
    // which STREAMS, and whose `Files.newInputStream` is registered a few lines
    // above and matches HotSpot on every input this lane measured — including
    // the one row the eager readers got wrong:
    //
    //   Files.newBufferedReader(<a directory>)
    //     HotSpot   no-throw at open; IOException on the first read
    //     shim      IOException at open   (std::fs::read of a directory)
    //
    // So the shim was buying a whole-file read, in memory, at open, in exchange
    // for a refusal at the wrong moment. Retiring it is one fix for both.
    // `probes/L4FilesSweep.java` covers the open/read/refusal rows.

    // RWF86.1 (gated): native shims for BufferedReader.read([CII)I and read()I.
    //
    // These delegate `read` straight to the Reader at slot 0, BYPASSING
    // java.io.BufferedReader's own buffer (`cb`/`nChars`/`nextChar`) and its
    // mark()/reset() bookkeeping. That is only correct for the *synthetic*
    // BufferedReader layout (slot 0 = wrapped Reader, no real JDK fields), which
    // only exists under the `synthetic-jdk` feature. In the default real-JDK
    // build every BufferedReader is a genuine JDK instance, and shadowing its
    // `read` with this passthrough silently broke mark()/reset(): after a
    // `mark(); read(); reset()` the chars consumed by the native read were lost
    // because the real reset() rewinds buffer indices the native never advanced.
    // H2's RUNSCRIPT/CSV/INIT BOM-skip (`mark(1); read(); reset()`) dropped the
    // first character of every script — `"create table"` parsed as `"reate
    // table"`. `Files.newBufferedReader` now builds a real BufferedReader, so
    // the side-table is never populated and these shims serve no real-JDK use.
    //
    // 2026-09-17: the cfg ALONE was the wrong guard, for the reason
    // `NativeMethodRegistry::drops_real_layout_synthetic` spells out and that
    // the H2 paragraph above already measured: a `--features synthetic-jdk`
    // binary run against a real JDK satisfies the cfg and registers these
    // OVER the real `java.io.BufferedReader.read`/`read(char[],int,int)`,
    // bypassing `cb`/`nChars`/`nextChar`/mark()/reset() exactly as measured —
    // this is the H2 defect, live, not hypothetical. Gate on the runtime
    // predicate too, the way the `native-io` twin of this block already does.
    #[cfg(feature = "synthetic-jdk")]
    if !r.drops_real_layout_synthetic() {
        r.register("java/io/BufferedReader", "read", "([CII)I", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let out_arr = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Int(-1))),
            };
            let off = match args.get(2) {
                Some(Value::Int(v)) => *v as usize,
                _ => 0,
            };
            let len = match args.get(3) {
                Some(Value::Int(v)) => *v as usize,
                _ => 0,
            };
            if len == 0 {
                return Ok(Some(Value::Int(0)));
            }
            let out_len = ctx.array_length(out_arr);
            if off > out_len || off.saturating_add(len) > out_len {
                return Ok(Some(Value::Int(-1)));
            }
            // Side-table-backed reader (from Files.newBufferedReader).
            if let Some(n) = br_sidetable_read_chars(ctx, this, out_arr, off, len) {
                return Ok(Some(Value::Int(n)));
            }
            // Fallback: delegate to underlying Reader at slot 0.
            match ctx.get_field(this, 0) {
                Value::Object(Some(inner)) => {
                    let r = ctx.invoke_virtual(
                        inner,
                        "read",
                        "([CII)I",
                        &[
                            Value::Object(Some(out_arr)),
                            Value::Int(off as i32),
                            Value::Int(len as i32),
                        ],
                    )?;
                    Ok(r.or(Some(Value::Int(-1))))
                }
                _ => Ok(Some(Value::Int(-1))),
            }
        }, cratonvm_native_api::NativeKind::Bridge);
        r.register("java/io/BufferedReader", "read", "()I", |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Some(c) = br_sidetable_read_one(ctx, this) {
                return Ok(Some(Value::Int(c)));
            }
            match ctx.get_field(this, 0) {
                Value::Object(Some(inner)) => {
                    let r = ctx.invoke_virtual(inner, "read", "()I", &[])?;
                    Ok(r.or(Some(Value::Int(-1))))
                }
                _ => Ok(Some(Value::Int(-1))),
            }
        }, cratonvm_native_api::NativeKind::Bridge);
    } // end BufferedReader.read shims (cfg + !drops_real_layout_synthetic)

    // (The APPEND-only OpenOption[] scanner that used to live here is gone:
    // `fsp_scan_open_options` reads the same array and also reports CREATE_NEW
    // and NOFOLLOW_LINKS, which this path has to honour too.)

    // DELETED 2026-08-05: the fd-backed `Files.newBufferedWriter` that used to
    // live here, and the `CRATONVM_SYNTHETIC_BUFFERED_WRITER` flag that gated
    // it back on.
    //
    // It allocated a `java/io/BufferedWriter`, wrote the OS file descriptor
    // into raw slot 0 — `java.io.Writer.writeBuffer` on the real layout, a
    // `char[]` the JDK owns — and left the six BufferedWriter natives below to
    // recognise their own object by asking whether that slot held an `Int`.
    // That is kind 3 in
    // `jdk-only-fabricated-object-layouts-FIXED-20260810.md`:
    // a VM value with no real field to live in.
    //
    // It was already default-OFF (real bytecode has been the default since
    // 2026-06-18, because this path "silently DROPPED all character data"), and
    // measuring the flagged arm before touching it showed it had no working
    // configuration left at all: `probes/BufferedWriterDiscriminatorProbe`
    // produced ZERO bytes for every one of its five `newBufferedWriter` writes,
    // and the overlay trace showed the fd being written once
    // (`set_field slot=0 value=Int(3)`) and every subsequent read of that slot
    // on the same object returning `Object(None)` — the fd destroyed before its
    // first use.
    //
    // So it is deleted rather than repaired or relocated to a side table: it is
    // the only writer of that overlay in a default build, and the default path
    // (real `BufferedWriter(OutputStreamWriter(Files.newOutputStream(p)))`) is
    // byte-identical to HotSpot across every line of that probe.

    // Minimal BufferedWriter natives backed by the fd stored at slot 0.
    // These are also registered in synthetic-jdk mode by phases_late, but
    // the registry dedups on (class, name, desc) so re-registering is
    // safe and keeps the contract explicit for Files.newBufferedWriter.
    //
    // These natives are registered on the REAL `java/io/BufferedWriter`
    // class, so in real-JDK mode they shadow EVERY BufferedWriter — not
    // just the synthetic fd-backed object that `Files.newBufferedWriter`
    // returns. A real `new BufferedWriter(new OutputStreamWriter(System.out))`
    // (the picocli / JUnit-console help-text writer) stores the wrapped
    // `Writer` in slot 0, not an `Int` fd, so the old `_ => Ok(None)` arms
    // silently DROPPED its output → empty `--help`. `bw_delegate_out`
    // distinguishes the two: when slot 0 is not an `Int`, the object is a
    // real BufferedWriter and the native forwards to its wrapped `out`
    // Writer so the genuine OutputStreamWriter/StreamEncoder bytecode runs.
    //
    // Two things every arm below now gets right, both of which used to answer a
    // plausible success:
    //
    //   * The delegating arms discarded the delegate's outcome
    //     (`let _ = ctx.invoke_virtual(out, "write", …)`). The wrapped
    //     `OutputStreamWriter` raising `IOException` — a full disk, a broken
    //     pipe, a closed underlying stream — was therefore invisible: the
    //     write returned normally with nothing written, and a caller inside
    //     `try (BufferedWriter w = …)` had no failure to catch. `Writer.write`
    //     is specified "@throws IOException If an I/O error occurs", and a
    //     BufferedWriter that does not report its delegate's error is a
    //     BufferedWriter that cannot report any error at all.
    //   * `bw_delegate_out` returning `None` in the default build means
    //     `out == null`, and `BufferedWriter.close()` is exactly what nulls
    //     `out` ("finally { out = null; cb = null; }"). Every write method in
    //     the real class opens with `ensureOpen()` —
    //     "if (out == null) throw new IOException("Stream closed")" — so the
    //     `None => return Ok(None)` arms were silently DISCARDING writes to a
    //     closed writer, the exact use-after-close the JDK raises on.
    // `java/io/BufferedWriter` — RETIRED FROM THE SHIPPING BUILDS 2026-08-28
    // (lane L4). Everything below is now behind `synthetic-jdk`, which is the
    // only build where the second arm of each body can fire.
    //
    // Each of the six has exactly two arms: a delegating arm that forwards to
    // the wrapped `out`, and an fd-backed arm reached through
    // `bw_synthetic_fd` — and that helper is itself
    // `#[cfg(feature = "synthetic-jdk")]` and answers `None` in every shipping
    // build, because `Files.newBufferedWriter`'s fd path was deleted on
    // 2026-08-05. So in `--real-jdk` and `--fast-boot` these six were pure
    // pass-throughs standing in front of the real class, and they cost two
    // things:
    //
    //   * the BUFFERING. MEASURED: `new BufferedWriter(sw, 4).write("ab")`
    //     reached the delegate immediately, where HotSpot holds it
    //     (`probes/L4StreamTailSweep.java`, row `writer buffered`).
    //   * the class's OWN argument contract, which is not the delegate's.
    //     MEASURED: `bw.write((String) null, 0, 1)` forwarded the null blind
    //     and one character of the word "null" was written to the file as
    //     data; the bounds failures came back as the delegate's exception type
    //     rather than `String.getChars`'s.
    //
    // Both were repaired in the delegating arm earlier in this lane, which is
    // what made the retirement provable rather than plausible: the arm is now
    // an exact re-implementation of what the real class already does, and the
    // probes that pin it (`L4StreamTailSweep`, `TailFamilySweep`) are 0-diff
    // against HotSpot in both modes with the real bytecode running instead.
    //
    // 2026-09-17: that "0-diff" measurement was against the DEFAULT build,
    // where this whole block does not exist. The cfg alone does not make that
    // true for a `--features synthetic-jdk` binary run against a real JDK: it
    // satisfies the cfg and registers these six OVER the real
    // `java.io.BufferedWriter`, and the buffering-loss row measured above
    // (`new BufferedWriter(sw, 4).write("ab")` reaching the delegate
    // immediately instead of being held) is exactly what a real
    // `BufferedWriter` wrapping any real `Writer` gets in that configuration —
    // the delegating arms below forward straight to `out` and never touch
    // `cb`/`nextChar`. Gate on the runtime predicate too, the way the
    // `native-io` twin of this block already does.
    #[cfg(feature = "synthetic-jdk")]
    if !r.drops_real_layout_synthetic() {
        let bw_class = "java/io/BufferedWriter";
        r.register(bw_class, "write", "(Ljava/lang/String;II)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Some(out) = bw_delegate_out(ctx, this) {
                // The DELEGATING arm has to enforce `BufferedWriter`'s OWN argument
                // contract before forwarding, because the writer underneath has a
                // different one. Forwarding blind, MEASURED against HotSpot:
                //
                //   write((String) null, 0, 1)  HotSpot NPE   this VM wrote "n"
                //   write("ab", -1, 1)          HotSpot SIOOBE   this VM IOOBE
                //   write("ab", 0, 9)           HotSpot SIOOBE   this VM IOOBE
                //
                // The first row is the one that matters: the null was stringified
                // somewhere down the delegate chain and ONE CHARACTER OF THE WORD
                // "null" was written to the file. A caller's null slipped into the
                // output as data.
                //
                // The rule is the fd-backed arm's below, and the two must agree:
                // `s.getChars(off, off + len, ...)` raises the String-specific
                // subclass, and a negative `len` writes nothing rather than
                // throwing (`BufferedWriter.write(String,int,int)`'s @implSpec).
                let s = args.get(1).cloned().unwrap_or(Value::Object(None));
                let text = match &s {
                    Value::Object(Some(o)) => ctx.read_string(*o).unwrap_or_default(),
                    _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
                };
                let off_i = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
                let len_i = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
                let total = text.encode_utf16().count() as i64;
                let end = i64::from(off_i) + i64::from(len_i);
                if len_i > 0 && (off_i < 0 || end > total) {
                    return Err(RuntimeError::StringIndexOutOfBoundsException {
                        index: off_i,
                        message: Some(format!("begin {off_i}, end {end}, length {total}")),
                    }
                    .into());
                }
                if len_i <= 0 {
                    return Ok(None);
                }
                let off = args.get(2).cloned().unwrap_or(Value::Int(0));
                let len = args.get(3).cloned().unwrap_or(Value::Int(0));
                ctx.invoke_virtual(out, "write", "(Ljava/lang/String;II)V", &[s, off, len])?;
                return Ok(None);
            }
            let text = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            // Keep these as the signed Java `int`s they are: casting to `usize`
            // first turns a negative offset into a colossal positive one, which the
            // clamp below then quietly turned into "write nothing".
            let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            let len = args.get(3).and_then(|v| v.as_int()).unwrap_or_default();
            let fd = match crate::phases_late::bw_synthetic_fd(ctx, this) {
                Some(fd) => fd,
                None => return Err(bw_stream_closed()),
            };
            // `BufferedWriter.write(String,int,int)` is the one overload in this
            // class whose bounds contract is asymmetric, and the old
            // `.min(text.chars().count())` collapsed both halves of it into a
            // silent truncation. Its @implSpec: "While the specification of this
            // method in the superclass recommends that an IndexOutOfBoundsException
            // be thrown if len is negative or off + len is negative, the
            // implementation in this class does not throw such an exception in
            // these cases but instead simply writes no characters." Its @throws:
            // "IndexOutOfBoundsException If off is negative, or off + len is
            // greater than the length of the given string."
            //
            // So a negative `len` is a no-op and a run past the end is an
            // exception — `w.write("ab", 0, 5)` MUST throw. The units are UTF-16
            // code units, because that is what `String.length()` and the
            // `s.getChars(b, b + d, …)` the real method performs both count; the
            // old code measured in code POINTS, so the check and the slice
            // disagreed for any supplementary character.
            let units: Vec<u16> = text.encode_utf16().collect();
            let total = units.len() as i64;
            let end = i64::from(off) + i64::from(len);
            // `StringIndexOutOfBoundsException`, not the bare supertype: the real
            // method reaches this through `String.getChars`, whose
            // `checkBoundsBeginEnd` raises the String-specific subclass, and a
            // `catch (StringIndexOutOfBoundsException)` does not match a supertype
            // instance while `catch (IndexOutOfBoundsException)` matches both.
            if len > 0 && (off < 0 || end > total) {
                return Err(RuntimeError::StringIndexOutOfBoundsException {
                    index: off,
                    message: Some(format!("begin {off}, end {end}, length {total}")),
                }
                .into());
            }
            if len <= 0 {
                return Ok(None);
            }
            let sub = String::from_utf16_lossy(&units[off as usize..end as usize]);
            ctx.fd_table()
                .write_string(fd, &sub)
                .map_err(|e| RuntimeError::IOException {
                    message: e.to_string(),
                })?;
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge);
        r.register(bw_class, "write", "(I)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Some(out) = bw_delegate_out(ctx, this) {
                let c = args.get(1).cloned().unwrap_or(Value::Int(0));
                ctx.invoke_virtual(out, "write", "(I)V", &[c])?;
                return Ok(None);
            }
            let c = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as u32;
            let fd = match crate::phases_late::bw_synthetic_fd(ctx, this) {
                Some(fd) => fd,
                None => return Err(bw_stream_closed()),
            };
            if let Some(ch) = char::from_u32(c) {
                let mut buf = [0u8; 4];
                let bytes = ch.encode_utf8(&mut buf).as_bytes();
                ctx.fd_table()
                    .write_bytes(fd, bytes)
                    .map_err(|e| RuntimeError::IOException {
                        message: e.to_string(),
                    })?;
            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge);
        r.register(bw_class, "write", "([CII)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Some(out) = bw_delegate_out(ctx, this) {
                let arr = args.get(1).cloned().unwrap_or(Value::Object(None));
                let off = args.get(2).cloned().unwrap_or(Value::Int(0));
                let len = args.get(3).cloned().unwrap_or(Value::Int(0));
                ctx.invoke_virtual(out, "write", "([CII)V", &[arr, off, len])?;
                return Ok(None);
            }
            let arr = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("BufferedWriter.write: null char[]".into()),
                    }
                    .into())
                }
            };
            let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
            let fd = match crate::phases_late::bw_synthetic_fd(ctx, this) {
                Some(fd) => fd,
                None => return Err(bw_stream_closed()),
            };
            let cap = ctx.array_length(arr) as i64;
            // Unlike the `String` overload above, the `char[]` one range-checks
            // BOTH ends: "@throws IndexOutOfBoundsException If off is negative, or
            // len is negative, or off + len is negative or greater than the length
            // of the given array" — the real body is a straight
            // `Objects.checkFromIndexSize(off, len, cbuf.length)`. The two
            // contracts differing by one clause is precisely why clamping cannot
            // stand in for either of them.
            let end = i64::from(off) + i64::from(len);
            if off < 0 || len < 0 || end > cap {
                return Err(RuntimeError::ioobe(
                    cratonvm_types::error::out_of_bounds_message::check_from_index_size(
                        i64::from(off),
                        i64::from(len),
                        cap,
                    ),
                )
                .into());
            }
            let mut chars: Vec<u16> = Vec::with_capacity(len as usize);
            for i in off as usize..end as usize {
                if let Value::Int(v) = ctx.get_array_element(arr, i) {
                    chars.push((v & 0xFFFF) as u16);
                }
            }
            let s = String::from_utf16_lossy(&chars);
            ctx.fd_table()
                .write_string(fd, &s)
                .map_err(|e| RuntimeError::IOException {
                    message: e.to_string(),
                })?;
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge);
        r.register(bw_class, "newLine", "()V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let sep = p57_line_separator(ctx);
            if let Some(out) = bw_delegate_out(ctx, this) {
                let s = ctx.create_string(&sep);
                ctx.invoke_virtual(
                    out,
                    "write",
                    "(Ljava/lang/String;)V",
                    &[Value::Object(Some(s))],
                )?;
                return Ok(None);
            }
            let fd = match crate::phases_late::bw_synthetic_fd(ctx, this) {
                Some(fd) => fd,
                None => return Err(bw_stream_closed()),
            };
            ctx.fd_table()
                .write_string(fd, &sep)
                .map_err(|e| RuntimeError::IOException {
                    message: e.to_string(),
                })?;
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge);
        r.register(bw_class, "flush", "()V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Some(out) = bw_delegate_out(ctx, this) {
                // A flush that reports success without the delegate having
                // succeeded is the worst answer this class can give: `flush()` is
                // exactly the call a caller makes to find out whether the bytes
                // landed before it acts on that belief.
                ctx.invoke_virtual(out, "flush", "()V", &[])?;
                return Ok(None);
            }
            // No delegate: either the synthetic-jdk fd-backed writer (nothing to do
            // — `BufWriter<File>` flushes on drop and every write above already
            // reached the buffered writer inside `fd_table`), or a null `out`,
            // which in the default build means the writer is CLOSED. Real
            // `BufferedWriter.flush()` opens with `ensureOpen()`, so the second
            // case is a refusal, not a no-op. Probe the fd first so the
            // synthetic-jdk arm keeps its no-op.
            match crate::phases_late::bw_synthetic_fd(ctx, this) {
                Some(_) => Ok(None),
                None => Err(bw_stream_closed()),
            }
        }, cratonvm_native_api::NativeKind::Bridge);
        r.register(bw_class, "close", "()V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Some(out) = bw_delegate_out(ctx, this) {
                // `close()` flushes first and the real class lets that flush throw
                // (`try (Writer w = out) { flushBuffer(); }`). Swallowing it is how
                // a full disk turns into a clean `try`-with-resources exit and a
                // truncated file nobody hears about.
                // `flush()` can move `out` before `close()` dereferences it, and
                // both calls can move `this` before the `out = null` write below --
                // so BOTH are pinned. `this_pin` is taken FIRST so its handle is the
                // lower one, which is what `unpin_native_roots` unwinds to.
                let this_pin = ctx.pin_native_root(this);
                let out_pin = ctx.pin_native_root(out);
                ctx.invoke_virtual(out, "flush", "()V", &[])?;
                let out = ctx.read_native_pin(out_pin, out);
                ctx.invoke_virtual(out, "close", "()V", &[])?;
                // NULL `out`, which is what makes the writer CLOSED. The real class
                // ends `close()` with `finally { out = null; cb = null; }`, and
                // every write method opens with `ensureOpen()` --
                // `if (out == null) throw new IOException("Stream closed")`. The
                // write arms above already raise `bw_stream_closed()` on that
                // branch; without this line they never reach it, because
                // `bw_delegate_out` kept answering the closed delegate.
                //
                // MEASURED: `bw.close(); bw.write("x")` returned normally against
                // HotSpot's IOException, in both modes
                // (`probes/TailFamilySweep.java`). It only shows with a sink whose
                // own `close` is a no-op -- a `StringWriter` -- which is why a
                // file-backed writer never surfaced it: there the delegate itself
                // throws, so the missing state change was invisible.
                let this = ctx.read_native_pin(this_pin, this);
                ctx.set_field_by_name(this, "out", Value::Object(None));
                ctx.unpin_native_roots(this_pin);
                return Ok(None);
            }
            let fd = match crate::phases_late::bw_synthetic_fd(ctx, this) {
                Some(fd) => fd,
                None => return Ok(None),
            };
            let _ = ctx.fd_table().close(fd);
            ctx.set_field(this, 0, Value::Int(-1));
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge);
    } // end java/io/BufferedWriter block (cfg + !drops_real_layout_synthetic)

    reg_files! { r.register(files,
        "readAllBytes",
        "(Ljava/nio/file/Path;)[B",
        |ctx, args| {
            let mut path_obj = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, &mut path_obj);
            let read = match vfs_read(&p) {
                Some(r) => r,
                None => std::fs::read(&p),
            };
            match read {
                Ok(data) => {
                    use cratonvm_types::ArrayElementType;
                    let arr = ctx.new_array(ArrayElementType::Byte, data.len());
                    ctx.write_byte_array_from(arr, 0, &data);
                    Ok(Some(Value::Object(Some(arr))))
                }
                // NIO contract: missing file → NoSuchFileException (see
                // newByteChannel above), not a bare IOException — callers like
                // FileSystemResource.getContentAsByteArray() catch
                // NoSuchFileException and translate it to FileNotFoundException
                // (ResourceTests#resourceCreateRelativeUnknown).
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Err(p57_no_such_file(ctx, &p)?)
                }
                // `Files.readAllBytes(<a directory>)` on Windows: opening the
                // directory handle succeeds but the subsequent read is refused
                // by the OS as access-denied (there is no EISDIR on Windows),
                // and HotSpot surfaces that as `AccessDeniedException`, not a
                // bare `IOException`. This arm is inert on Linux, where the
                // same call fails with `IsADirectory` (EISDIR) instead and
                // correctly falls through to the bare `IOException` below —
                // see the comment on `newFileChannel`'s open-failure mapping.
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    Err(p57_access_denied(ctx, &p)?)
                }
                Err(e) => Err(p57_io_error(&e)),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    // --- Path additional methods ---
    let path = "java/nio/file/Path";
    reg_path! { r.register(path,
        "toAbsolutePath",
        "()Ljava/nio/file/Path;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, &mut this);
            // JDK `Path.toAbsolutePath()` does NOT resolve symlinks or require the
            // file to exist (that is `toRealPath`) — it only makes a *relative*
            // path absolute. Must NOT `canonicalize()` here: this build runs UNIX-
            // mode `std::path` on Windows, so `canonicalize` on an existing path
            // returns a `\\?\C:\…` verbatim path that renders as `//?/C:/…` and
            // breaks downstream string-based Path ops — e.g. WildFly's
            // `Environment.validateWildFlyDir` then rejects a valid `jboss.dist`
            // ("could not find jboss-modules.jar" though it exists). A drive-letter
            // (`C:`), UNC (`\\`/`//`), or leading-separator path is already absolute
            // and returned unchanged; a relative path is anchored to the CWD.
            let abs = p57_absolute_path_string(&p);
            let result = p57_alloc_path(ctx, &abs)?;
            Ok(Some(Value::Object(Some(result))))
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    // `Path.toRealPath(LinkOption...)` — resolve to the real, canonical path of
    // an *existing* file. Per the JDK contract this method throws
    // `java.nio.file.NoSuchFileException` when the file does not exist; callers
    // such as Jetty's `start.jar` (`DirConfigSource.<init>`) rely on that
    // specific exception type — its bytecode has an exception-table entry that
    // catches `NoSuchFileException` to fall through to `start.d` scanning.
    // Previously `toRealPath` was unregistered and the real-JDK `WindowsPath`
    // bytecode (whose `WindowsNativeDispatcher` natives are not implemented)
    // returned `null`, so `FS.canReadFile(null)` -> `Files.exists(null)` raised
    // an uncatchable NPE. We must return a non-null Path or throw the *typed*
    // `NoSuchFileException` so the catch clause matches.
    reg_path! { r.register(path,
        "toRealPath",
        "([Ljava/nio/file/LinkOption;)Ljava/nio/file/Path;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, &mut this);
            // jar-filesystem entries: a real path inside a mounted jar. If the
            // entry is present, the path itself is already "real"; otherwise
            // it does not exist.
            if vfs_decode(&p).is_some() {
                if matches!(vfs_classify(&p), Some(JarFsKind::Absent) | None) {
                    return Err(p57_no_such_file(ctx, &p)?);
                }
                let result = p57_alloc_path(ctx, &p)?;
                return Ok(Some(Value::Object(Some(result))));
            }
            match std::fs::canonicalize(&p) {
                Ok(c) => {
                    let real = c.to_string_lossy().replace('\\', "/");
                    // Strip the Windows `\\?\` extended-length prefix that
                    // `canonicalize` adds, so the path stays usable by other
                    // string-based Path natives.
                    let real = real
                        .strip_prefix("//?/")
                        .map(|s| s.to_string())
                        .unwrap_or(real);
                    let result = p57_alloc_path(ctx, &real)?;
                    Ok(Some(Value::Object(Some(result))))
                }
                Err(_) => Err(p57_no_such_file(ctx, &p)?),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    // The two registrations just above are on the INTERFACE `java/nio/file/Path`
    // and are `Bridge` -- for a jar/jrt-mounted `Path`, stamped
    // `sun/nio/fs/WindowsPath` the same as every other Path this VM mints
    // (`path_layout::impl_class`, one carrier class for all of them), they are
    // shadowed: `sun/nio/fs/WindowsPath.toAbsolutePath`/`.toRealPath` are BOTH
    // `RETIRED_SHADOW_L4_WINFS_TRIPLES` entries (real bytecode wins, so a REAL
    // host path resolves through real Win32 calls end to end). Real
    // `WindowsPath.getAbsolutePath()`'s body opens `if (isAbsolute()) return
    // path;` (javap-verified) -- `isAbsolute()` is ALSO retired, and a jar/jrt
    // Path's `type` field is deliberately left `RELATIVE` by
    // `p57_write_path_fields`'s `is_vfs` branch (changing it to `ABSOLUTE`
    // would dodge THIS crash but reopen the `resolve`/`compareTo` NPE that
    // branch's own history names) -- so `isAbsolute()` answers `false`, and
    // real `getAbsolutePath()` falls into `getFileSystem().defaultDirectory()`,
    // a method only the concrete `sun.nio.fs.WindowsFileSystem` declares. Our
    // jrt/jar `FileSystem` singleton is deliberately minted against the
    // ABSTRACT `java/nio/file/FileSystem` (`filesystem_layout.rs`'s own doc:
    // jar/jrt FileSystems are their own, larger, undertaking, not covered
    // there) and has no such method: `NoSuchMethodError`. Found via real
    // in-process javac's `SystemModulesLocationHandler.initSystemModules` ->
    // `Locations.normalize` -> `WindowsPath.toRealPath` ->
    // `WindowsLinkSupport.getRealPath` -> `WindowsPath.toAbsolutePath` ->
    // `getAbsolutePath` (H2's `CREATE ALIAS ... AS $$ ... $$`, the jrt-mounted
    // platform-classpath listing HIB-CV-27 added).
    //
    // `WindowsLinkSupport.getRealPath` calls the COVARIANT
    // `toAbsolutePath()Lsun/nio/fs/WindowsPath;` directly (javap-verified,
    // `invokevirtual`, NOT the bridge) -- and that exact triple is NOT itself
    // in the retired table (nothing ever competed for it before), so a plain
    // `register` here is not auto-downgraded and simply wins, the same as any
    // other untouched triple. Receiver-aware: a jar/jrt Path is ALREADY
    // absolute within its own namespace (every logical path this VM's jrtfs/
    // jarfs encoders produce is rooted at that filesystem's own root), so
    // answer with `this` unchanged -- exactly real `toAbsolutePath()`'s own
    // `if (isAbsolute()) return this;` fast path, had `isAbsolute()` been
    // able to say so. A REAL host receiver is handed straight to its own real
    // bytecode via `invoke_virtual_bytecode_only`
    // (`native-collections`'s `asl_delegate_foreign` is the precedent for this
    // shape), so this registration changes NOTHING for it. The bridge
    // `toAbsolutePath()Ljava/nio/file/Path;` (which IS retired) just forwards
    // to this covariant method (javap-verified), so it is fixed too, for
    // free, without needing its own registration.
    r.register(
        "sun/nio/fs/WindowsPath",
        "toAbsolutePath",
        "()Lsun/nio/fs/WindowsPath;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            let p = p57_read_path(ctx, &mut this);
            if jarfs_decode(&p).is_some() || jrtfs_decode(&p).is_some() {
                return Ok(Some(Value::Object(Some(this))));
            }
            ctx.invoke_virtual_bytecode_only(
                this,
                "toAbsolutePath",
                "()Lsun/nio/fs/WindowsPath;",
                &[],
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // `toRealPath`, unlike `toAbsolutePath` above, has NO untouched triple to
    // register on instead: BOTH `sun/nio/fs/WindowsPath.toRealPath`
    // descriptors are `RETIRED_SHADOW_L4_WINFS_TRIPLES` entries, and the
    // covariant one is what real bytecode actually calls internally
    // (javap-verified: the bridge `toRealPath([Ljava/nio/file/LinkOption;)
    // Ljava/nio/file/Path;` just forwards to it, same shape as
    // `toAbsolutePath` above). Fixing `toAbsolutePath` alone is not enough
    // here: real `toRealPath()`'s body ALWAYS calls
    // `WindowsLinkSupport.getRealPath(this, followLinks)` first, a genuine
    // Win32 symlink-resolution routine with real `CreateFile`/
    // `GetFinalPathNameByHandle` calls that has no counterpart for a jar/jrt
    // mount (no real handle exists), so that call has to be avoided
    // altogether for a jar/jrt receiver, not patched after the fact.
    //
    // A plain `register` on a retired triple is auto-downgraded to
    // `cratonvm_native_api::NativeKind::SyntheticStub` by `NativeMethodRegistry::register`'s own
    // `triple_is_retired_shadow` check -- REJECTED outright under
    // `--real-jdk` (`cratonvm_native_api::NativeKind::allowed_in`), not merely shadowed -- so
    // there is no active registration for `force_native_over_real_jdk_bytecode`
    // to even prefer (the DirectoryStream collision this same investigation
    // hit earlier turned out to need a different fix for exactly this
    // reason). `register_with_kind(..., Intrinsic)` bypasses that downgrade —
    // it is a direct call, not routed through `register`'s wrapper — and
    // `Intrinsic` is by design what "forces every dispatch path through the
    // native even when real bytecode exists" (`NativeKind`'s own doc). Safe
    // here specifically because the body below is receiver-aware and
    // byte-for-byte delegates to real bytecode for any receiver that is not
    // ours, via `invoke_virtual_bytecode_only` — a REAL host `WindowsPath`
    // sees no behavior change at all, only a jar/jrt one does.
    //
    // The jar/jrt body mirrors the existing, already-correct interface-level
    // `Path.toRealPath` registration a few dozen lines up: a present virtual
    // entry is already "real" (no symlinks inside a mounted jar/jrt image),
    // an absent one is `NoSuchFileException`.
    register_vfs_windows_path_intrinsic(
        r,
        "toRealPath",
        "([Ljava/nio/file/LinkOption;)Lsun/nio/fs/WindowsPath;",
        |ctx, args| {
            // The receiver and the LinkOption array are forwarded after the
            // Path read: current args (gc-common w21-b).
            let (p, args) = p57_read_path_arg(ctx, args, 0)?;
            let args = &*args;
            let this = obj_arg(args, 0)?;
            if vfs_decode(&p).is_some() {
                if matches!(vfs_classify(&p), Some(JarFsKind::Absent) | None) {
                    return Err(p57_no_such_file(ctx, &p)?);
                }
                let result = p57_alloc_path(ctx, &p)?;
                return Ok(Some(Value::Object(Some(result))));
            }
            let link_opts = args.get(1..).unwrap_or(&[]);
            ctx.invoke_virtual_bytecode_only(
                this,
                "toRealPath",
                "([Ljava/nio/file/LinkOption;)Lsun/nio/fs/WindowsPath;",
                link_opts,
            )
        },
    );

    // Route through the shared root-aware `p57_normalize_path` (keeps the root
    // and leading `..` on relative paths) instead of an inline `..`-pop that
    // dropped the drive root. This is the last-registered (winning) `normalize`.
    reg_path! { r.register(path, "normalize", "()Ljava/nio/file/Path;", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, &mut this);
        let normalized = p57_normalize_path(&p);
        let result = p57_alloc_path(ctx, &normalized)?;
        Ok(Some(Value::Object(Some(result))))
    }, cratonvm_native_api::NativeKind::Bridge) }

    reg_path! { r.register(path, "toFile", "()Ljava/io/File;", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, &mut this);
        p57_path_to_file(ctx, &p)
    }, cratonvm_native_api::NativeKind::Bridge) }

    reg_path! { r.register(path, "toUri", "()Ljava/net/URI;", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, &mut this);
        // jar-FS Path → `jar:file:/<jar>!/<entry>` URI (matches JDK zipfs), so
        // callers that round-trip back through URL/openStream can re-open it.
        if let Some((jar, entry)) = jarfs_decode(&p) {
            let jar_slash = jar.replace('\\', "/");
            let jar_abs = if jar_slash.starts_with('/') {
                jar_slash
            } else {
                format!("/{jar_slash}")
            };
            let entry = entry.trim_start_matches('/');
            let uri_str = format!("jar:file:{jar_abs}!/{entry}");
            let uri = try_alloc_concurrent_synthetic(ctx, "java/net/URI", 5)?;
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh URI (native stale-local family).
            let uri_pin = ctx.pin_native_root(uri);
            let uri = ctx.read_native_pin(uri_pin, uri);
            // JDK-ONLY-LAYOUT: raw slots only on OUR layout — slot 0 is
            // `scheme` and slot 4 is `host` on a real `java.net.URI`.
            if crate::net_phase_e::uri_has_synthetic_layout(ctx, uri) {
                let s = ctx.create_string(&uri_str);
                let uri = ctx.read_native_pin(uri_pin, uri);
                ctx.set_field(uri, 0, Value::Object(Some(s)));
                ctx.set_field(uri, 4, Value::Object(Some(s)));
            }
            let uri = ctx.read_native_pin(uri_pin, uri);
            crate::net_phase_e::uri_publish_named(ctx, uri, &uri_str, None);
            let uri = ctx.read_native_pin(uri_pin, uri);
            ctx.unpin_native_roots(uri_pin);
            return Ok(Some(Value::Object(Some(uri))));
        }
        // jrt-FS Path → `jrt:/modules/<module>/<entry>` URI (matches the JDK
        // runtime-image scheme).
        if let Some((_jh, entry)) = jrtfs_decode(&p) {
            let entry = entry.trim_start_matches('/');
            let uri_str = format!("jrt:/{entry}");
            let uri = try_alloc_concurrent_synthetic(ctx, "java/net/URI", 5)?;
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh URI (native stale-local family).
            let uri_pin = ctx.pin_native_root(uri);
            let uri = ctx.read_native_pin(uri_pin, uri);
            // JDK-ONLY-LAYOUT: raw slots only on OUR layout — slot 0 is
            // `scheme` and slot 4 is `host` on a real `java.net.URI`.
            if crate::net_phase_e::uri_has_synthetic_layout(ctx, uri) {
                let s = ctx.create_string(&uri_str);
                let uri = ctx.read_native_pin(uri_pin, uri);
                ctx.set_field(uri, 0, Value::Object(Some(s)));
                ctx.set_field(uri, 4, Value::Object(Some(s)));
            }
            let uri = ctx.read_native_pin(uri_pin, uri);
            crate::net_phase_e::uri_publish_named(ctx, uri, &uri_str, None);
            let uri = ctx.read_native_pin(uri_pin, uri);
            ctx.unpin_native_roots(uri_pin);
            return Ok(Some(Value::Object(Some(uri))));
        }
        let prefixed;
        let slash_p: &str = if p.starts_with('/') {
            &p
        } else {
            prefixed = format!("/{}", p);
            &prefixed
        };
        // Percent-encode the path component, matching `java.nio.file.Path.toUri()`
        // (and `File.toURI()`): a path char like `#`, ` `, `?` must be `%`-escaped
        // so it stays part of the path rather than being parsed as a URI fragment
        // or query. HotSpot renders `…/resource#test1.txt` as
        // `file:///…/resource%23test1.txt`; leaving the `#` literal made
        // `toUri().toURL()` drop everything after it (Spring's
        // PathMatchingResourcePatternResolver URL/URI-syntax assertions).
        // Per `UnixUriUtils.toUri`/`WindowsUriSupport.toUri`, a Path that
        // names an existing DIRECTORY renders with a trailing `/`; a file (or a
        // path that does not exist) does not. `java.io.File.toURI()` below
        // already applies the same rule. Until `Path` construction normalized
        // its stored string this was masked for paths the caller happened to
        // write with a trailing separator, and wrong for every other directory.
        let dir_slash;
        let slash_p: &str = if !slash_p.ends_with('/') && std::path::Path::new(&p).is_dir() {
            dir_slash = format!("{slash_p}/");
            &dir_slash
        } else {
            slash_p
        };
        let encoded = encode_file_uri_path(slash_p);
        let uri_str = format!("file://{}", encoded);
        let uri = try_alloc_concurrent_synthetic(ctx, "java/net/URI", 5)?;
        // Pin across the create_strings below — a moving young GC there would
        // relocate the fresh URI (native stale-local family).
        let uri_pin = ctx.pin_native_root(uri);
        let uri = ctx.read_native_pin(uri_pin, uri);
        // JDK-ONLY-LAYOUT: raw slots only on OUR layout. Slot 0 is `scheme` and
        // slot 4 is `host` on a real `java.net.URI`; the leading-slash path the
        // JDK exposes via `getPath()` is carried by the named publish below.
        if crate::net_phase_e::uri_has_synthetic_layout(ctx, uri) {
            let s = ctx.create_string(&uri_str);
            let uri = ctx.read_native_pin(uri_pin, uri);
            ctx.set_field(uri, 0, Value::Object(Some(s)));
            let path_s = ctx.create_string(&encoded);
            let uri = ctx.read_native_pin(uri_pin, uri);
            ctx.set_field(uri, 4, Value::Object(Some(path_s)));
        }
        let uri = ctx.read_native_pin(uri_pin, uri);
        // `encoded`, not `slash_p`: the override is the RAW path component and
        // `uri_publish_named` derives `decodedPath` from it, so handing it the
        // decoded spelling made `getRawPath()` answer an un-escaped path while
        // `toString()` stayed correct. This is the registration that actually
        // runs -- `Path.toUri()` is registered in this file AND in the phase-57
        // registrar above, and the later one wins -- which is why fixing only
        // the other copy moved nothing. See `net_phase_e::uri_publish_named`.
        crate::net_phase_e::uri_publish_named(ctx, uri, &uri_str, Some(&encoded));
        let uri = ctx.read_native_pin(uri_pin, uri);
        ctx.unpin_native_roots(uri_pin);
        Ok(Some(Value::Object(Some(uri))))
    }, cratonvm_native_api::NativeKind::Bridge) }

    reg_path! { r.register(path, "getNameCount", "()I", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, &mut this);
        let count = p57_name_elements(&p).len();
        Ok(Some(Value::Int(count as i32)))
    }, cratonvm_native_api::NativeKind::Bridge) }

    reg_path! { r.register(path, "getName", "(I)Ljava/nio/file/Path;", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let idx = match args[1] {
            Value::Int(i) => i,
            _ => 0,
        };
        let p = p57_read_path(ctx, &mut this);
        let parts: Vec<String> = p57_name_elements(&p);
        // FIX (finding 4): JDK throws IllegalArgumentException for out-of-range index.
        if idx < 0 || idx as usize >= parts.len() {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("Invalid index: {idx}"),
            }
            .into());
        }
        let name = parts[idx as usize].as_str();
        let result = p57_alloc_path(ctx, name)?;
        Ok(Some(Value::Object(Some(result))))
    }, cratonvm_native_api::NativeKind::Bridge) }

    reg_path! { r.register(path,
        "startsWith",
        "(Ljava/nio/file/Path;)Z",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            let mut other = obj_arg(args, 1)?;
            let (p, o) = p57_read_path_pair(ctx, &mut this, &mut other);
            Ok(Some(Value::Int(i32::from(p57_starts_with(&p, &o)))))
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    reg_path! { r.register(path, "startsWith", "(Ljava/lang/String;)Z", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let other = obj_arg(args, 1)?;
        // The String argument is read BEFORE the Path, whose read can run Java
        // (gc-common w21-b).
        let o = ctx.read_string(other).unwrap_or_default();
        let p = p57_read_path(ctx, &mut this);
        Ok(Some(Value::Int(i32::from(p57_starts_with(&p, &o)))))
    }, cratonvm_native_api::NativeKind::Bridge) }

    reg_path! { r.register(path, "endsWith", "(Ljava/nio/file/Path;)Z", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let mut other = obj_arg(args, 1)?;
        let (p, o) = p57_read_path_pair(ctx, &mut this, &mut other);
        Ok(Some(Value::Int(i32::from(p57_ends_with(&p, &o)))))
    }, cratonvm_native_api::NativeKind::Bridge) }

    reg_path! { r.register(path, "endsWith", "(Ljava/lang/String;)Z", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let other = obj_arg(args, 1)?;
        // The String argument is read BEFORE the Path, whose read can run Java
        // (gc-common w21-b).
        let o = ctx.read_string(other).unwrap_or_default();
        let p = p57_read_path(ctx, &mut this);
        Ok(Some(Value::Int(i32::from(p57_ends_with(&p, &o)))))
    }, cratonvm_native_api::NativeKind::Bridge) }

    reg_path! { r.register(path, "isAbsolute", "()Z", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let p = p57_read_path(ctx, &mut this);
        // keycloak-15: drive-relative / driveless-rooted paths are not absolute on
        // Windows (see `p57_win_is_absolute`); POSIX leading-`/` rule on Unix.
        let abs = if cfg!(windows) {
            p57_win_is_absolute(&p)
        } else {
            p.starts_with('/')
        };
        Ok(Some(Value::Int(if abs { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge) }

    // JVMS 6.5 (nio21 close): the JDK's own default body -- an anonymous
    // `Path$1` over `getNameCount` / `getName` -- answers, where this row used
    // to mint a one-field object stamped with the INTERFACE `java/util/Iterator`
    // (the census named it). Both `getNameCount` and `getName` are natives of
    // this registrar, so the body reads the carrier exactly as the carrier's
    // own iterator did. (The earlier `iterator` registration above is
    // overwritten by this one.)
    reg_path! { r.register(path, "iterator", "()Ljava/util/Iterator;", |ctx, args| {
        ctx.invoke_special_bytecode_only(
            "java/nio/file/Path",
            "iterator",
            "()Ljava/util/Iterator;",
            args,
        )
    }, cratonvm_native_api::NativeKind::Bridge) }

    reg_path! { r.register(path, "subpath", "(II)Ljava/nio/file/Path;", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let begin = match args[1] {
            Value::Int(i) => i,
            _ => 0,
        };
        let end = match args[2] {
            Value::Int(i) => i,
            _ => 0,
        };
        let p = p57_read_path(ctx, &mut this);
        let parts: Vec<String> = p57_name_elements(&p);
        let count = parts.len() as i32;
        // FIX (finding 4): JDK throws IllegalArgumentException for an out-of-range range.
        if begin < 0 || begin >= count || end <= begin || end > count {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("Invalid subpath range: begin={begin}, end={end}, count={count}"),
            }
            .into());
        }
        let sub: Vec<String> = parts[begin as usize..end as usize].to_vec();
        let result = p57_alloc_path(ctx, &sub.join("/"))?;
        Ok(Some(Value::Object(Some(result))))
    }, cratonvm_native_api::NativeKind::Bridge) }

    reg_path! { r.register(path, "compareTo", "(Ljava/nio/file/Path;)I", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let mut other = obj_arg(args, 1)?;
        let (p, o) = p57_read_path_pair(ctx, &mut this, &mut other);
        Ok(Some(Value::Int(p.cmp(&o) as i32)))
    }, cratonvm_native_api::NativeKind::Bridge) }

    // `p57_alloc_path`/`p57_alloc_path_raw` now mint a Path stamped as the
    // concrete platform class (see their own docs), so dispatch on a REAL
    // `UnixPath`/`WindowsPath` receiver keys on ITS name first (`H11-1`).
    // Mirror every row this block just registered under the interface name
    // onto the concrete one, or a receiver this crate minted would find
    // NEITHER its own registration NOR real bytecode not yet safe to run --
    // exactly the two-halves rule `concrete_receiver`'s module doc states for
    // every `alloc_concrete` caller. Same class both allocators mint, so the
    // two cannot drift.
    cratonvm_native_io::concrete_receiver::mirror_class_registrations(
        r,
        __path_rows_before,
        path,
        cratonvm_native_api::path_layout::impl_class(),
    );

    // --- Paths utility class ---
    let paths = "java/nio/file/Paths";
    r.register(
        paths,
        "get",
        "(Ljava/lang/String;[Ljava/lang/String;)Ljava/nio/file/Path;",
        |ctx, args| {
            let first = obj_arg(args, 0)?;
            let mut p = ctx.read_string(first).unwrap_or_default();
            // Varargs more[] - field 0 of the array
            if let Value::Object(Some(arr)) = &args[1] {
                let len = ctx.array_length(*arr);
                // K5 (round 15 wave 6, lane jni6): `Paths.get` is `Path.of`;
                // this put a `/` in front of the first part even when `first`
                // was empty (`Paths.get("", "b")` answered the ABSOLUTE `/b`)
                // and kept empty parts.
                let join = len > 0;
                let mut literal = (cfg!(windows) && join).then(|| p.clone());
                for i in 0..len {
                    if let Value::Object(Some(s)) = ctx.get_array_element(*arr, i) {
                        let part = ctx.read_string(s).unwrap_or_default();
                        if let Some(literal) = literal.as_mut() {
                            p57_windows_literal_join(literal, &part);
                        }
                        if join {
                            p57_get_path_join(&mut p, &part);
                            continue;
                        }
                        if !p.ends_with('/') && !part.starts_with('/') {
                            p.push('/');
                        }
                        p.push_str(&part);
                    }
                }
                if let Some((input, reason, index)) =
                    literal.as_deref().and_then(p57_windows_join_refusal)
                {
                    return p57_throw_invalid_path(ctx, &input, &reason, index);
                }
            }
            p57_alloc_path_checked(ctx, &p)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- FileSystem additional methods ---
    let fsys = "java/nio/file/FileSystem";
    // `newWatchService` is NOT registered here either — see the matching note
    // on the `fs_class` block above. This copy allocated a ZERO-field
    // WatchService, which is the object the heap guard reported as
    // "out-of-bounds field write dropped ... class_name=java/nio/file/
    // WatchService real_field_count=Some(0)".

    // `FileSystem.getPathMatcher(String)` and the ONE method the returned
    // interface declares. Until 2026-08-17 the body below was
    //
    //     |ctx, _args| { alloc(java/nio/file/PathMatcher, 0 fields) }
    //
    // — it ignored the pattern entirely and handed back an object stamped with
    // the INTERFACE, carrying no state, and `PathMatcher.matches` had no
    // registration anywhere in the VM. That is an object with zero usable
    // methods: `0 of 1`. MEASURED consequence (`scratchpad/g22/GlobProbe`-class
    // census, 292 rows on both VMs): every `matches()` answered
    // `AbstractMethodError: java/nio/file/PathMatcher.matches(Ljava/nio/file/
    // Path;)Z has no Code attribute`, and every one of the ~50 rows where
    // HotSpot REFUSES the pattern (`getPathMatcher(null)`, `"*.txt"` with no
    // syntax prefix, `"foo:…"`, `glob:[abc`, `glob:{a,b`, `glob:abc\`,
    // `regex:[a-`) built a matcher happily instead, because nothing parsed.
    // `RCrypto` reaches it from inside java.base — `JceSecurity.
    // setupJurisdictionPolicies` -> `Files.newDirectoryStream(dir, glob)`,
    // whose real JDK bytecode is `matcher.matches(entry.getFileName())`.
    //
    // Deleting the registration is NOT the fix, even though the real
    // `sun.nio.fs.WindowsFileSystem.getPathMatcher` is a complete
    // implementation: `FileSystems.getDefault()` returns CratonVM's own
    // synthetic `java/nio/file/FileSystem` (`p57_default_filesystem_singleton`;
    // its `sun.nio.fs.WindowsFileSystem` getClass() answer is the
    // `jdk_concrete_getclass_alias` mapping, not its real class), and
    // `java.nio.file.FileSystem.getPathMatcher` is ABSTRACT — so unregistering
    // moves the `AbstractMethodError` one call earlier rather than removing it.
    //
    // Nor may the fix be to make `newDirectoryStream`'s glob overload skip the
    // matcher: that is exactly the fabricated success `d378eee51` removed, and
    // it made `RCrypto` green over a filter that was never invoked.
    r.register(
        fsys,
        "getPathMatcher",
        "(Ljava/lang/String;)Ljava/nio/file/PathMatcher;",
        |ctx, args| {
            // JVMS 6.5 (nio21 close): a concrete platform filesystem (the JDK's
            // `LinuxFileSystem` / `WindowsFileSystem`, which dispatch reaches
            // through the mirror of this row onto its own class) answers with
            // its own body: a lambda over `Pattern`, which `new` could
            // produce, where this row mints the INTERFACE below. Its body
            // reads no filesystem state. Only the abstract stamp (the
            // synthetic jar / jrt mounts) keeps the carrier.
            if let Some(Value::Object(Some(this))) = args.first().copied() {
                let class = ctx.class_name_arc_of_id(ctx.class_id_of_object(this));
                if class.is_some_and(|c| &*c != "java/nio/file/FileSystem") {
                    return ctx.invoke_virtual_bytecode_only(
                        this,
                        "getPathMatcher",
                        "(Ljava/lang/String;)Ljava/nio/file/PathMatcher;",
                        &args[1..],
                    );
                }
            }
            let sai = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                // HotSpot's is a helpful-NPE synthesized by the JVM from
                // `syntaxAndInput.indexOf(':')`, transcribed verbatim
                // (MEASURED — it cannot be derived).
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some(
                            "Cannot invoke \"String.indexOf(int)\" because \
                             \"syntaxAndInput\" is null"
                                .into(),
                        ),
                    }
                    .into())
                }
            };
            let sai = ctx.read_string(sai).unwrap_or_default();
            // `int pos = syntaxAndInput.indexOf(':'); if (pos <= 0) throw` —
            // note `<= 0`, so a LEADING colon is refused too, and only the
            // FIRST colon splits (`glob:a:b` is the glob `a:b`).
            let split = match sai.find(':') {
                None | Some(0) => None,
                Some(b) => Some((sai[..b].to_string(), sai[b + 1..].to_string())),
            };
            let (syntax, input) = match split {
                Some(pair) => pair,
                None => return Err(p57_bare_illegal_argument(ctx)),
            };
            // `syntax.equalsIgnoreCase(GLOB_SYNTAX)` — `GLOB:` and `Glob:` are
            // accepted, ` glob:` and `glob :` are not (no trimming). MEASURED.
            let expr = if syntax.eq_ignore_ascii_case("glob") {
                let units: Vec<u16> = input.encode_utf16().collect();
                match p57_globs_to_regex(&units, cfg!(windows)) {
                    Ok(u) => String::from_utf16_lossy(&u),
                    Err((desc, idx)) => {
                        return Err(p57_throw_pattern_syntax(ctx, desc, &input, idx))
                    }
                }
            } else if syntax.eq_ignore_ascii_case("regex") {
                // No translation at all: the string goes to `Pattern.compile`
                // untouched, so the caller owns its own separator escaping.
                input
            } else {
                return Err(RuntimeError::UnsupportedOperationException {
                    message: format!("Syntax '{syntax}' not recognized"),
                }
                .into());
            };
            let expr_obj = ctx.create_string(&expr);
            // CASE_INSENSITIVE | UNICODE_CASE, applied on Windows to `regex:`
            // patterns as well as `glob:` ones — MEASURED: `regex:A.TXT`
            // matches `a.txt`, `regex:(?-i)A\.TXT` does not, and `regex:Ä`
            // matches `ä`.
            let pattern = ctx.invoke(
                "java/util/regex/Pattern",
                "compile",
                "(Ljava/lang/String;I)Ljava/util/regex/Pattern;",
                &[Value::Object(Some(expr_obj)), Value::Int(P57_MATCHER_FLAGS)],
            )?;
            let pattern = match pattern {
                Some(Value::Object(Some(p))) => p,
                _ => {
                    return Err(RuntimeError::IllegalStateException {
                        message: format!("Pattern.compile returned no Pattern for {expr}"),
                    }
                    .into())
                }
            };
            // The carrier allocation can move the just-compiled Pattern.
            let pat_pin = ctx.pin_native_root(pattern);
            // gc-common w15-f: typed so the stale-handle audit sees the binding;
            // the allocation is the last GC point before the store.
            let pm: ObjectRef =
                try_alloc_concurrent_synthetic(ctx, "java/nio/file/PathMatcher", 1)?;
            let pattern = ctx.read_native_pin(pat_pin, pattern);
            ctx.set_field(pm, P57_MATCHER_PATTERN_FIELD, Value::Object(Some(pattern)));
            ctx.unpin_native_roots(pat_pin);
            Ok(Some(Value::Object(Some(pm))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // The receiver's runtime class IS `java/nio/file/PathMatcher` (see above),
    // so this registration is what the receiver-own-class lookup finds. A
    // one-field object stamped with an interface is the same shape the
    // `Path.iterator` body a few hundred lines up already uses for
    // `java/util/Iterator`.
    r.register(
        "java/nio/file/PathMatcher",
        "matches",
        "(Ljava/nio/file/Path;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let path = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some(
                            "Cannot invoke \"java.nio.file.Path.toString()\" \
                             because \"path\" is null"
                                .into(),
                        ),
                    }
                    .into())
                }
            };
            let pattern = match ctx.get_field(this, P57_MATCHER_PATTERN_FIELD) {
                Value::Object(Some(p)) => p,
                _ => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "PathMatcher carries no compiled Pattern".into(),
                    }
                    .into())
                }
            };
            // `pattern.matcher(path.toString()).matches()` — the WHOLE
            // normalized path string, fully anchored (`Matcher.matches`, plus
            // the `^`/`$` the glob translation adds). There is no basename-only
            // mode; `Files.newDirectoryStream(dir, glob)` gets basename
            // semantics by passing `entry.getFileName()`, not by anything here.
            let pat_pin = ctx.pin_native_root(pattern);
            let s = ctx.invoke_virtual(path, "toString", "()Ljava/lang/String;", &[])?;
            let pattern = ctx.read_native_pin(pat_pin, pattern);
            let s = match s {
                Some(v @ Value::Object(Some(_))) => v,
                _ => Value::Object(None),
            };
            let matcher = ctx.invoke(
                "java/util/regex/Pattern",
                "matcher",
                "(Ljava/lang/CharSequence;)Ljava/util/regex/Matcher;",
                &[Value::Object(Some(pattern)), s],
            );
            ctx.unpin_native_roots(pat_pin);
            let matcher = match matcher? {
                Some(Value::Object(Some(m))) => m,
                _ => return Ok(Some(Value::Int(0))),
            };
            let verdict = ctx.invoke(
                "java/util/regex/Matcher",
                "matches",
                "()Z",
                &[Value::Object(Some(matcher))],
            )?;
            Ok(Some(Value::Int(match verdict {
                Some(Value::Int(v)) => i32::from(v != 0),
                _ => 0,
            })))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // The (class, method, descriptor) triples a JDK 25 image declares
    // ACC_NATIVE, measured against linux-x64 AND windows-x64 25.0.4+7 on
    // 2026-08-05. `java.io.UnixFileSystem` exists only on the Linux image and
    // `java.io.WinNTFileSystem` only on the Windows one, so each is judged
    // against its own platform and the union is what this table holds.
    //
    // Everything registered below that is NOT in here is one of two things:
    // the un-suffixed spelling, which is the Java wrapper that calls the JNI
    // entry point (a contract §1.4 shadow, not a §1.5 bridge), or a
    // descriptor/name the other platform uses. Both keep the ambient category
    // so the census goes on reporting them as unadjudicated.
    const FS_IMAGE_NATIVE: &[(&str, &str, &str)] = &[
        (
            "java/io/UnixFileSystem",
            "canonicalize0",
            "(Ljava/lang/String;)Ljava/lang/String;",
        ),
        (
            "java/io/UnixFileSystem",
            "checkAccess0",
            "(Ljava/io/File;I)Z",
        ),
        (
            "java/io/UnixFileSystem",
            "createDirectory0",
            "(Ljava/io/File;)Z",
        ),
        (
            "java/io/UnixFileSystem",
            "createFileExclusively0",
            "(Ljava/lang/String;)Z",
        ),
        ("java/io/UnixFileSystem", "delete0", "(Ljava/io/File;)Z"),
        (
            "java/io/UnixFileSystem",
            "getBooleanAttributes0",
            "(Ljava/io/File;)I",
        ),
        (
            "java/io/UnixFileSystem",
            "getLastModifiedTime0",
            "(Ljava/io/File;)J",
        ),
        ("java/io/UnixFileSystem", "getLength0", "(Ljava/io/File;)J"),
        (
            "java/io/UnixFileSystem",
            "getNameMax0",
            "(Ljava/lang/String;)J",
        ),
        ("java/io/UnixFileSystem", "getSpace0", "(Ljava/io/File;I)J"),
        ("java/io/UnixFileSystem", "initIDs", "()V"),
        (
            "java/io/UnixFileSystem",
            "list0",
            "(Ljava/io/File;)[Ljava/lang/String;",
        ),
        (
            "java/io/UnixFileSystem",
            "rename0",
            "(Ljava/io/File;Ljava/io/File;)Z",
        ),
        (
            "java/io/UnixFileSystem",
            "setLastModifiedTime0",
            "(Ljava/io/File;J)Z",
        ),
        (
            "java/io/UnixFileSystem",
            "setPermission0",
            "(Ljava/io/File;IZZ)Z",
        ),
        (
            "java/io/UnixFileSystem",
            "setReadOnly0",
            "(Ljava/io/File;)Z",
        ),
        (
            "java/io/WinNTFileSystem",
            "canonicalize0",
            "(Ljava/lang/String;)Ljava/lang/String;",
        ),
        (
            "java/io/WinNTFileSystem",
            "checkAccess0",
            "(Ljava/io/File;I)Z",
        ),
        (
            "java/io/WinNTFileSystem",
            "createDirectory0",
            "(Ljava/io/File;)Z",
        ),
        (
            "java/io/WinNTFileSystem",
            "createFileExclusively0",
            "(Ljava/lang/String;)Z",
        ),
        ("java/io/WinNTFileSystem", "delete0", "(Ljava/io/File;Z)Z"),
        (
            "java/io/WinNTFileSystem",
            "getBooleanAttributes0",
            "(Ljava/io/File;)I",
        ),
        (
            "java/io/WinNTFileSystem",
            "getDriveDirectory",
            "(I)Ljava/lang/String;",
        ),
        (
            "java/io/WinNTFileSystem",
            "getFinalPath0",
            "(Ljava/lang/String;)Ljava/lang/String;",
        ),
        (
            "java/io/WinNTFileSystem",
            "getLastModifiedTime0",
            "(Ljava/io/File;)J",
        ),
        ("java/io/WinNTFileSystem", "getLength0", "(Ljava/io/File;)J"),
        (
            "java/io/WinNTFileSystem",
            "getNameMax0",
            "(Ljava/lang/String;)I",
        ),
        ("java/io/WinNTFileSystem", "getSpace0", "(Ljava/io/File;I)J"),
        ("java/io/WinNTFileSystem", "initIDs", "()V"),
        (
            "java/io/WinNTFileSystem",
            "list0",
            "(Ljava/io/File;)[Ljava/lang/String;",
        ),
        ("java/io/WinNTFileSystem", "listRoots0", "()I"),
        (
            "java/io/WinNTFileSystem",
            "rename0",
            "(Ljava/io/File;Ljava/io/File;)Z",
        ),
        (
            "java/io/WinNTFileSystem",
            "setLastModifiedTime0",
            "(Ljava/io/File;J)Z",
        ),
        (
            "java/io/WinNTFileSystem",
            "setPermission0",
            "(Ljava/io/File;IZZ)Z",
        ),
        (
            "java/io/WinNTFileSystem",
            "setReadOnly0",
            "(Ljava/io/File;)Z",
        ),
    ];
    /// Register a `FileSystem` native, stating `Bridge` only when the image
    /// backs this exact triple. See `FS_IMAGE_NATIVE`.
    fn fs_reg(
        r: &mut NativeMethodRegistry,
        cls: &str,
        name: &str,
        desc: &str,
        cb: cratonvm_native_api::NativeCallback,
    ) {
        if FS_IMAGE_NATIVE.contains(&(cls, name, desc)) {
            r.register(cls, name, desc, cb, cratonvm_native_api::NativeKind::Bridge);
        } else {
            r.register(cls, name, desc, cb, cratonvm_native_api::NativeKind::Bridge);
        }
    }

    // --- WinNTFileSystem / UnixFileSystem native methods ---
    //
    // These are JNI natives in the real JDK. We implement them using std::fs.
    //
    // EVERY operation is registered under BOTH spellings — the bare pre-JDK-22
    // name and the `0`-suffixed JDK-22+ name — because the two JDK generations
    // put the native in different places and only one of them is ever called
    // on a given JDK. See the `fs_boolean_attributes0` banner in this file for
    // why registering only the bare name silently registers a method nothing
    // calls on a modern JDK.
    for fs_cls in &["java/io/WinNTFileSystem", "java/io/UnixFileSystem"] {
        r.register(
            fs_cls,
            "canonicalize0",
            "(Ljava/lang/String;)Ljava/lang/String;",
            |ctx, args| {
                let path_ref = obj_arg(args, 1)?;
                let p = ctx.read_string(path_ref).unwrap_or_default();
                // Normalize even for non-existent paths: make absolute and
                // collapse `.`/`..` so containment checks behave like the
                // real JDK. Strips the `\\?\` extended-length prefix too.
                let canonical = file_canonicalize_path(&p)?;
                let s = ctx.create_string(&canonical);
                Ok(Some(Value::Object(Some(s))))
            },
            cratonvm_native_api::NativeKind::Bridge,
        );

        // getBooleanAttributes returns a bitmask:
        // BA_EXISTS=0x01, BA_REGULAR=0x02, BA_DIRECTORY=0x04, BA_HIDDEN=0x08
        //
        // `getBooleanAttributes0` is the raw JNI native (JDK 22+) and
        // `getBooleanAttributes` the pre-22 spelling / public wrapper; they
        // differ only in BA_HIDDEN, which is why they get different bodies.
        r.register(
            fs_cls,
            "getBooleanAttributes0",
            "(Ljava/io/File;)I",
            |ctx, args| {
                let file_ref = obj_arg(args, 1)?;
                let path = file_read_path(ctx, file_ref);
                Ok(Some(Value::Int(fs_boolean_attributes0(&path))))
            },
            cratonvm_native_api::NativeKind::Bridge,
        );
        fs_reg(
            r,
            fs_cls,
            "getBooleanAttributes",
            "(Ljava/io/File;)I",
            |ctx, args| {
                let file_ref = obj_arg(args, 1)?;
                let path = file_read_path(ctx, file_ref);
                Ok(Some(Value::Int(fs_boolean_attributes(&path))))
            },
        );

        for name in ["getLastModifiedTime", "getLastModifiedTime0"] {
            fs_reg(r, fs_cls, name, "(Ljava/io/File;)J", |ctx, args| {
                let file_ref = obj_arg(args, 1)?;
                let path = file_read_path(ctx, file_ref);
                let millis = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .map(|t| {
                        t.duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as i64
                    })
                    .unwrap_or(0);
                Ok(Some(Value::Long(millis)))
            });
        }

        for name in ["getLength", "getLength0"] {
            fs_reg(r, fs_cls, name, "(Ljava/io/File;)J", |ctx, args| {
                let file_ref = obj_arg(args, 1)?;
                let path = file_read_path(ctx, file_ref);
                let len = std::fs::metadata(&path)
                    .map(|m| m.len() as i64)
                    .unwrap_or(0);
                Ok(Some(Value::Long(len)))
            });
        }

        for name in ["list", "list0"] {
            fs_reg(
                r,
                fs_cls,
                name,
                "(Ljava/io/File;)[Ljava/lang/String;",
                |ctx, args| {
                    let file_ref = obj_arg(args, 1)?;
                    let path = file_read_path(ctx, file_ref);
                    // `null`, not an empty array, when the path is not a
                    // readable directory — `File.list()` documents the
                    // difference and callers branch on it.
                    let entries = match fs_list_dir(&path) {
                        Some(e) => e,
                        None => return Ok(Some(Value::Object(None))),
                    };
                    // A `String[]`, not the untyped `Object[]` that
                    // `new_array(ArrayElementType::Reference, ..)` produces:
                    // the descriptor is `[Ljava/lang/String;` and
                    // `File.normalizedList()` assigns the result straight to a
                    // `String[]` local, which is a checkcast.
                    let string_class = string_class_id(ctx);
                    let arr = ctx.new_ref_array(string_class, entries.len());
                    // Pin across the `create_string` calls below — a moving
                    // young GC there relocates the fresh array (the native
                    // stale-local family), same as `java/io/File.list()`.
                    let arr_pin = ctx.pin_native_root(arr);
                    for (i, name) in entries.iter().enumerate() {
                        let s = ctx.create_string(name);
                        let arr = ctx.read_native_pin(arr_pin, arr);
                        ctx.set_array_element(arr, i, Value::Object(Some(s)));
                    }
                    let arr = ctx.read_native_pin(arr_pin, arr);
                    ctx.unpin_native_roots(arr_pin);
                    Ok(Some(Value::Object(Some(arr))))
                },
            );
        }

        for name in ["createDirectory", "createDirectory0"] {
            fs_reg(r, fs_cls, name, "(Ljava/io/File;)Z", |ctx, args| {
                let file_ref = obj_arg(args, 1)?;
                let path = file_read_path(ctx, file_ref);
                let ok = std::fs::create_dir(&path).is_ok();
                Ok(Some(Value::Int(if ok { 1 } else { 0 })))
            });
        }

        r.register(
            fs_cls,
            "rename0",
            "(Ljava/io/File;Ljava/io/File;)Z",
            |ctx, args| {
                let from_ref = obj_arg(args, 1)?;
                let to_ref = obj_arg(args, 2)?;
                let from = file_read_path(ctx, from_ref);
                let to = file_read_path(ctx, to_ref);
                let ok = std::fs::rename(&from, &to).is_ok();
                Ok(Some(Value::Int(if ok { 1 } else { 0 })))
            },
            cratonvm_native_api::NativeKind::Bridge,
        );

        fs_reg(r, fs_cls, "delete0", "(Ljava/io/File;)Z", |ctx, args| {
            let file_ref = obj_arg(args, 1)?;
            let path = file_read_path(ctx, file_ref);
            let ok = std::fs::remove_file(&path)
                .or_else(|_| std::fs::remove_dir(&path))
                .is_ok();
            Ok(Some(Value::Int(if ok { 1 } else { 0 })))
        });
        // `WinNTFileSystem.delete0` takes a second `allowDeleteReadOnlyFiles`
        // argument (`java.io.File.allowDeleteReadOnlyFiles`, default false)
        // that `UnixFileSystem.delete0` does not have — a different method,
        // not a different spelling, so it needs its own registration or
        // `File.delete()` is an `UnsatisfiedLinkError` on Windows.
        fs_reg(r, fs_cls, "delete0", "(Ljava/io/File;Z)Z", |ctx, args| {
            let file_ref = obj_arg(args, 1)?;
            let allow_read_only = matches!(args.get(2), Some(v) if v.as_int().unwrap_or(0) != 0);
            let path = file_read_path(ctx, file_ref);
            if allow_read_only {
                if let Ok(md) = std::fs::metadata(&path) {
                    let mut perms = md.permissions();
                    if perms.readonly() {
                        // `set_readonly(false)` is right on Windows, where it
                        // clears FILE_ATTRIBUTE_READONLY and that IS the whole
                        // operation `allowDeleteReadOnlyFiles` asks for. On Unix
                        // the same call writes mode 0o666 — it grants write to
                        // group and other as well, which is a permission
                        // widening nobody asked for (clippy::
                        // permissions_set_readonly_false). Restore the owner
                        // write bit only; that is the Unix reading of "make it
                        // writable again", and deletion there depends on the
                        // DIRECTORY's mode anyway.
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            let mode = perms.mode();
                            perms.set_mode(mode | 0o200);
                        }
                        #[cfg(not(unix))]
                        perms.set_readonly(false);
                        let _ = std::fs::set_permissions(&path, perms);
                    }
                }
            }
            let ok = std::fs::remove_file(&path)
                .or_else(|_| std::fs::remove_dir(&path))
                .is_ok();
            Ok(Some(Value::Int(if ok { 1 } else { 0 })))
        });

        for name in ["setLastModifiedTime", "setLastModifiedTime0"] {
            fs_reg(r, fs_cls, name, "(Ljava/io/File;J)Z", |ctx, args| {
                // Real-JDK `File.setLastModified(long)` bytecode delegates here.
                // This used to be a stub that claimed success without touching
                // the file, so any caller that reached the bytecode path (rather
                // than the direct `java/io/File.setLastModified` native below)
                // got `true` and an unchanged timestamp. Share the same helper
                // so both entry points behave identically for files AND
                // directories.
                let file_ref = obj_arg(args, 1)?;
                let path = file_read_path(ctx, file_ref);
                let millis = match args.get(2) {
                    Some(Value::Long(v)) => *v,
                    _ => 0,
                };
                Ok(Some(Value::Int(if set_file_mtime_millis(&path, millis) {
                    1
                } else {
                    0
                })))
            });
        }

        // Was a hardcoded `true`: `File.setReadOnly()`'s real bytecode
        // delegates here, so a caller was told the file had been made
        // read-only while its permissions were untouched — and a follow-up
        // `canWrite()` (which IS a real permission query) then contradicted
        // it. Same body as the direct `java/io/File.setReadOnly()` native.
        for name in ["setReadOnly", "setReadOnly0"] {
            fs_reg(r, fs_cls, name, "(Ljava/io/File;)Z", |ctx, args| {
                let file_ref = obj_arg(args, 1)?;
                let path = file_read_path(ctx, file_ref);
                let ok = std::fs::metadata(&path)
                    .and_then(|meta| {
                        let mut perms = meta.permissions();
                        perms.set_readonly(true);
                        std::fs::set_permissions(&path, perms)
                    })
                    .is_ok();
                Ok(Some(Value::Int(if ok { 1 } else { 0 })))
            });
        }

        // `File.setWritable`/`setReadable`/`setExecutable` bytecode lands here.
        // Never registered before, so all three were an `UnsatisfiedLinkError`
        // for any caller that reached the real `FileSystem` bytecode.
        for name in ["setPermission", "setPermission0"] {
            fs_reg(r, fs_cls, name, "(Ljava/io/File;IZZ)Z", |ctx, args| {
                let file_ref = obj_arg(args, 1)?;
                let path = file_read_path(ctx, file_ref);
                let access = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
                let enable = args.get(3).and_then(|v| v.as_int()).unwrap_or(0) != 0;
                let owner_only = args.get(4).and_then(|v| v.as_int()).unwrap_or(0) != 0;
                let ok = fs_set_permission(&path, access, enable, owner_only);
                Ok(Some(Value::Int(if ok { 1 } else { 0 })))
            });
        }

        // `File.createNewFile()` / `File.createTempFile(...)` bytecode. Same
        // story as `setPermission`: never registered under either spelling.
        for name in ["createFileExclusively", "createFileExclusively0"] {
            fs_reg(r, fs_cls, name, "(Ljava/lang/String;)Z", |ctx, args| {
                let path_ref = obj_arg(args, 1)?;
                let path = ctx.read_string(path_ref).unwrap_or_default();
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                {
                    Ok(_) => Ok(Some(Value::Int(1))),
                    // The JDK returns false only for "already exists"; every
                    // other errno is an IOException the caller must see.
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        Ok(Some(Value::Int(0)))
                    }
                    // JIT r23 wave 5, lane L3 (`CRATONVM_IO_WINDOWS_OS_ERROR_REASON`):
                    // `JNU_ThrowIOExceptionWithLastError` gives the OS text
                    // alone, no path: `strerror` on Linux ("No such file or
                    // directory"), the system text on Windows.
                    Err(e) => Err(RuntimeError::IOException {
                        message: cratonvm_native_io::os_error_reason(&e)
                            .unwrap_or_else(|| format!("{path}: {e}")),
                    }
                    .into()),
                }
            });
        }

        // real-JDK `File.getTotalSpace()`/`getFreeSpace()`/`getUsableSpace()`
        // delegate to `FileSystem.getSpace(File, int)` (SPACE_TOTAL=0,
        // SPACE_FREE=1, SPACE_USABLE=2) rather than being native themselves —
        // this path is normally shadowed by the direct natives registered on
        // `java/io/File` itself (see `file_disk_space_bytes` below), but once
        // any test in the process instruments `java.io.File` via Mockito's
        // inline mock maker (`@Mock private File f`), the real (redefined)
        // `File` bytecode runs and reaches this native instead — must return
        // the same real values, not a hardcoded stub, or a mixed
        // mocked/real-File test class (e.g. `DiskSpaceHealthIndicatorTests`)
        // gets a correct answer for the mocked instances but a fake one for
        // real `File`s in the same JVM process.
        for name in ["getSpace", "getSpace0"] {
            fs_reg(r, fs_cls, name, "(Ljava/io/File;I)J", |ctx, args| {
                let file_ref = obj_arg(args, 1)?;
                let space_type = match args.get(2) {
                    Some(Value::Int(v)) => *v,
                    _ => 2,
                };
                let path = file_read_path(ctx, file_ref);
                let value =
                    file_disk_space_bytes(&path).map_or(
                        0,
                        |(total, free, usable)| match space_type {
                            0 => total,
                            1 => free,
                            _ => usable,
                        },
                    );
                Ok(Some(Value::Long(value as i64)))
            });
        }

        // Was "does the path exist?" for every mode, so `canWrite()` said
        // `true` for a file `setReadOnly()` had just locked down.
        for name in ["checkAccess", "checkAccess0"] {
            fs_reg(r, fs_cls, name, "(Ljava/io/File;I)Z", |ctx, args| {
                let file_ref = obj_arg(args, 1)?;
                let path = file_read_path(ctx, file_ref);
                let access = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
                Ok(Some(Value::Int(if fs_check_access(&path, access) {
                    1
                } else {
                    0
                })))
            });
        }

        // Was a flat 255. Unix can answer this exactly the way the JDK's own
        // `UnixFileSystem.getNameMax0` does — `pathconf(_PC_NAME_MAX)` — and
        // that is not always 255 (an eCryptfs mount reports 143, and callers
        // like Lucene/H2 use the value to decide how long a generated file name
        // may be, so an over-long name fails at create time instead).
        //
        // The return type is NOT the same on both platforms: `long` on
        // `UnixFileSystem`, `int` on `WinNTFileSystem`. Registering only the
        // `I` form left the Unix native unresolved, since a native is looked
        // up by descriptor as well as name.
        fs_reg(
            r,
            fs_cls,
            "getNameMax0",
            "(Ljava/lang/String;)I",
            |ctx, args| {
                let path = match args.get(1) {
                    Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                    _ => String::new(),
                };
                Ok(Some(Value::Int(file_system_name_max(&path))))            },
        );
        fs_reg(
            r,
            fs_cls,
            "getNameMax0",
            "(Ljava/lang/String;)J",
            |ctx, args| {
                let path = match args.get(1) {
                    Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                    _ => String::new(),
                };
                Ok(Some(Value::Long(file_system_name_max(&path) as i64)))            },
        );

        // `WinNTFileSystem.canonicalize(String)` is
        // `getFinalPath(canonicalize0(s))`, so this native's result IS what a
        // caller reaching real `FileSystem` bytecode gets back. It used to
        // return `std::fs::canonicalize`'s output verbatim, which on Windows
        // carries the `\\?\` extended-length prefix that the real
        // `GetFinalPathNameByHandleW`-based native strips: `canonicalize` came
        // back as `\\?\C:\...\x.txt` where HotSpot returns `C:\...\x.txt`, so
        // every `startsWith(canonicalBase)` containment check against a
        // non-prefixed base failed. `strip_unc` is the same helper
        // `file_canonicalize_path_uncached` already applies for this reason.
        fs_reg(
            r,
            fs_cls,
            "getFinalPath0",
            "(Ljava/lang/String;)Ljava/lang/String;",
            |ctx, args| {
                let path_ref = obj_arg(args, 1)?;
                let p = ctx.read_string(path_ref).unwrap_or_default();
                let final_path = std::fs::canonicalize(&p)
                    .map(|c| strip_unc(&c.to_string_lossy()))
                    .unwrap_or(p);
                let s = ctx.create_string(&final_path);
                Ok(Some(Value::Object(Some(s))))
            },
        );

        // --- WinNTFileSystem-only natives ---
        // Registered on both class names for symmetry with the loop; only the
        // Windows class ever declares them, and a native is only reachable
        // through a declared method.

        // `File.listRoots()` -> a bitmask with bit 0 = `A:`.
        fs_reg(r, fs_cls, "listRoots0", "()I", |_ctx, _args| {
            Ok(Some(Value::Int(fs_list_roots_bitmask())))
        });

        // The per-drive working directory, minus its `X:` prefix — what
        // `_wgetdcwd` gives the JDK. Reached only for drive-relative paths
        // (`C:foo`), which `WinNTFileSystem.resolve` has to expand.
        fs_reg(
            r,
            fs_cls,
            "getDriveDirectory",
            "(I)Ljava/lang/String;",
            |ctx, args| {
                let drive = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
                if !(1..=26).contains(&drive) {
                    return Ok(Some(Value::Object(None)));
                }
                let letter = (b'A' + (drive - 1) as u8) as char;
                // Windows keeps each drive's working directory in a hidden
                // `=X:` environment variable; that IS what `_wgetdcwd` reads.
                let dir = cratonvm_types::flags::runtime_var(format!("={letter}:"))
                    .ok()
                    .or_else(|| {
                        let cwd = std::env::current_dir().ok()?;
                        let cwd = cwd.to_string_lossy().to_string();
                        cwd.starts_with(&format!("{letter}:")).then_some(cwd)
                    })
                    .unwrap_or_else(|| format!("{letter}:\\"));
                // Strip the `X:` prefix the JDK's native also strips.
                let stripped = dir.get(2..).unwrap_or("\\").to_string();
                let s = ctx.create_string(&stripped);
                Ok(Some(Value::Object(Some(s))))
            },
        );

        // The real native caches jfieldIDs; there is nothing for us to cache,
        // but the class's `<clinit>` calls it and an unregistered native there
        // fails class initialisation outright.
        r.register(
            fs_cls,
            "initIDs",
            "()V",
            |_ctx, _args| Ok(None),
            cratonvm_native_api::NativeKind::Bridge,
        );
    }

    // Round 24 — Files.write(Path, Iterable<? extends CharSequence>, OpenOption...)
    // and the (..., Charset, ...) overload. Used by JBoss
    // ProcessEnvironment.obtainProcessUUID to write standalone/data/process.uuid.
    // Also Files.write(Path, byte[], OpenOption...) — needed in real-JDK mode
    // where register_p71_files_bridge is not invoked. Without these, the JDK
    // bytecode falls through to FileSystemProvider.newOutputStream which we
    // do not implement and throws "Не удается найти указанный файл" (errno 2).
    let files_cls = "java/nio/file/Files";
    reg_files! { r.register(files_cls,
        "write",
        "(Ljava/nio/file/Path;[B[Ljava/nio/file/OpenOption;)Ljava/nio/file/Path;",
        files_write_bytes_impl, cratonvm_native_api::NativeKind::Bridge,
    ) }
    fn write_iterable_impl(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> cratonvm_types::error::MethodCallResult {
        let path_obj = obj_arg(args, 0)?;
        let iterable: ObjectRef = obj_arg(args, 1)?;
        let options = args.get(2).copied();
        let opts_obj = match options {
            Some(Value::Object(Some(o))) => Some(o),
            _ => None,
        };
        // Everything below re-enters Java repeatedly (iterator/hasNext/next/
        // toString), so the Path, the OpenOption[] and the Iterator all have to
        // be pinned — `path_pin` FIRST, since unpinning is by batch base.
        let path_pin = ctx.pin_native_root(path_obj);
        let opts_pin = opts_obj.map(|o| ctx.pin_native_root(o));
        let it_val = ctx
            .invoke_virtual(iterable, "iterator", "()Ljava/util/Iterator;", &[])
            .ok()
            .flatten();
        let it = match it_val {
            Some(Value::Object(Some(o))) => o,
            _ => {
                ctx.unpin_native_roots(path_pin);
                return Err(RuntimeError::IOException {
                    message: "Files.write(Iterable): null iterator".to_string(),
                }
                .into());
            }
        };
        let it_pin = ctx.pin_native_root(it);
        // `Files.write(Path, Iterable<? extends CharSequence>, …)` terminates
        // EVERY element with `System.lineSeparator()` — the JDK's own
        // implementation is `for (CharSequence line : lines) { writer.append(line);
        // writer.newLine(); }`, and `BufferedWriter.newLine()` is the platform
        // separator. This used to push a bare `'\n'`, so on Windows CratonVM
        // wrote `alpha\nbeta\n` where HotSpot writes `alpha\r\nbeta\r\n`.
        //
        // The sibling `Files.writeString`/`Files.write(byte[])` deliberately
        // append NOTHING, and that asymmetry is the whole reason this is easy to
        // get backwards: only the Iterable overload has lines to terminate.
        //
        // Read once, before the loop: the iteration below re-enters Java, and a
        // property lookup per element would be both slower and — if some
        // element's `toString` changed the property — inconsistent within a
        // single file.
        let sep = p57_line_separator(ctx);
        let mut out = String::new();
        loop {
            let it = ctx.read_native_pin(it_pin, it);
            let has = ctx.invoke_virtual(it, "hasNext", "()Z", &[]).ok().flatten();
            match has {
                Some(Value::Int(1)) => {}
                _ => break,
            }
            let it = ctx.read_native_pin(it_pin, it);
            let nxt = ctx
                .invoke_virtual(it, "next", "()Ljava/lang/Object;", &[])
                .ok()
                .flatten();
            let elem = match nxt {
                Some(Value::Object(Some(o))) => o,
                _ => break,
            };
            // CharSequence: try read_string first (Strings), fall back to toString().
            let s = ctx.read_string(elem).unwrap_or_else(|| {
                let s_val = ctx
                    .invoke_virtual(elem, "toString", "()Ljava/lang/String;", &[])
                    .ok()
                    .flatten();
                match s_val {
                    Some(Value::Object(Some(so))) => ctx.read_string(so).unwrap_or_default(),
                    _ => String::new(),
                }
            });
            out.push_str(&s);
            out.push_str(&sep);
        }
        // The iteration above re-entered Java (`hasNext`/`next`/`toString`), so
        // the pinned `Path` is the only reference still safe to return.
        let path_obj = ctx.read_native_pin(path_pin, path_obj);
        let options = match (opts_obj, opts_pin) {
            (Some(o), Some(h)) => Some(Value::Object(Some(ctx.read_native_pin(h, o)))),
            _ => None,
        };
        ctx.unpin_native_roots(path_pin);
        p57_files_write_bytes(ctx, path_obj, out.as_bytes(), options)
    }
    reg_files! { r.register(files_cls,
        "write",
        "(Ljava/nio/file/Path;Ljava/lang/Iterable;[Ljava/nio/file/OpenOption;)Ljava/nio/file/Path;",
        write_iterable_impl, cratonvm_native_api::NativeKind::Bridge,
    ) }
    reg_files! { r.register(files_cls,
        "write",
        "(Ljava/nio/file/Path;Ljava/lang/Iterable;Ljava/nio/charset/Charset;[Ljava/nio/file/OpenOption;)Ljava/nio/file/Path;",
        |ctx, args| {
            // Charset arg at slot 2 ignored — we always emit UTF-8, matching
            // the platform default we report from `Charset.defaultCharset`.
            // Forward the (path, iterable, options) shape by reordering.
            let trimmed: Vec<Value> = vec![args[0], args[1], args.get(3).copied().unwrap_or(Value::Object(None))];
            write_iterable_impl(ctx, &trimmed)
        },
        cratonvm_native_api::NativeKind::Bridge,
    ) }

}

pub fn vfs_entry_as_real_channel(ctx: &mut dyn NativeContext, data: &[u8]) -> Option<Value> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static VFS_ENTRY_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = VFS_ENTRY_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "cratonvm-vfs-entry-{}-{:x}.tmp",
        std::process::id(),
        n
    ));
    std::fs::write(&path, data).ok()?;
    let p = path.to_string_lossy().replace('\\', "/");
    let fd_id = crate::capability_gate::open_read_gated(&*ctx, &p).ok()?;
    open_real_filechannel(ctx, fd_id, &p, true, false, false)
}

pub fn open_real_filechannel(
    ctx: &mut dyn NativeContext,
    fd_id: cratonvm_native_api::fd_table::FdId,
    p: &str,
    readable: bool,
    writable: bool,
    append: bool,
) -> Option<Value> {
    // GC DISCIPLINE. The `FileDescriptor` and the path `String`
    // below are freshly allocated and NOTHING in Java refers to
    // either until `FileChannelImpl.open` stores them, so a Rust
    // local is each one's only reference — across `create_string`,
    // which allocates, and `ensure_class_initialized`, which runs
    // `FileChannelImpl`'s class initializer, i.e. arbitrary
    // bytecode. Under a moving collector the locals go stale;
    // under the Generational collector's NON-MOVING young sweep an
    // unreachable object is ZEROED IN PLACE, and a zeroed
    // `FileDescriptor` reads its `fd`/`handle` back as 0.
    //
    // That is the shape of
    // `known-issues/springboot/generational-non-moving-sweep-zeroes-a-live-filechannel-20260906.md`,
    // whose symptom is `FileChannel.map: invalid fd` from a
    // channel that opened cleanly. `native_fcimpl_open` and
    // `new_native_thread_set` in `native-io` were converted on
    // 2026-09-06; this site is the same construction one provider
    // layer up and was missed, because it lives in a CLOSURE
    // registered inside `register_phase57_nio_file` and both
    // shipped audits index by top-level `fn`.
    //
    // Local reproducer: `test_classes/gc/NioChannelChurn.java`.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let fd_h = {
        let o = match scope.new_object("java/io/FileDescriptor").ok()?? {
            Value::Object(Some(o)) => o,
            _ => return None,
        };
        scope.root(o)
    };
    // The platform's spelling (`CRATONVM_FD_FIELDS_PER_PLATFORM`, JIT r23
    // w1 L6): Unix `fd = id, handle = -1` (`UnixChannelFactory`), Windows
    // `fd = -1, handle = id` (`WindowsChannelFactory`); both hold the id
    // with the switch off. `fd_from_descriptor` reads either.
    let (fd_field, handle_field) = cratonvm_native_io::nio_native::descriptor_fields_for_id(
        fd_id,
        cratonvm_native_io::nio_native::fd_fields_per_platform_enabled(),
    );
    let fd_obj = scope.get(&fd_h);
    scope.set_field_by_name(fd_obj, "handle", Value::Long(handle_field));
    let fd_obj = scope.get(&fd_h);
    scope.set_field_by_name(fd_obj, "fd", Value::Int(fd_field));
    // `FileDescriptor.append`: what `FileChannelImpl.position()` asks
    // (`fdAccess.getAppend(fd)`: an appending channel's position is its
    // size) and what `transferTo0` / `transferFrom0` are told. The JDK's
    // `UnixChannelFactory` sets it with `fdAccess.setAppend`; without it
    // an APPEND channel reported the seek offset (nio20 wave 3, L1;
    // `Nio20Conf` `append` rows).
    if append {
        let fd_obj = scope.get(&fd_h);
        scope.set_field_by_name(fd_obj, "append", Value::Int(1));
    }
    let path_h = {
        let o = scope.create_string(p);
        scope.root(o)
    };
    scope
        .ensure_class_initialized("sun/nio/ch/FileChannelImpl")
        .ok()?;
    let (fd_obj, path_str) = (scope.get(&fd_h), scope.get(&path_h));
    match scope.invoke(
        "sun/nio/ch/FileChannelImpl",
        "open",
        "(Ljava/io/FileDescriptor;Ljava/lang/String;ZZZZLjava/io/Closeable;)Ljava/nio/channels/FileChannel;",
        &[
            Value::Object(Some(fd_obj)),
            Value::Object(Some(path_str)),
            Value::Int(readable as i32),
            Value::Int(writable as i32),
            Value::Int(0), // sync
            Value::Int(0), // direct
            Value::Object(None), // parent
        ],
    ) {
        Ok(Some(v @ Value::Object(Some(_)))) => Some(v),
        _ => None,
    }
}
