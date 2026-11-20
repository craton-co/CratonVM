#[allow(unused_imports)]
use super::super::*;

// JDK-ONLY-CLASSIFY: unknown — needs census, at the granularity of this whole
// crate. The `set_category(Bridge)` below is the crate's root ambient
// assignment: 1,105 of `native-io`'s 1,129 registrations end up `Bridge`
// because of it, either directly or through a callee that never sets a category
// of its own. Unlike `native-collections`, that tag is often RIGHT here — this
// crate does own genuine OS boundaries — but it is right by luck of placement,
// not by per-site judgement. Measured against JDK 25 with `javap -p -s`, 86 of
// the resolvable `Bridge` triples target an ACC_NATIVE method (`sun.nio.ch.Net`,
// `FileInputStream`/`FileOutputStream`'s `*0` family, `RandomAccessFile`,
// `ProcessHandleImpl`) while 307 shadow methods that have concrete bytecode.
//
// Note the ambient value is also caller-visible: this function saves and
// restores `__prev_cat`, and `vm/src/vm/vm_init.rs` calls it while the registry
// is at its DEFAULT category, which is `SyntheticStub`. If the
// `set_category(Bridge)` line below were ever moved after a registration, that
// registration would become a synthetic stub with no syntactic marker at all.
// Per-entry-point verdicts are annotated on the `register_*` functions.
//
// CORRECTIONS 2026-08-20 (H5-1), from a re-derivation of every number above:
//
//  * "or through a callee that never sets a category of its own" IS NOW
//    FALSE. All 29 functions in this crate that call `register`/
//    `register_with_kind` open with an explicit `set_category(...)` and close
//    with a `set_category(__prev_cat)` restore. There is no ambient-inheriting
//    registrar left in `native-io`. Nothing here needs "making explicit".
//  * The 86/307 split is stale and low. Re-derived by parsing all 1,113
//    `.register(` / `.register(` call sites in `native-io/src`
//    (1,098 resolved statically, including loop and `let`-bound class names;
//    15 unresolved, and about a third of THOSE are the regex matching comment
//    prose rather than code) and adjudicating each triple with `javap -p -s`
//    against JDK 25.0.3+9. Expanding the loops gives 1,493 registrations and
//    **1,457 distinct triples**, which split: **106 ACC_NATIVE, 656 shadowing
//    concrete bytecode, 222 abstract, 367 naming a method the class does not
//    declare, and 106 on 18 classes the image does not contain.**
//    Both counts are floors in different directions
//    — this one includes `#[cfg(feature = "synthetic-jdk")]` and
//    flag-gated blocks that a given binary may never register, and the JDK
//    image measured is the WINDOWS one, on which `sun/nio/fs/Unix*`,
//    `sun/nio/ch/EPoll*` and `KQueuePort` read as absent classes purely
//    because this host has no Linux image. Take the shape, not the digits.
//  * `C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot`, cited by 66
//    records in `docs/known-issues/jdk-only/` and by comments in this file,
//    DOES NOT EXIST on this host. The JDK 25.0.3+9 oracle is
//    `C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot`. Resolve it rather than
//    pasting it: `JDK="$(dirname "$(dirname "$(command -v javap)")")"`.
pub fn register_io_natives(registry: &mut NativeMethodRegistry) {
            // W7-69: publish this crate's ByteBuffer slot map so the read-side alias
    // sweep can check it against the loaded class. Unconditional and outside
    // the flag check on purpose — the list is one `&'static` push per process,
    // and gating it would leave a run that enables the flag later with nothing
    // to sweep. `register_io_natives` is called in BOTH arms of `vm_init`'s
    // `if config.use_synthetic_jdk` fork, so the map is published in Compatible
    // mode too, where it is exactly the mode the census is about.
    read_alias::declare_slot_map(&BB_SLOT_MAP);
    // SECURITY FIX (V12): apply the requested hardening deployment profile
    // before any I/O natives are registered, so a deployment that requests it
    // (CRATONVM_CONFINE_IO / CRATONVM_UNTRUSTED_CODE) fails closed — CWD
    // confinement on + CWD registered as a sandbox root — without the embedder
    // having to call set_path_confine_to_cwd/add_sandbox_root manually. The
    // env-less default is unchanged (permissive, JDK-faithful).
    apply_certified_deployment_profile();

    // WP1.12 — real subprocess + ProcessHandleImpl (must override JDK natives)
    process::register_process_natives(registry);

    // --- sun.nio.ch.FileDispatcherImpl / FileChannelImpl / NativeThread / IOUtil ---
    // Real-JDK-mode NIO natives. Must be registered before any JDK
    // bytecode that touches sun.nio.ch runs, because these natives
    // take raw memory pointers and would segfault silently if
    // unregistered (the VM would try to dispatch to a null callback).
    nio_native::register_nio_natives_real(registry);

    // jdk.internal.jimage.NativeImageBuffer.getNativeMap — the run-time
    // image memory map. `BasicImageReader.<init>` calls this during boot of
    // the module system (`ModuleFinder.ofSystem()` → `SystemModuleFinders`).
    // Registered here (not inside the synthetic-jdk-gated `register_nio_natives`)
    // so it is present in real-JDK mode, where the module finder runs the
    // genuine JDK bytecode.
    registry.register(
        "jdk/internal/jimage/NativeImageBuffer",
        "getNativeMap",
        "(Ljava/lang/String;)Ljava/nio/ByteBuffer;",
        native_jimage_get_native_map,
        NativeKind::Bridge,
    );

    // Phase B (RB.3 / RB.4): real-mode sun.nio.cs.StreamDecoder /
    // StreamEncoder shims.  These override the JDK bytecode that
    // reaches into unimplemented sun.nio.ch internals.
    stream_decoder::register_stream_decoder_natives(registry);
    stream_encoder::register_stream_encoder_natives(registry);

    // RA.7: real JarFile / ZipFile natives (zip crate backed).
    zip_real_jar::register_jar_natives(registry);

    // T19.7.a — sun.nio.ch.Selector / SelectionKey / SelectableChannel
    // readiness multiplexer.  XNIO / Undertow / Vert.x build on top of
    // this; without it, async HTTP servers can't demux connections.
    nio_selector::register_nio_selector(registry);

    // --- Wave 3 NIO / async I/O ---
    // Registered AFTER `register_nio_natives_real` so that any colliding
    // (class, method, descriptor) triple is overwritten by the real
    // implementation. Agents B/C/D/E delivered file-disjoint modules; this
    // is the single integration call site.
    //
    // WP3.3 + WP3.6 — FileChannel.map (memmap2) + transferTo (sendfile /
    // TransmitFile / userspace fallback). Supersedes the deliberate
    // `native_fc_map0_real` / `native_fc_unmap0_real` /
    // `native_fc_transfer_to0` / `native_fc_max_direct_transfer_size0`
    // stubs in `nio_native.rs:317-345`.
    file_channel::register_file_channel_real(registry);
    // `FileChannelImpl.read/write(ByteBuffer)` as one native call instead of
    // twenty JDK frames. Registered AFTER `register_file_channel_real` so a
    // future overlap there is decided by this file, and gated by
    // `CRATONVM_FC_FAST_IO=0` — see `file_channel_fast_read`.
    file_channel_fast_read::register_file_channel_fast_io(registry);
    // WP3.4 — non-blocking SocketChannel / ServerSocketChannel.
    socket_channel::register_socket_channel_real(registry);
    // WP3.2 — AIO socket channels + AsynchronousChannelGroup. Supersedes
    // the synthetic `t16_asc_*` / `t16_acg_*` stubs registered in
    // `register_t16_channel_overrides`.
    async_socket::register_async_socket_real(registry);
    // WP3.5 — DirectByteBuffer real allocation + Bits accounting.
    direct_buffer::register_direct_buffer_real(registry);
    // WP3.7 — anonymous Pipe via real kernel pipes.
    pipe::register_pipe_real(registry);
    // WP3.7 — DatagramChannel send/receive/multicast. Coexists with the
    // existing `t16_dc_*` UDP family in `nio_native.rs` which uses a
    // separate registry; new `dgram_*0` natives use their own registry.
    datagram::register_datagram_real(registry);
    // WP3.8 — WatchService on `sun/nio/fs/{Unix,Windows,Polling}WatchService`.
    // File-disjoint from the existing `register_watch_service` (which
    // targets the public `java/nio/file/*` synthetic-jdk surface).
    watch::register_watch_service_real(registry);

    // --- java.io.File ---
    registry.register(
        "java/io/File",
        "<init>",
        "(Ljava/lang/String;)V",
        native_file_init_string, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/io/File",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        native_file_init_string_string, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/io/File",
        "<init>",
        "(Ljava/io/File;Ljava/lang/String;)V",
        native_file_init_file_string, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register("java/io/File", "exists", "()Z", native_file_exists, cratonvm_native_api::NativeKind::Bridge);
    registry.register("java/io/File", "isFile", "()Z", native_file_is_file, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        "java/io/File",
        "isDirectory",
        "()Z",
        native_file_is_directory, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register("java/io/File", "length", "()J", native_file_length, cratonvm_native_api::NativeKind::Bridge);
    registry.register("java/io/File", "delete", "()Z", native_file_delete, cratonvm_native_api::NativeKind::Bridge);
    registry.register("java/io/File", "mkdir", "()Z", native_file_mkdir, cratonvm_native_api::NativeKind::Bridge);
    registry.register("java/io/File", "mkdirs", "()Z", native_file_mkdirs, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        "java/io/File",
        "getName",
        "()Ljava/lang/String;",
        native_file_get_name, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/io/File",
        "getPath",
        "()Ljava/lang/String;",
        native_file_get_path, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/io/File",
        "getAbsolutePath",
        "()Ljava/lang/String;",
        native_file_get_absolute_path, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/io/File",
        "getParent",
        "()Ljava/lang/String;",
        native_file_get_parent, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/io/File",
        "list",
        "()[Ljava/lang/String;",
        native_file_list, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register("java/io/File", "canRead", "()Z", native_file_can_read, cratonvm_native_api::NativeKind::Bridge);
    registry.register("java/io/File", "canWrite", "()Z", native_file_can_write, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        "java/io/File",
        "createNewFile",
        "()Z",
        native_file_create_new_file, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/io/File",
        "renameTo",
        "(Ljava/io/File;)Z",
        native_file_rename_to, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- java.io.FileInputStream ---
    // FIS-FIX 2026-05-19: we deliberately do NOT register a native `<init>`
    // override. The real-JDK `FileInputStream` constructor allocates the
    // `fd` `FileDescriptor` and then calls the `open0` native — overriding
    // `<init>` skipped that allocation, so `open0`/`readBytes` had nowhere
    // valid to store/recover the OS handle (instance slot 0 is a *reference*
    // slot for the `FileDescriptor`, and a raw int written there is dropped).
    // The `open0`/`read0`/`readBytes`/`skip0`/`available0` natives below
    // store the handle on the `FileDescriptor` object instead.

    // --- java.io.FileOutputStream ---
    // FOS-FIX 2026-05-20: do NOT register native `<init>` overrides. As with
    // `FileInputStream` (see FIS-FIX), the real-JDK `FileOutputStream`
    // constructor allocates the `fd` `FileDescriptor` and calls the `open0`
    // native. Overriding `<init>` skipped that allocation, so the fd had to
    // be stored in instance slot 0 — but slot 0 is the *reference*-typed `fd`
    // field, and a raw `Value::Int` write there is silently dropped, leaving
    // every `write`/`flush`/`close` a no-op (empty files). The `open0`
    // native below stores the handle on the `FileDescriptor` object instead.
    // JDK-ONLY-CLASSIFY: H5-1 N5. JDK 25's `FileOutputStream` declares exactly
    // four ACC_NATIVE methods (`initIDs`, `open0`, `write(IZ)V`,
    // `writeBytes([BIIZ)V`); all four are registered `Bridge` further down
    // this function (see the block below with the `FileOutputStream: open0,
    // write(I,Z), writeBytes` comment). The five triples here are the PUBLIC
    // surface (`write(I)V`, `write([BII)V`, `write([B)V`, `flush()V`,
    // `close()V`), all of which have concrete bytecode — the same shape
    // `FileInputStream`'s public surface had before H5-A. `SyntheticStub` is
    // the correct classification so `vm_exec` prefers that real bytecode when
    // it is loaded; do not "fix" this to `Bridge`.
    // Real-library close must run its channel/lock cleanup in both modes.
    // These helpers serve only the synthetic class library (i56 residual).
    if !registry.real_jdk() {
        // `register_with_kind`, not `set_category` + `register`: these five
        // triples are a deliberate SyntheticStub classification (see the
        // comment above), and only `register_with_kind` records that on the
        // census as `kind_stated`. The previous `set_category`/`register`
        // pairing produced the identical runtime `kind` but left it
        // unstated, which the kind-map gate's one-way adjudication rule
        // reads as a lost decision on every re-freeze after this one.
        registry.register(
            "java/io/FileOutputStream",
            "write",
            "(I)V",
            native_fos_write_byte_jfr,
            cratonvm_native_api::NativeKind::SyntheticStub,
        );
        registry.register(
            "java/io/FileOutputStream",
            "write",
            "([BII)V",
            native_fos_write_bytes_jfr,
            cratonvm_native_api::NativeKind::SyntheticStub,
        );
        registry.register(
            "java/io/FileOutputStream",
            "write",
            "([B)V",
            native_fos_write_byte_array_jfr,
            cratonvm_native_api::NativeKind::SyntheticStub,
        );
        registry.register(
            "java/io/FileOutputStream",
            "flush",
            "()V",
            native_fos_flush,
            cratonvm_native_api::NativeKind::SyntheticStub,
        );
        registry.register(
            "java/io/FileOutputStream",
            "close",
            "()V",
            native_fos_close,
            cratonvm_native_api::NativeKind::SyntheticStub,
        );
    }

    // FOS-FIX addendum: the `<init>` overrides are intentionally NOT registered
    // for the default (real-JDK) build — there the real bytecode constructor
    // runs and allocates the `fd` `FileDescriptor`. But in `synthetic-jdk` mode
    // there is no real-JDK bytecode, so without these `new FileOutputStream(path)`
    // raises NoSuchMethodError. The synthetic streams use the legacy slot-0
    // `FdId` layout that these `<init>` natives and the `write`/`flush`/`close`
    // natives above all agree on, so they are safe here and only here.
    //
    // 2026-08-07: "here and only here" was enforced by the WRONG GUARD, and it
    // silently un-did the FOS-FIX above for anyone building with the feature.
    // `#[cfg(feature = "synthetic-jdk")]` asks what was COMPILED; what decides
    // whether a real `FileOutputStream` is on the other end is which CLASS
    // LIBRARY was LOADED, i.e. the launcher flag. A feature build run
    // `--real-jdk` satisfies the cfg and registered these over the real class,
    // reproducing the exact 2026-05-20 defect the comment above describes:
    // `<init>` skipped the real constructor, so no `FileDescriptor` was
    // allocated (`getFD()` threw), the fd went to instance slot 0 — the
    // reference-typed `fd` field, where an `Value::Int` write is dropped — and
    // every `write`/`flush`/`close` became a silent no-op.
    //
    // Measured on `regression-suite`'s `RFileTimes`: `new FileOutputStream(f)`
    // + `write(5 bytes)` + `close()` left a ZERO-LENGTH file, and the
    // `JarOutputStream` built on one produced an archive with no EOCD record.
    // `Files.write`, `Files.newOutputStream` and `RandomAccessFile` were all
    // unaffected, which is what kept it hidden.
    //
    // The runtime flag is the correct guard and is already set in exactly the
    // arms that matter — both real-JDK arms of `vm_init`, and neither
    // synthetic arm. The cfg stays as well: in a default build these natives
    // should not even be compiled in.
    #[cfg(feature = "synthetic-jdk")]
    if !registry.real_jdk() {
        registry.register(
            "java/io/FileOutputStream",
            "<init>",
            "(Ljava/lang/String;)V",
            native_fos_init_string, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/FileOutputStream",
            "<init>",
            "(Ljava/lang/String;Z)V",
            native_fos_init_string_append, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/FileOutputStream",
            "<init>",
            "(Ljava/io/File;)V",
            native_fos_init_file, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/FileOutputStream",
            "<init>",
            "(Ljava/io/File;Z)V",
            native_fos_init_file_append, cratonvm_native_api::NativeKind::Bridge,
        );
        // FileInputStream `<init>(String)` is likewise dropped in the default
        // (real-JDK) build (see FIS-FIX above) where the real ctor allocates the
        // `fd` and calls `open0`. In synthetic mode there is no bytecode ctor, so
        // route `<init>(String)` straight to the same logic `open0` runs.
        registry.register(
            "java/io/FileInputStream",
            "<init>",
            "(Ljava/lang/String;)V",
            native_fis_open0, cratonvm_native_api::NativeKind::Bridge,
        );
        // ...and the `File` overload, the one every `TckIo` fixture actually
        // uses. Its absence was invisible for as long as the corpus was dark.
        registry.register(
            "java/io/FileInputStream",
            "<init>",
            "(Ljava/io/File;)V",
            native_fis_init_file, cratonvm_native_api::NativeKind::Bridge,
        );
        // Public FileInputStream read surface. In real-JDK mode the bytecode
        // read()/read(byte[])/read(byte[],i,i)/available()/skip()/close() call
        // the internal read0/readBytes/available0/skip0 natives registered
        // above; synthetic mode has no bytecode, so wire the public methods
        // straight to the same native implementations.
        registry.register("java/io/FileInputStream", "read", "()I", native_fis_read, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            "java/io/FileInputStream",
            "read",
            "([BII)I",
            native_fis_read_bytes, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register("java/io/FileInputStream", "read", "([B)I", |ctx, args| {
            // read(byte[] b) == readBytes(b, 0, b.length)
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(-1))),
            };
            let arr = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Int(-1))),
            };
            let len = ctx.array_length(arr) as i32;
            native_fis_read_bytes(
                ctx,
                &[
                    Value::Object(Some(this)),
                    Value::Object(Some(arr)),
                    Value::Int(0),
                    Value::Int(len),
                ],
            )
        }, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            "java/io/FileInputStream",
            "available",
            "()I",
            native_fis_available, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register("java/io/FileInputStream", "skip", "(J)J", native_fis_skip, cratonvm_native_api::NativeKind::Bridge);
        registry.register("java/io/FileInputStream", "close", "()V", native_fis_close, cratonvm_native_api::NativeKind::Bridge);
    }

    // Real-JDK fallback: keep the public FileInputStream surface available for
    // synthetic fallback classes without stealing a real FileInputStream
    // constructor. `vm_exec` protects SyntheticStub-tagged FileInputStream
    // methods by preferring real bytecode when it exists.
    //
    // JDK-ONLY-CLASSIFY: stub — correctly tagged, and the tag is LOAD-BEARING.
    // JDK 25's `FileInputStream` declares exactly nine ACC_NATIVE methods
    // (`open0`, `read0`, `readBytes`, `length0`, `position0`, `skip0`,
    // `available0`, `isRegularFile0`, `initIDs`); every one of them is
    // registered `Bridge` further down this function. The seven triples in the
    // block below are the PUBLIC surface (`<init>(String)`, `read()`,
    // `read([B)`, `read([BII)`, `available()`, `skip(J)`, `close()`), all of
    // which have concrete bytecode — so `SyntheticStub` is the correct
    // classification and is what makes `vm_exec` prefer that bytecode. Do not
    // "fix" this to `Bridge`: the tag is the mechanism, not a mislabel.
    {
                        registry.register(
            "java/io/FileInputStream",
            "<init>",
            "(Ljava/lang/String;)V",
            native_fis_open0, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        registry.register("java/io/FileInputStream", "read", "()I", native_fis_read, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(
            "java/io/FileInputStream",
            "read",
            "([BII)I",
            native_fis_read_bytes, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        registry.register("java/io/FileInputStream", "read", "([B)I", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(-1))),
            };
            let arr = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Int(-1))),
            };
            let len = ctx.array_length(arr) as i32;
            native_fis_read_bytes(
                ctx,
                &[
                    Value::Object(Some(this)),
                    Value::Object(Some(arr)),
                    Value::Int(0),
                    Value::Int(len),
                ],
            )
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(
            "java/io/FileInputStream",
            "available",
            "()I",
            native_fis_available, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        registry.register("java/io/FileInputStream", "skip", "(J)J", native_fis_skip, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register("java/io/FileInputStream", "close", "()V", native_fis_close, cratonvm_native_api::NativeKind::SyntheticStub);
            }

    // --- JDK 25 real bytecode uses different method names for I/O natives ---
    // FileInputStream: open0, read0, readBytes, skip0, available0 etc.
    // `initIDs` only caches jfieldIDs for HotSpot's own JNI code; CratonVM
    // resolves fields by name, so there is nothing to cache and an empty body
    // is the spec-correct implementation.
    registry.register(
        "java/io/FileInputStream",
        "initIDs",
        "()V",
        native_noop,
        NativeKind::Bridge,
    );
    registry.register(
        "java/io/FileInputStream",
        "open0",
        "(Ljava/lang/String;)V",
        native_fis_open0,
        NativeKind::Bridge,
    );
    registry.register(
        "java/io/FileInputStream",
        "read0",
        "()I",
        native_fis_read,
        NativeKind::Bridge,
    );
    registry.register(
        "java/io/FileInputStream",
        "readBytes",
        "([BII)I",
        native_fis_read_bytes,
        NativeKind::Bridge,
    );
    // JDK-ONLY-CLASSIFY: RESOLVED 2026-08-20 (H5-1). The duplicate
    // `FileInputStream.read([BII)I` registration that used to sit here — the
    // repo's named instance of the last-write-wins overwrite hazard — is
    // DELETED. What stood here was:
    //
    //     registry.register("java/io/FileInputStream", "read", "([BII)I",
    //                       native_fis_read_bytes);
    //
    // registered under the ambient `Bridge` this function sets at its head, and
    // it silently overwrote the `SyntheticStub`-tagged registration of the SAME
    // triple with the SAME callback ~85 lines above (the public-surface block).
    //
    // Two facts settle which of the two is correct, and neither was in the
    // comment this replaces:
    //
    //  1. `javap -p -s java.io.FileInputStream` on JDK 25.0.3+9 shows
    //     `public int read(byte[], int, int)` with a `Code` attribute and NO
    //     `ACC_NATIVE`; the only native bulk read is the private
    //     `readBytes([BII)I`, which IS bridged (register_with_kind above).
    //     So §1.5 has nothing for `read([BII)I` to bind to and `Bridge` was
    //     wrong on the merits.
    //  2. BOTH registrations named `native_fis_read_bytes`. The old comment's
    //     reason for keeping the line — "silently demoting it would also change
    //     which of the two callbacks wins" — was false: there is only one
    //     callback. Deleting the line changes the slot's KIND and nothing else.
    //
    // Effect of the deletion: the triple keeps `native_fis_read_bytes` and
    // reverts to the `SyntheticStub` tag its six public-surface siblings
    // (`read()`, `read([B)`, `available()`, `skip(J)`, `close()`,
    // `<init>(String)`) already carry, which is what makes `vm_exec` prefer the
    // real bytecode. Under `--real-jdk` a `SyntheticStub` is refused at
    // registration, so the real `read([BII)I` bytecode runs and calls the
    // genuine `readBytes` bridge — one `Bridge`-tagged shadow retired.
    registry.register(
        "java/io/FileInputStream",
        "skip0",
        "(J)J",
        native_fis_skip,
        NativeKind::Bridge,
    );
    registry.register(
        "java/io/FileInputStream",
        "available0",
        "()I",
        native_fis_available,
        NativeKind::Bridge,
    );
    // `length0`/`position0` both returned a hardcoded 0 — i.e. "this file is
    // empty and we are at its start" for EVERY file. JDK 25's
    // `FileInputStream.readAllBytes()`/`available()` size their reads from
    // exactly these two, so the pair had to be worked around by lying about
    // `isRegularFile0` as well (see below). Both are answerable from the
    // fd table: `file_size` is the real length, and `available()` (already
    // used by the `available0` native) is bytes-remaining, so
    // `position = length - available`.
    registry.register(
        "java/io/FileInputStream",
        "length0",
        "()J",
        native_fis_length0,
        NativeKind::Bridge,
    );
    registry.register(
        "java/io/FileInputStream",
        "position0",
        "()J",
        native_fis_position0,
        NativeKind::Bridge,
    );
    // Previously hardcoded "not a regular file" so that `readAllBytes()`
    // avoided the `length0()`-sized fast path, which the stub above would
    // have sized at zero. With `length0`/`position0` real, this can report
    // the truth: `file_size` only succeeds for fd-table entries that really
    // are files (sockets/pipes/stdin fail), which is precisely the predicate.
    registry.register(
        "java/io/FileInputStream",
        "isRegularFile0",
        "(Ljava/io/FileDescriptor;)Z",
        native_fis_is_regular_file0,
        NativeKind::Bridge,
    );

    // FileOutputStream: open0, write(I,Z), writeBytes
    // Same as `FileInputStream.initIDs` above — jfieldID caching only, which
    // CratonVM's by-name field resolution does not need.
    registry.register(
        "java/io/FileOutputStream",
        "initIDs",
        "()V",
        native_noop,
        NativeKind::Bridge,
    );
    registry.register(
        "java/io/FileOutputStream",
        "open0",
        "(Ljava/lang/String;Z)V",
        native_fos_init_string_append,
        NativeKind::Bridge,
    );
    registry.register(
        "java/io/FileOutputStream",
        "write",
        "(IZ)V",
        native_fos_write_byte_ignore_append,
        NativeKind::Bridge,
    );
    registry.register(
        "java/io/FileOutputStream",
        "writeBytes",
        "([BIIZ)V",
        native_fos_write_bytes_ignore_append,
        NativeKind::Bridge,
    );

    // FileDescriptor.close0() — the real-JDK `FileInputStream.close()` /
    // `FileOutputStream.close()` bytecode routes through
    // `FileDescriptor.closeAll` -> `FileDescriptor.close()` -> `close0()`.
    // Releases the OS fd stashed on the descriptor's own `fd`/`handle`.
    registry.register(
        "java/io/FileDescriptor",
        "close0",
        "()V",
        native_fd_close0,
        NativeKind::Bridge,
    );

    // `sun/nio/ch/UnixDispatcher.close0(Ljava/io/FileDescriptor;)V` IS NOT
    // REGISTERED HERE, and the deleted registration that used to sit at this
    // point must not come back. It bound `native_fd_close0` — the body
    // immediately above, which closes a `ctx.fd_table()` entry — to a triple
    // whose receivers are NIO SOCKET descriptors, which this crate does not
    // keep in the fd table at all. `net.rs` keeps them in `net_sockets()`, and
    // `next_net_fd()` (net.rs, ~line 827) starts its counter at `0x4000_0000`
    // with the stated reason "so there's no collision with the
    // `FileDescriptorTable` counter". The two id spaces are therefore DISJOINT
    // BY CONSTRUCTION: had this registration ever won, `FdTable::flush`/`close`
    // would have missed on every socket id, the OS socket would have stayed
    // open, and the only visible effect would have been `fd`/`handle` set to
    // `-1` — a leak that reports success.
    //
    // It never won. `net.rs`'s `register_sun_nio_ch_net` registers the same
    // triple with `net_close`, and it runs LATER in the same boot:
    // `register_io_natives` reaches `nio_native::register_t16_channel_overrides`
    // hundreds of lines below this point, and that function's last statement is
    // `crate::net::register_sun_nio_ch_net(r)` — the only call site of it in
    // the workspace. `register()` is last-write-wins, so `net_close` has always
    // owned the slot. MEASURED before this comment was written: see the
    // `--dump-native-registry` note earlier in this file (2026-08-17,
    // `--real-jdk`), which reports the deleted registration with
    // `owns_slot: false` and names `net.rs`'s as the winner.
    //
    // `net_close` is also the right winner on the merits, not merely the
    // surviving one: it closes the `net_sockets()` entry the fd id actually
    // names, and it is the sibling of the `preClose0` registration three lines
    // below it in `net.rs`, which closes the SAME registry. A "fallback if
    // `register_sun_nio_ch_net` is ever gated" — the reason the previous
    // comment gave for leaving the dead line in place — would have been a
    // fallback that leaks the socket, so there is nothing here worth keeping.
    // Grep `"sun/nio/ch/UnixDispatcher"` in `net.rs` for the live rows rather
    // than trusting a line number.
    //
    // What this does NOT answer, and a source read cannot: whether `net_close`
    // actually serves the `java.net.MulticastSocket.close()` path the deleted
    // registration was originally written for (H5-1 §10.N3). That still needs
    // a run. H8-1, 2026-08-20.

    /// Platform `sockaddr_in`/`sockaddr_in6` ABI facts for the
    /// `sun.nio.ch.NativeSocketAddress` probes below. Unix reads them off
    /// the `libc` crate's struct layout for the compile target; Windows
    /// (no `libc` dependency — unconditional `libc::` here broke
    /// `cargo check` on Windows with 12 E0433s) uses the fixed Winsock
    /// ws2def.h/ws2ipdef.h layout as literals, cross-checked against
    /// `SOCKADDR_IN`/`SOCKADDR_IN6`: family/port are u16, `sin_zero[8]`
    /// pads v4 to 16 bytes, v6 is 28 bytes with flowinfo at 4, addr at 8,
    /// scope_id at 24. AF_INET6 genuinely differs per platform (23 on
    /// Windows, 10 on Linux) — exactly why these must not be hardcoded
    /// from one platform's headers.
    mod sockaddr_abi {
        #[cfg(unix)]
        pub const AF_INET: i32 = libc::AF_INET;
        #[cfg(unix)]
        pub const AF_INET6: i32 = libc::AF_INET6;
        #[cfg(unix)]
        pub const SIZEOF_SOCKADDR4: i32 = std::mem::size_of::<libc::sockaddr_in>() as i32;
        #[cfg(unix)]
        pub const SIZEOF_SOCKADDR6: i32 = std::mem::size_of::<libc::sockaddr_in6>() as i32;
        #[cfg(unix)]
        pub const SIZEOF_FAMILY: i32 = std::mem::size_of::<libc::sa_family_t>() as i32;
        #[cfg(unix)]
        pub const OFFSET_FAMILY: i32 = std::mem::offset_of!(libc::sockaddr_in, sin_family) as i32;
        #[cfg(unix)]
        pub const OFFSET_SIN4_PORT: i32 = std::mem::offset_of!(libc::sockaddr_in, sin_port) as i32;
        #[cfg(unix)]
        pub const OFFSET_SIN4_ADDR: i32 = std::mem::offset_of!(libc::sockaddr_in, sin_addr) as i32;
        #[cfg(unix)]
        pub const OFFSET_SIN6_PORT: i32 =
            std::mem::offset_of!(libc::sockaddr_in6, sin6_port) as i32;
        #[cfg(unix)]
        pub const OFFSET_SIN6_ADDR: i32 =
            std::mem::offset_of!(libc::sockaddr_in6, sin6_addr) as i32;
        #[cfg(unix)]
        pub const OFFSET_SIN6_SCOPE_ID: i32 =
            std::mem::offset_of!(libc::sockaddr_in6, sin6_scope_id) as i32;
        #[cfg(unix)]
        pub const OFFSET_SIN6_FLOWINFO: i32 =
            std::mem::offset_of!(libc::sockaddr_in6, sin6_flowinfo) as i32;

        #[cfg(windows)]
        pub const AF_INET: i32 = 2;
        #[cfg(windows)]
        pub const AF_INET6: i32 = 23;
        #[cfg(windows)]
        pub const SIZEOF_SOCKADDR4: i32 = 16;
        #[cfg(windows)]
        pub const SIZEOF_SOCKADDR6: i32 = 28;
        #[cfg(windows)]
        pub const SIZEOF_FAMILY: i32 = 2;
        #[cfg(windows)]
        pub const OFFSET_FAMILY: i32 = 0;
        #[cfg(windows)]
        pub const OFFSET_SIN4_PORT: i32 = 2;
        #[cfg(windows)]
        pub const OFFSET_SIN4_ADDR: i32 = 4;
        #[cfg(windows)]
        pub const OFFSET_SIN6_PORT: i32 = 2;
        #[cfg(windows)]
        pub const OFFSET_SIN6_ADDR: i32 = 8;
        #[cfg(windows)]
        pub const OFFSET_SIN6_SCOPE_ID: i32 = 24;
        #[cfg(windows)]
        pub const OFFSET_SIN6_FLOWINFO: i32 = 4;
    }

    // platform ABI values, not runtime-computed state. On unix they are
    // read straight off Rust's own `libc` layout (compiled for the same
    // target CratonVM runs on); on Windows — where the `libc` crate is not
    // a dependency and the original unconditional `libc::` references broke
    // `cargo check` outright — the equivalent Winsock `SOCKADDR_IN`/
    // `SOCKADDR_IN6` values are fixed by the ws2def.h/ws2ipdef.h ABI and
    // are provided as literals in `sockaddr_abi` below (note AF_INET6 is
    // 23 on Windows vs 10 on Linux).
    //
    // KEEP (all twelve `NativeSocketAddress` accessors below): each returns a
    // compile-time platform ABI constant, which is exactly what the real JNI
    // implementations do (`offsetof`/`sizeof` on `struct sockaddr_in*`). These
    // are not stubbed values standing in for runtime state.
    use sockaddr_abi as sa;
    registry.register(
        "sun/nio/ch/NativeSocketAddress",
        "AFINET",
        "()I",
        |_ctx, _args| Ok(Some(Value::Int(sa::AF_INET))),
        NativeKind::Bridge,
    );
    registry.register(
        "sun/nio/ch/NativeSocketAddress",
        "AFINET6",
        "()I",
        |_ctx, _args| Ok(Some(Value::Int(sa::AF_INET6))),
        NativeKind::Bridge,
    );
    registry.register(
        "sun/nio/ch/NativeSocketAddress",
        "sizeofSockAddr4",
        "()I",
        |_ctx, _args| Ok(Some(Value::Int(sa::SIZEOF_SOCKADDR4))),
        NativeKind::Bridge,
    );
    registry.register(
        "sun/nio/ch/NativeSocketAddress",
        "sizeofSockAddr6",
        "()I",
        |_ctx, _args| Ok(Some(Value::Int(sa::SIZEOF_SOCKADDR6))),
        NativeKind::Bridge,
    );
    registry.register(
        "sun/nio/ch/NativeSocketAddress",
        "sizeofFamily",
        "()I",
        |_ctx, _args| Ok(Some(Value::Int(sa::SIZEOF_FAMILY))),
        NativeKind::Bridge,
    );
    registry.register(
        "sun/nio/ch/NativeSocketAddress",
        "offsetFamily",
        "()I",
        |_ctx, _args| Ok(Some(Value::Int(sa::OFFSET_FAMILY))),
        NativeKind::Bridge,
    );
    registry.register(
        "sun/nio/ch/NativeSocketAddress",
        "offsetSin4Port",
        "()I",
        |_ctx, _args| Ok(Some(Value::Int(sa::OFFSET_SIN4_PORT))),
        NativeKind::Bridge,
    );
    registry.register(
        "sun/nio/ch/NativeSocketAddress",
        "offsetSin4Addr",
        "()I",
        |_ctx, _args| Ok(Some(Value::Int(sa::OFFSET_SIN4_ADDR))),
        NativeKind::Bridge,
    );
    registry.register(
        "sun/nio/ch/NativeSocketAddress",
        "offsetSin6Port",
        "()I",
        |_ctx, _args| Ok(Some(Value::Int(sa::OFFSET_SIN6_PORT))),
        NativeKind::Bridge,
    );
    registry.register(
        "sun/nio/ch/NativeSocketAddress",
        "offsetSin6Addr",
        "()I",
        |_ctx, _args| Ok(Some(Value::Int(sa::OFFSET_SIN6_ADDR))),
        NativeKind::Bridge,
    );
    registry.register(
        "sun/nio/ch/NativeSocketAddress",
        "offsetSin6ScopeId",
        "()I",
        |_ctx, _args| Ok(Some(Value::Int(sa::OFFSET_SIN6_SCOPE_ID))),
        NativeKind::Bridge,
    );
    registry.register(
        "sun/nio/ch/NativeSocketAddress",
        "offsetSin6FlowInfo",
        "()I",
        |_ctx, _args| Ok(Some(Value::Int(sa::OFFSET_SIN6_FLOWINFO))),
        NativeKind::Bridge,
    );

    // P69-Cleaner-realfix: the `FileCleanable.register` no-op was removed.
    // It existed because the synthetic `java.lang.ref.Cleaner` left a
    // bogus `impl` field, so the real `PhantomCleanable.<init>` ->
    // `CleanerImpl.getCleanerImpl` `checkcast` threw ClassCastException.
    // The real `Cleaner.create()` bytecode now runs (Thread `holder` is
    // populated), producing a genuine `CleanerImpl`, so the real-JDK
    // `FileCleanable.register` -> phantom-cleanable GC-time fd cleanup
    // path works unmodified.

    // --- java.io.FileWriter ---
    // REAL-BY-DEFAULT (2026-06-18): the synthetic byte-fd FileWriter shim below
    // silently DROPS all character data and is gated off by default. See
    // `real_filewriter_enabled` and
    // docs/feature-designs/native-dispatch-architecture.md.
    if !real_filewriter_enabled() {
        // FileWriter wraps FileOutputStream; we use the same fd-in-field-0 layout.
        registry.register(
            "java/io/FileWriter",
            "<init>",
            "(Ljava/lang/String;)V",
            native_fos_init_string, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/FileWriter",
            "<init>",
            "(Ljava/lang/String;Z)V",
            native_fos_init_string_append, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/FileWriter",
            "<init>",
            "(Ljava/io/File;)V",
            native_fos_init_file, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/FileWriter",
            "<init>",
            "(Ljava/io/File;Z)V",
            native_fos_init_file_append, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register("java/io/FileWriter", "write", "(I)V", native_fos_write_byte, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            "java/io/FileWriter",
            "write",
            "([BII)V",
            native_fos_write_bytes, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/FileWriter",
            "write",
            "([B)V",
            native_fos_write_byte_array, cratonvm_native_api::NativeKind::Bridge,
        );
        // write(String) for FileWriter — write UTF-8 bytes
        registry.register(
            "java/io/FileWriter",
            "write",
            "(Ljava/lang/String;)V",
            |ctx, args| {
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(None),
                };
                let text = match args.get(1) {
                    Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                    _ => String::new(),
                };
                let fd = match ctx.get_field(this, 0) {
                    Value::Int(fd) => fd as u32,
                    _ => return Ok(None),
                };
                // `let _ =` here made `FileWriter.write(String)` the only
                // write in this crate that could not fail: "@throws IOException
                // If an I/O error occurs" (`Writer.write(String)`), and a
                // caller that got no exception has been told the characters
                // are in the file.
                ctx.fd_table().write_string(fd, &text).map_err(io_err)?;
                Ok(None)
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        // write(String, int, int) — substring write
        registry.register(
            "java/io/FileWriter",
            "write",
            "(Ljava/lang/String;II)V",
            |ctx, args| {
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(None),
                };
                let text = match args.get(1) {
                    Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                    _ => String::new(),
                };
                let off = match args.get(2) {
                    Some(Value::Int(v)) => *v,
                    _ => 0,
                };
                let len = match args.get(3) {
                    Some(Value::Int(v)) => *v,
                    _ => text.encode_utf16().count() as i32,
                };
                // `FileWriter` extends `OutputStreamWriter` and does not
                // redeclare this method, so `Writer`'s unweakened bounds
                // contract applies — see `writer_string_region`. Clamping BOTH
                // ends made `fw.write(s, 0, s.length() + 1)` write the whole
                // string and return normally.
                let sub = writer_string_region(&text, off, len)?;
                let fd = match ctx.get_field(this, 0) {
                    Value::Int(fd) => fd as u32,
                    _ => return Ok(None),
                };
                ctx.fd_table().write_string(fd, &sub).map_err(io_err)?;
                Ok(None)
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register("java/io/FileWriter", "flush", "()V", native_fos_flush, cratonvm_native_api::NativeKind::Bridge);
        registry.register("java/io/FileWriter", "close", "()V", native_fos_close, cratonvm_native_api::NativeKind::Bridge);
    } // end if !real_filewriter_enabled() — synthetic byte-fd FileWriter shim

    // RDR-MIGRATION 2026-06-01: the InputStreamReader natives below used to be
    // registered UNCONDITIONALLY (even in real-JDK mode). They were a
    // "StreamDecoder-bypass": store the underlying InputStream on <init> and
    // decode bytes directly in `native_isr_read*`. That shadowed the real JDK
    // InputStreamReader bytecode and, crucially, never set up the real
    // InputStreamReader `sd` (StreamDecoder) field — so a real BufferedReader
    // wrapping it could `read()`/`read(char[])` but `readLine()` (which uses
    // the real `in`/`cb`/`fill` machinery) misbehaved.
    //
    // We now run the REAL InputStreamReader bytecode, which builds a real
    // `sun.nio.cs.StreamDecoder` via `StreamDecoder.forInputStreamReader(...)`.
    // That factory + the decoder's read/ready/close are provided by the
    // `stream_decoder` native shim (registered above via
    // `register_stream_decoder_natives`), which drives the underlying stream
    // through `in.read([BII)I` virtually and decodes with the real charset
    // engine (full UTF-8/UTF-16/Latin-1 — no longer ASCII-only). This makes
    // FileReader → InputStreamReader → StreamDecoder → FileInputStream a
    // fully real-bytecode path, analogous to the FileInputStream open0/read0
    // surface. The old synthetic ISR natives are kept only under
    // `synthetic-jdk`.
    //
    // 2026-08-12 (W7-50): the cfg alone was the WRONG GUARD here, for exactly
    // the reason spelled out on the `FileOutputStream` block above — it asks
    // what was COMPILED, and what decides whether a real `InputStreamReader`
    // is on the other end is which CLASS LIBRARY was LOADED. A
    // `--features synthetic-jdk` binary run `--real-jdk` (or `--real-jdk`)
    // satisfied the cfg and registered these over the real class, where slot 0
    // is `Reader.lock` and slot 1 is `Reader.skipBuffer`. `native_isr_init`
    // then clobbers the monitor object, writes an `InputStream` into a
    // `char[]` slot, and never creates the `sd` StreamDecoder.
    #[cfg(feature = "synthetic-jdk")]
    if !registry.drops_real_layout_synthetic() {
        registry.register(
            "java/io/InputStreamReader",
            "<init>",
            "(Ljava/io/InputStream;)V",
            native_isr_init, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/InputStreamReader",
            "<init>",
            "(Ljava/io/InputStream;Ljava/lang/String;)V",
            native_isr_init_charset, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/InputStreamReader",
            "<init>",
            "(Ljava/io/InputStream;Ljava/nio/charset/Charset;)V",
            native_isr_init_charset, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/InputStreamReader",
            "<init>",
            "(Ljava/io/InputStream;Ljava/nio/charset/CharsetDecoder;)V",
            native_isr_init_charset, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register("java/io/InputStreamReader", "read", "()I", native_isr_read, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            "java/io/InputStreamReader",
            "read",
            "([CII)I",
            native_isr_read_chars, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/InputStreamReader",
            "close",
            "()V",
            native_isr_close, cratonvm_native_api::NativeKind::Bridge,
        );

        // RA.3: Reader.read(CharBuffer) — real-JDK path.  Registered on
        // the base `java/io/Reader` so every subclass inherits, and
        // additionally on `InputStreamReader` so our own
        // `read([CII)I` override is driven directly (invoke_virtual
        // re-dispatches to the registered native on the receiver's
        // concrete class).
        registry.register(
            "java/io/Reader",
            "read",
            "(Ljava/nio/CharBuffer;)I",
            native_reader_read_charbuffer, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/InputStreamReader",
            "read",
            "(Ljava/nio/CharBuffer;)I",
            native_reader_read_charbuffer, cratonvm_native_api::NativeKind::Bridge,
        );
    } // end #[cfg(feature = "synthetic-jdk")] synthetic InputStreamReader natives

    // Subsequent synthetic-only Reader/Writer overrides assume our
    // synthetic 1-3-field layouts (fd at slot 0) and corrupt state
    // when invoked on real JDK instances (BufferedReader: in + cb +
    // nChars + nextChar + ...).  Keep them gated.
    //
    // 2026-08-12 (W7-50): "gated" meant the cfg alone, which is the wrong
    // guard (see the `FileOutputStream` and `InputStreamReader` notes above).
    // Measured on a `--features synthetic-jdk` binary under `--real-jdk`,
    // where every class here is a REAL JDK class:
    //
    //  * `native_br_read_line` reads slot 0 expecting a `Value::Int` fd. On a
    //    real `BufferedReader` slot 0 is `in`, a reference, so the match falls
    //    to `_` and `readLine()` returns Java `null` on the FIRST call, with
    //    no I/O attempted and nothing thrown. That is `RJdkNet`'s
    //    "echo reply: null" and `RJdkServices`' "discovered providers: []" --
    //    the latter because real `ServiceLoader$LazyClassPathLookupIterator`
    //    parses its `META-INF/services` descriptor through exactly this
    //    `readLine`, so it collects zero provider names and the for-each body
    //    never runs.
    //  * `native_osw_init` / `native_bw_init` park a value on slot 0 and never
    //    build the `se` StreamEncoder or set `out`. JDK 25's `PrintStream`
    //    ctor builds `charOut = new OutputStreamWriter(this, charset)` and
    //    `textOut = new BufferedWriter(charOut)`, so `print(char[])` reaches
    //    `BufferedWriter.ensureOpen()` with a null `out`, which throws
    //    `IOException("Stream closed")`; `PrintStream`'s own exception table
    //    catches it and sets `trouble = true`. That is `RJdkHello`'s
    //    "PrintStream reported an error" -- `checkError()` is not registered
    //    anywhere, so it faithfully reported a flag we caused.
    //
    // The runtime flag is the correct guard and is already set in exactly the
    // arms that matter. The cfg stays too: a default build should not compile
    // these in at all.
    #[cfg(feature = "synthetic-jdk")]
    if !registry.drops_real_layout_synthetic() {
        // --- java.io.BufferedReader ---
        registry.register(
            "java/io/BufferedReader",
            "<init>",
            "(Ljava/io/Reader;)V",
            native_br_init, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/BufferedReader",
            "readLine",
            "()Ljava/lang/String;",
            native_br_read_line, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register("java/io/BufferedReader", "read", "()I", native_br_read, cratonvm_native_api::NativeKind::Bridge);
        registry.register("java/io/BufferedReader", "ready", "()Z", native_br_ready, cratonvm_native_api::NativeKind::Bridge);
        registry.register("java/io/BufferedReader", "close", "()V", native_br_close, cratonvm_native_api::NativeKind::Bridge);

        // --- java.io.OutputStreamWriter ---
        registry.register(
            "java/io/OutputStreamWriter",
            "<init>",
            "(Ljava/io/OutputStream;)V",
            native_osw_init, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/OutputStreamWriter",
            "<init>",
            "(Ljava/io/OutputStream;Ljava/lang/String;)V",
            native_osw_init_charset, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/OutputStreamWriter",
            "write",
            "(Ljava/lang/String;II)V",
            native_osw_write, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/OutputStreamWriter",
            "flush",
            "()V",
            native_osw_flush, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/OutputStreamWriter",
            "close",
            "()V",
            native_osw_close, cratonvm_native_api::NativeKind::Bridge,
        );

        // --- java.io.BufferedWriter ---
        registry.register(
            "java/io/BufferedWriter",
            "<init>",
            "(Ljava/io/Writer;)V",
            native_bw_init, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/BufferedWriter",
            "write",
            "(Ljava/lang/String;II)V",
            native_bw_write_string, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/BufferedWriter",
            "write",
            "(I)V",
            native_bw_write_int, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/BufferedWriter",
            "newLine",
            "()V",
            native_bw_new_line, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register("java/io/BufferedWriter", "flush", "()V", native_bw_flush, cratonvm_native_api::NativeKind::Bridge);
        registry.register("java/io/BufferedWriter", "close", "()V", native_bw_close, cratonvm_native_api::NativeKind::Bridge);
    } // end synthetic-jdk InputStreamReader/BufferedReader/OutputStreamWriter/BufferedWriter block

    // --- java.io.ByteArrayInputStream ---
    let bais = "java/io/ByteArrayInputStream";
    registry.register(bais, "<init>", "([B)V", native_bais_init, cratonvm_native_api::NativeKind::Bridge);
    registry.register(bais, "<init>", "([BII)V", native_bais_init_offset, cratonvm_native_api::NativeKind::Bridge);
    registry.register(bais, "read", "()I", native_bais_read, cratonvm_native_api::NativeKind::Bridge);
    registry.register(bais, "read", "([B)I", native_bais_read_byte_array, cratonvm_native_api::NativeKind::Bridge);
    registry.register(bais, "read", "([BII)I", native_bais_read_bytes, cratonvm_native_api::NativeKind::Bridge);
    registry.register(bais, "available", "()I", native_bais_available, cratonvm_native_api::NativeKind::Bridge);
    registry.register(bais, "skip", "(J)J", native_bais_skip, cratonvm_native_api::NativeKind::Bridge);
    registry.register(bais, "reset", "()V", native_bais_reset, cratonvm_native_api::NativeKind::Bridge);
    registry.register(bais, "close", "()V", native_bais_close, cratonvm_native_api::NativeKind::Bridge);

    // --- java.io.ByteArrayOutputStream ---
    let baos = "java/io/ByteArrayOutputStream";
    registry.register(baos, "<init>", "()V", native_baos_init, cratonvm_native_api::NativeKind::Bridge);
    registry.register(baos, "<init>", "(I)V", native_baos_init_capacity, cratonvm_native_api::NativeKind::Bridge);
    registry.register(baos, "write", "(I)V", native_baos_write, cratonvm_native_api::NativeKind::Bridge);
    registry.register(baos, "write", "([B)V", native_baos_write_byte_array, cratonvm_native_api::NativeKind::Bridge);
    registry.register(baos, "write", "([BII)V", native_baos_write_bytes, cratonvm_native_api::NativeKind::Bridge);
    registry.register(baos, "toByteArray", "()[B", native_baos_to_byte_array, cratonvm_native_api::NativeKind::Bridge);
    registry.register(baos, "size", "()I", native_baos_size, cratonvm_native_api::NativeKind::Bridge);
    registry.register(baos, "reset", "()V", native_baos_reset, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        baos,
        "toString",
        "()Ljava/lang/String;",
        native_baos_to_string, cratonvm_native_api::NativeKind::Bridge,
    );
    // The NAME overload gets its own body: it is the only one of the two that
    // can be handed a charset that does not exist, and it owes an
    // `UnsupportedEncodingException` when it is. See
    // `native_baos_to_string_charset_name`.
    registry.register(
        baos,
        "toString",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_baos_to_string_charset_name, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        baos,
        "toString",
        "(Ljava/nio/charset/Charset;)Ljava/lang/String;",
        native_baos_to_string_charset, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(baos, "close", "()V", native_baos_close, cratonvm_native_api::NativeKind::Bridge);
    registry.register(baos, "flush", "()V", native_baos_flush, cratonvm_native_api::NativeKind::Bridge);

    // --- java.io.InputStream (base class fallback) ---
    //
    // ELEVEN ROWS RETIRED FROM THIS BLOCK AND THE `OutputStream` ONE BELOW,
    // 2026-08-22 (WORKER 4). What is left is the two rows that are NOT section
    // 1.4 shadows, because the method they answer is ABSTRACT in `java.base` and
    // has no bytecode to shadow: `InputStream.read()I` and
    // `OutputStream.write(I)V`.
    //
    // # What was here, and why it is not any more
    //
    // `read([B)I`, `read([BII)I`, `available()I`, `close()V`, `skip(J)J`,
    // `readAllBytes()[B`, `readNBytes(I)[B` and `readNBytes([BII)I` on
    // `java/io/InputStream`; `write([BII)V`, `flush()V` and `close()V` on
    // `java/io/OutputStream`. Every one of them stood in front of a real
    // `java.base` body, and the bodies here were transcriptions of those bodies
    // -- `native_bais_read_bytes`, for a receiver that is not one of ours, ran
    // `invoke_virtual(this, "read", "()I")` in a loop, which is
    // `InputStream.read(byte[],int,int)`'s JDK default written out in Rust.
    //
    // # The population they existed for no longer exists
    //
    // The comment that used to sit on `native_bais_read_bytes` named it:
    // *"synthetic streams (URL.openStream, getResourceAsStream) that materialise
    // as bare InputStream-typed receivers but actually have the
    // ByteArrayInputStream layout in slots 0..3"*. A BARE `java.io.InputStream`
    // receiver is a JVMS 6.5 defect in its own right -- the class is abstract --
    // and `regression-suite/probes/W4StreamCarrier.java` (added for this) asks
    // whether any survives. MEASURED, 15 carriers, BOTH modes:
    //
    //     abstractOrInterface = 0
    //     URL.openStream()                  -> java.io.ByteArrayInputStream
    //     URLConnection.getInputStream()    -> java.io.ByteArrayInputStream
    //     Class.getResourceAsStream()       -> java.io.ByteArrayInputStream
    //     ClassLoader.getResourceAsStream() -> java.io.ByteArrayInputStream
    //
    // Every one of them is a CONCRETE `ByteArrayInputStream`, which has its own
    // exact-class registrations twenty lines above and reaches them first. The
    // fallback was serving a shape the VM stopped producing.
    //
    // # And the shape it DOES still serve is measured identical without it
    //
    // `regression-suite/probes/W4BaseStream.java` drives the other population --
    // an application subclass that declares only the abstract primitive and
    // inherits the rest, which reaches these rows through the superclass walk
    // (`H11-1`, `H11-3` N3). MEASURED, 26 cases, ZERO diffs against HotSpot
    // 25.0.4+7 in both modes, and `--dump-native-registry` on the same run shows
    // thirteen of the fourteen rows taking `invocations: 0` while
    // `read([BII)I` takes 1 -- the positive control that makes the zeros
    // informative rather than merely absent (`[zero@consumer]`).
    //
    // # Trap 4, per row rather than assumed
    //
    // `--dump-native-registry` unioned over 105 per-vector strict-mode boots:
    // `dupX = 0` for all eleven. The deletions remove eleven rows and promote
    // nobody. Two rows in these families were DELIBERATELY LEFT, and both have
    // a rival underneath:
    //
    //   * `java/io/OutputStream.write([B)V` -- `native-builtins/src/lib.rs`
    //     owns it, `invocations = 10`. Not this crate's to retire.
    //   * `java/io/InputStream.transferTo` -- `phases_late/zip_streams.rs`
    //     registered the same triple then; it no longer does (it registers on
    //     `ByteArrayInputStream` / `FileInputStream` only), so this copy is
    //     the only root-class row now. See the note on its registration below.
    //
    // `java/io/FilterOutputStream.close()V` also stays: `dupX = 1`, and the
    // comment on it records the kafka gzip truncation it was added for.
    registry.register("java/io/InputStream", "read", "()I", native_bais_read, cratonvm_native_api::NativeKind::Bridge);
    // `SyntheticStub`, stated. `java.io.InputStream.transferTo` is ordinary
    // bytecode in `java.base` — a read/write loop — so contract §1.5 cannot call
    // this a bridge, and `phases_late/zip_streams.rs` registers the same triple
    // as a stub. Left on the ambient `Bridge` this copy was the one `--real-jdk`
    // kept, because the stub copy is refused at the door: the fake outlived the
    // mode built to remove it, decided by which of two files was tagged what.
    //
    // Round 15 wave 4 (lane walk3): kept, not retired, and made the JDK loop
    // (`native_is_transfer_to`, `CRATONVM_COMPAT_TRANSFER_TO_EXACT`). Retiring
    // it is the better end state -- `--compatible` would run the JDK bytecode
    // as `--real-jdk` does -- but two `vm` unit tests call this row by name
    // (`input_stream_transfer_to_p72`, and the `ProcessPipeInputStream`
    // inherited-dispatch test), and whether the `synthetic-jdk` arm has
    // `InputStream` bytecode to fall back to is unverified; see W3-2 of
    // `docs/feature-designs/jit-ir-optimizations.md`.
    registry.register(
        "java/io/InputStream",
        "transferTo",
        "(Ljava/io/OutputStream;)J",
        native_is_transfer_to,
        cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        "java/io/InputStream",
        "readAllBytes",
        "()[B",
        native_is_read_all_bytes,
        cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // --- java.io.OutputStream (base class fallback) ---
    // `write([BII)V`, `flush()V` and `close()V` retired here -- see the note on
    // the `InputStream` block above, which covers both families. `write(I)V`
    // stays because `java.io.OutputStream.write(int)` is ABSTRACT: there is no
    // bytecode behind it, so it is a stand-in and not a shadow.
    registry.register("java/io/OutputStream", "write", "(I)V", native_baos_write, cratonvm_native_api::NativeKind::Bridge);
    // FilterOutputStream.close() MUST flush and then close the wrapped stream
    // (`out`, slot 0). Without this, a `DataOutputStream`/`BufferedOutputStream`
    // wrapping e.g. a `GZIPOutputStream` resolved its inherited `close()` to the
    // base `OutputStream.close` no-op above — so closing the DataOutputStream
    // never reached `GZIPOutputStream.finish()`, and only the 10-byte gzip header
    // was emitted (the deflated body + trailer were dropped). That truncated every
    // kafka compressed record batch built through `MemoryRecordsBuilder`'s
    // `DataOutputStream(compressionStream)` → "Unexpected end of ZLIB input stream"
    // on read-back (bug-15). `closed` is at slot 1 (idempotency).
    registry.register(
        "java/io/FilterOutputStream",
        "close",
        "()V",
        native_filteros_close, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- java.util.Scanner ---
    register_scanner_natives(registry);

    // --- java.nio (Phase 23) ---
    // T14/T15: The synthetic NIO overrides here assume a specific 5-field
    // ByteBuffer layout (buf/pos/limit/capacity/mark).  In real-JDK mode
    // the JDK's ByteBuffer has a different layout, so calling these
    // overrides on a real instance panics with a layout mismatch.  Gate
    // them behind the synthetic-jdk feature so real-JDK mode uses the
    // JDK's own bytecode implementations.
    //
    // 2026-08-12 (W7-50): the comment says "real-JDK mode", the guard said
    // "real-JDK build" -- the same wrong guard as the `FileOutputStream` and
    // Reader/Writer blocks above. A `--features synthetic-jdk` binary run
    // `--real-jdk` registered this whole ~60-method surface over the real
    // `java.nio.ByteBuffer`, and because it runs AFTER the order-aware `s2`
    // family from `native-builtins`' `register_s2_bytebuffer_essentials` and
    // registration is last-write-wins, it WON. Two measured consequences:
    //
    //  * `native_bb_put_int`/`get_int`/`put_short`/... are hard-coded
    //    `to_be_bytes`/`from_be_bytes` and never read `bigEndian`, while
    //    `order(ByteOrder)` is not re-registered here at all. So the order
    //    write lands, `order()` reports LITTLE_ENDIAN, and `putInt` writes
    //    big-endian anyway. That asymmetry is `RJdkNio`'s "little-endian
    //    layout" (big-endian passed, which is what made it look narrow).
    //  * `native_bb_remaining` goes through `bb_state`, which resolves the
    //    backing array as `hb`-by-name, then slot 5, then slot 0 -- and has
    //    no direct-buffer arm. On the real `DirectByteBuffer` that
    //    `ByteBuffer.allocateDirect` returns, slot 0 is `Buffer.mark`, whose
    //    initial value is -1. That is `RJdkDefineClass`'s "ByteBuffer missing
    //    backing array (field 0 returned Int(-1))", raised from the
    //    `int len = b.remaining()` that opens real
    //    `ClassLoader.defineClass(String,ByteBuffer,ProtectionDomain)`.
    #[cfg(feature = "synthetic-jdk")]
    if !registry.drops_real_layout_synthetic() {
        register_nio_natives(registry);
    }

    // --- Phase 26: Extended I/O ---
    register_string_rw_natives(registry);
    register_data_stream_natives(registry);

    // --- Phase 32: java.nio.file ---
    register_nio_file_natives(registry);

    // --- Phase 36: I/O extras ---
    register_io_extras_natives(registry);

    // --- Real-JDK RandomAccessFile natives (open0/read0/readBytes0/... ) ---
    // Registered unconditionally; in synthetic mode the <init>/read/write
    // overrides in register_io_extras_natives intercept the public Java
    // methods first, so the open0-family is only hit via real-JDK bytecode.
    random_access_file::register_random_access_file_natives(registry);

    // --- Phase 40: Buffered I/O + Piped streams ---
    register_buffered_stream_natives(registry);

    // --- Phase 45: NIO channel extras (FileLock, MappedByteBuffer, FileChannel additions, Files.walk/list) ---
    register_nio_channel_extras(registry);

    // --- Phase 92: Networking & I/O Completeness ---
    register_phase92_io_completeness(registry);

    // --- T16.5 / T16.6: Async channels, DatagramChannel, MulticastSocket,
    //     logging extras. Registered LAST so these overrides win over the
    //     phase-72 (`phases_late`) and phase-92 registrations for signatures
    //     we implement (see `nio_native::register_t16_channel_overrides`). ---
    nio_native::register_t16_channel_overrides(registry);

    // T16 may install broad DatagramChannel compatibility callbacks. Reapply
    // the fd-table-backed Phase 92 channel surface after it so real-JDK DNS
    // clients observe the bound channel's actual local-address state.
    register_datagram_channel(registry);
    }

// ===========================================================================
// InputStream base-class helpers (Java 9+): transferTo, readAllBytes, readNBytes
// These delegate to read() or read([B,I,I) via invoke_virtual so they work with
// any concrete InputStream subtype registered in the native registry.
// ===========================================================================

// `native_is_skip`, `native_is_read_n_bytes` and `native_is_read_n_bytes_buf`
// were DELETED 2026-08-22 (WORKER 4) with the `java/io/InputStream` rows that
// were their only call sites -- see the retirement note on that registration
// block. Each was a Rust transcription of the corresponding `java.base` default
// body; `git log -S native_is_read_n_bytes` has them.
//
// `native_is_read_all_bytes` SURVIVES and is deliberately not deleted with
// them. (This note used to say `process.rs` registers it on its own class; it
// does not any more -- gc-common w9-d found its only call site to be the
// `InputStream.readAllBytes` `SyntheticStub` registration above.)

/// InputStream.readAllBytes() → byte[] (Java 9+)
///
/// Was a one-`invoke_virtual`-per-byte loop through the single-byte
/// `read()I` overload — correct but catastrophically slow for any stream
/// with more than a few KB of content (each byte pays a full virtual
/// dispatch). Found root-causing a real-world case: `JarInputStream
/// .checkManifest()`'s `readAllBytes()` on a signed jar with a ~769KB
/// `MANIFEST.MF` (bcprov-jdk18on) took 18-70+ seconds (and, under shared-host
/// CPU contention, blew past a 300s test-suite timeout, surfacing as an
/// apparent hang even though the thread was making genuine — just glacial —
/// progress; confirmed via `--stack-dump-on-timeout`, which sampled the
/// identical `read()I` pc across dumps because the loop resets to the same
/// bytecode entry point on every one of the ~769,000 single-byte calls).
/// Rewritten to bulk-read via the virtual `read([BII)I` overload (16KB
/// chunks, same pattern as `native_is_transfer_to` below) — real
/// `ZipInputStream`/`InflaterInputStream`/etc. already implement that
/// overload efficiently (native inflate), so this just stops bypassing it.
pub(crate) fn native_is_read_all_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            let arr = ctx.new_array(ArrayElementType::Byte, 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let cls_name = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .unwrap_or_default();
    // Same Mockito inherited-helper guard as native_bais_read_bytes.
    if cls_name.contains("$MockitoMock$") {
        let arr = ctx.new_array(ArrayElementType::Byte, 0);
        return Ok(Some(Value::Object(Some(arr))));
    }

    let this_pin = ctx.pin_native_root(this);
    const CHUNK: usize = 16 * 1024;
    let chunk_buf = ctx.new_array(ArrayElementType::Byte, CHUNK);
    let chunk_pin = ctx.pin_native_root(chunk_buf);
    let mut bytes: Vec<u8> = Vec::new();
    let mut scratch = vec![0u8; CHUNK];
    // JIT round 15 wave 4 (lane walk3 patch, applied by the orchestrator;
    // `r15w4-walk3-read-all-bytes-stub-diverges-patch`): JDK 25's
    // `readNBytes(Integer.MAX_VALUE)` -- each `read` asks `(nread,
    // 16384 - nread)` of the current buffer, and a zero-length read starts a
    // new buffer instead of ending the stream. `0` restores the old loop.
    let exact = true;
    loop {
        // One buffer's worth, as `readNBytes` fills one `byte[16384]`.
        let mut nread: usize = 0;
        let last = loop {
            let this_cur = ctx.read_native_pin(this_pin, this);
            let chunk_cur = ctx.read_native_pin(chunk_pin, chunk_buf);
            let (off, len) = if exact {
                (nread, CHUNK - nread)
            } else {
                (0, CHUNK)
            };
            let n = match ctx.invoke_virtual(
                this_cur,
                "read",
                "([BII)I",
                &[
                    Value::Object(Some(chunk_cur)),
                    // Casts: both are at most `CHUNK` (16384).
                    Value::Int(off as i32),
                    Value::Int(len as i32),
                ],
            ) {
                Ok(Some(Value::Int(n))) => n,
                Ok(_) => -1,
                Err(e) => {
                    ctx.unpin_native_roots(this_pin);
                    return Err(e);
                }
            };
            if n <= 0 {
                break n;
            }
            // Cast: `n > 0` here.
            let n = n as usize;
            if exact {
                // A stream answering more than it was asked is clamped; the JDK
                // would throw at its next `read` (a negative `len`).
                nread = (nread + n).min(CHUNK);
                if nread == CHUNK {
                    break 1;
                }
            } else {
                let chunk_cur = ctx.read_native_pin(chunk_pin, chunk_buf);
                let copied = ctx.read_byte_array_into(chunk_cur, 0, &mut scratch[..n.min(CHUNK)]);
                bytes.extend_from_slice(&scratch[..copied]);
            }
        };
        if exact && nread > 0 {
            let chunk_cur = ctx.read_native_pin(chunk_pin, chunk_buf);
            let copied = ctx.read_byte_array_into(chunk_cur, 0, &mut scratch[..nread]);
            bytes.extend_from_slice(&scratch[..copied]);
        }
        // The JDK goes round again while the last read returned >= 0.
        if last < 0 || !exact {
            break;
        }
    }
    ctx.unpin_native_roots(this_pin);
    // gc-common w9-d: the result is caller-sized (the whole stream), so a
    // refusal reclaims and retries first. Nothing is held past this point --
    // the pins above are released and the content is in `bytes` -- and this
    // native is registered-only (`InputStream.readAllBytes`).
    let arr: ObjectRef =
        new_byte_array_reclaiming(ctx, bytes.len(), "byte[] (InputStream.readAllBytes)")?;
    ctx.write_byte_array_from(arr, 0, &bytes);
    Ok(Some(Value::Object(Some(arr))))
}

/// A caller-sized `byte[]` with one reclaim-and-retry before the catchable
/// `OutOfMemoryError` (gc-common w9-d,
/// `common-w8b-native-factories-still-single-attempt`).
///
/// The twin of `native-builtins`' `new_array_reclaiming` with an empty `live`
/// set; that one is `pub(crate)` in a crate that depends on this one. **The
/// caller must hold no unpinned `ObjectRef` it uses afterwards and must be
/// reached only as a registered native** -- the reclaim is a real, moving
/// collection. HotSpot's `readAllBytes` is Java and collects before it throws;
/// the single no-GC attempt this replaces threw on a heap one collection would
/// have freed.
///
/// gc-common w12-e: the callers are the whole-stream reads (`readAllBytes`,
/// `Files.readAllBytes`), the read-outs (`ByteArrayOutputStream.toByteArray`)
/// and the caller-sized read scratch (`InputStreamReader.read(char[],int,int)`,
/// `DataInputStream.readUTF` through `dis_read_exact`). Each pins what it
/// still uses BEFORE calling this, and re-reads it through the pin after.
///
/// gc-common w13-e added the write side: `ByteArrayOutputStream` growth and
/// `ByteArrayOutputStream(int)`, `BufferedOutputStream(out, size)`, and the
/// `ByteBuffer` producers through [`alloc_byte_buffer_reclaiming`].
pub(crate) fn new_byte_array_reclaiming(
    ctx: &mut dyn NativeContext,
    length: usize,
    what: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    new_prim_array_reclaiming(ctx, ArrayElementType::Byte, length, what)
}

/// Past this length a collection cannot make the array fit: HotSpot refuses
/// it before trying the heap (`Integer.MAX_VALUE - 8` is the JDK's
/// `SOFT_MAX_ARRAY_LENGTH`). The `native-collections` ladder has the same
/// bound. Only the reclaim is skipped; the answer (an `OutOfMemoryError`) is
/// unchanged.
pub(crate) const RECLAIM_MAX_ARRAY_LENGTH: usize = (i32::MAX as usize) - 8;

/// [`new_byte_array_reclaiming`] for any primitive element type (gc-common
/// w12-e: `CharArrayWriter.toCharArray` needs a `char[]`). Same contract:
/// the caller holds nothing unpinned that it uses afterwards, and is reached
/// only as a registered native (or from one that is, with everything pinned).
pub(crate) fn new_prim_array_reclaiming(
    ctx: &mut dyn NativeContext,
    element_type: ArrayElementType,
    length: usize,
    what: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(arr) = ctx.try_new_array(element_type, length) {
        return Ok(arr);
    }
    if length <= RECLAIM_MAX_ARRAY_LENGTH && ctx.reclaim_before_alloc_retry() {
        if let Some(arr) = ctx.try_new_array(element_type, length) {
            return Ok(arr);
        }
    }
    Err(RuntimeError::OutOfMemoryError {
        message: format!("Java heap space (allocating {what})"),
    }
    .into())
}

/// The length a growing `byte[]` / `char[]` write buffer asks for when it
/// holds `cap` elements and needs `needed` (`needed > cap`): double, or
/// exactly `needed` when that is more, clamped to `SOFT_MAX_ARRAY_LENGTH` the
/// way `jdk.internal.util.ArraysSupport.newLength` clamps it
/// (gc-common w13-e, `common-w12e-native-io-write-side-growth-single-attempt`).
///
/// The clamp matters only past 1 GiB: `cap * 2` used to be asked for as is, so
/// a 1.2 G-element buffer requested 2.4 G elements, which no heap can hold and
/// which [`new_prim_array_reclaiming`] does not even reclaim for. The JDK asks
/// for `SOFT_MAX_ARRAY_LENGTH` there instead, and only a `needed` past it
/// (a genuinely impossible array) goes on to the `OutOfMemoryError`.
pub(crate) fn grown_array_capacity(cap: usize, needed: usize) -> usize {
    let preferred = cap.saturating_mul(2).max(needed);
    if preferred <= RECLAIM_MAX_ARRAY_LENGTH {
        preferred
    } else {
        needed.max(RECLAIM_MAX_ARRAY_LENGTH)
    }
}



pub(crate) fn transfer_to_exact() -> bool {
    true
}

/// InputStream.transferTo(OutputStream out) -> long (Java 9+)
/// Reads all bytes from this stream and writes them to the given output stream.
///
/// A `SyntheticStub` (refused under `--real-jdk`, which runs the JDK's own
/// loop); in `--compatible` it serves every stream that inherits
/// `transferTo`, so it must be that loop: see [`transfer_to_exact`].
pub(crate) fn native_is_transfer_to(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let exact = transfer_to_exact();
    let out = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        // `Objects.requireNonNull(out, "out")`, before the first read.
        _ if exact => {
            return Err(RuntimeError::NullPointerException {
                message: Some("out".into()),
            }
            .into())
        }
        _ => return Ok(Some(Value::Long(0))),
    };

    let this_pin = ctx.pin_native_root(this);
    let out_pin = ctx.pin_native_root(out);
    let buf = ctx.new_array(ArrayElementType::Byte, 16 * 1024);
    let buf_pin = ctx.pin_native_root(buf);
    let mut transferred: i64 = 0;

    loop {
        let this_cur = ctx.read_native_pin(this_pin, this);
        let buf_cur = ctx.read_native_pin(buf_pin, buf);
        let n = match ctx.invoke_virtual(
            this_cur,
            "read",
            "([BII)I",
            &[
                Value::Object(Some(buf_cur)),
                Value::Int(0),
                Value::Int(16 * 1024),
            ],
        ) {
            Ok(Some(Value::Int(n))) => n,
            Ok(_) => -1,
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        };
        // The JDK loops while `read >= 0` and writes an empty chunk for a `0`.
        if n < 0 || (n == 0 && !exact) {
            break;
        }

        let out_cur = ctx.read_native_pin(out_pin, out);
        let buf_cur = ctx.read_native_pin(buf_pin, buf);
        if let Err(e) = ctx.invoke_virtual(
            out_cur,
            "write",
            "([BII)V",
            &[Value::Object(Some(buf_cur)), Value::Int(0), Value::Int(n)],
        ) {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
        transferred = transferred.saturating_add(n as i64);
    }

    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Long(transferred)))
}


// JDK-ONLY-CLASSIFY: stub — `java.util.Scanner` declares zero ACC_NATIVE
// methods in JDK 25. All 37 registrations here shadow concrete bytecode (35
// on `Scanner` itself, 2 on abstract `Readable`/`Iterator` methods), so there
// is no VM/OS boundary being crossed: the OS boundary is one layer down, in the
// `InputStream` these natives read through, and that layer is already bridged.
//
// CORRECTION 2026-08-20 (H5-1): this paragraph used to end "Tagged `Bridge`
// purely by inheritance from `register_io_natives`". That is no longer true of
// the tree — this function sets `Bridge` EXPLICITLY at its head, because the
// 2026-08-19 retag attempt was reverted by restoring an explicit
// `set_category`, not the inheritance. The tag is a stated decision with a
// measured reason. Two further corrections from the same pass: "2 on abstract
// `Readable`/`Iterator` methods" is wrong about WHICH two — this crate
// registers nothing at all on `java/lang/Readable` or `java/util/Iterator`
// (the `hasNext`/`next` interface rows below are registered on
// `java/util/Scanner` itself); the two abstract targets are
// `java/io/Closeable.close()V` and `java/lang/AutoCloseable.close()V` at the
// foot of this function. And because the effective tag is `Bridge`, none of
// this is dropped under `--real-jdk`: this registrar is strict-mode LIVE,
// whatever the "stub" verdict above says.
//
// UPDATE 2026-08-20 (H11): those two abstract targets are MEASURED DEAD —
// `invocations: 0` for both across 15 corpus vectors and a purpose-built
// try-with-resources probe, and unreachable by construction because dispatch
// keys on the receiver's class and the fallback walk never visits an interface.
// They are still here only because `vm/src/vm/tests.rs`'s
// `auto_closeable_close_p70` calls the slot directly and its helper panics on
// an unregistered triple. The count stays 37 and the reason is a test outside
// this crate, not a dispatch requirement. Long note at the foot of this
// function.
/// Whether the real `java.util.Scanner` should run instead of this crate's
/// tokenizer, on a real JDK.
///
/// The same shape as `jdk_random_enabled` in `native-collections`: a
/// registration-time gate, so the family is not registered at all rather than
/// refused at dispatch. `CRATONVM_ENFORCE_NATIVE_SHADOW` cannot serve here --
/// it is a `--real-jdk` instrument and `NativeKind::allowed_in` is
/// unconditionally true for `Compatible`, so the dial moves compatible mode by
/// zero (measured: 50 diff lines with it and without it).
pub(crate) fn jdk_scanner_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("1") | Ok("true") | Ok("on")
        )
    })
}

pub(crate) fn register_scanner_natives(registry: &mut NativeMethodRegistry) {
    // right and the tag is nevertheless LOAD-BEARING, the same shape the
    // `FileInputStream` block in this file already calls out.
    //
    // `java.util.Scanner` declares no ACC_NATIVE method, so "stub" is the
    // correct verdict. But this VM OWNS a Scanner's state: the source is an
    // `Arc<str>` and the tokenizer is a Rust regex engine over `&str` (see
    // known-issues/jdk-only/G71-1). Refuse the shim under `--real-jdk` and the
    // real bytecode runs against a Scanner whose real fields were never
    // populated:
    //
    //   RJdkIntrinsics3: findWithinHorizon(String, 0) expected "42", got null
    //
    // MEASURED 2026-08-30 (lane L3), and the paragraph above is WRONG about the
    // blocker. `apps/probes/ScannerShadowSweep` is the first differential
    // coverage this class has ever had -- 94 rows, 43 owning registrations, and
    // it found 25 wrong rows IDENTICAL in both modes:
    //
    //   * `new Scanner("a").locale()` is NULL, where HotSpot answers the
    //     default -- an NPE in any caller that compares it;
    //   * `nextBigInteger()`, `hasNextBigInteger()`, `hasNextShort()` and
    //     `hasNextByte()` reach the parse path with RADIX 0
    //     (`IllegalArgumentException: radix:0`);
    //   * `nextBigDecimal()`, `nextBigInteger(radix)`, `skip(String)` and
    //     `findAll(String)` NPE on `this.matcher` / `this.patternCache`, real
    //     fields our `<init>` never populates;
    //   * `new Scanner("a,b,,c").useDelimiter(",")` walks `[a][b]` where HotSpot
    //     walks `[a][b][][c]` -- it drops the empty token AND everything after
    //     it;
    //   * `1,234` does not parse as a grouped int in any locale;
    //   * nine rows carry invented exception messages ("no more elements",
    //     "token mismatch") where HotSpot's are null or carry the
    //     `NumberFormatException` text.
    //
    // AND REFUSING THE FAMILY FIXES 24 OF THEM. Run under
    // `CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/Scanner`, the probe goes from 25
    // wrong rows to 12. The recorded objection -- "the real bytecode runs
    // against a Scanner whose real fields were never populated" -- described a
    // refusal that left `<init>` shadowed. Refuse the WHOLE family and the real
    // constructor runs, so the state is the JDK's and there is nothing left to
    // populate.
    //
    // The 11 rows that broke under the dial were not a missing capability
    // either. Scanner native invocations fell 186 -> 17 under it, and the 17
    // that SURVIVED are exactly the `findWithinHorizon` and `match` rows below:
    // the dial refuses `Bridge` and does not refuse `Intrinsic`. So real
    // `nextLine`/`findInLine` bytecode was calling a still-shadowed
    // `findWithinHorizon` that did not understand the real Scanner's state.
    // Both candidate capabilities were ruled out by measurement rather than
    // argument: `apps/probes/MatcherRegionProbe` is 39 of 40 rows clean on the
    // region-bounded `Matcher` API `findPatternInBuffer` runs on -- `region`,
    // `usePattern`, transparent and anchoring bounds, `hitEnd`, a `CharBuffer`
    // input and Scanner's own line pattern -- and `apps/probes/ReadableProbe` is
    // 0-diff on all 15 rows of `Readable.read(CharBuffer)`, the loop it drives.
    //
    // `SyntheticStub`, therefore, and the three `Intrinsic` rows below go with
    // it -- a family that is refused in part is the configuration that produced
    // the false blocker. `--real-jdk` now drops all 43 and runs java.base's own
    // `Scanner`. COMPATIBLE MODE IS UNCHANGED: `NativeKind::allowed_in` returns
    // an unconditional `true` there, so this moves the default mode by zero and
    // the 25 rows stay open in it, against the day the Rust tokenizer is retired
    // outright.
    // The compatible-mode half of the same retirement. `SyntheticStub` already
    // drops this family under `--real-jdk`; on a real JDK with the flag on it is
    // not registered in EITHER mode and `java.util.Scanner`'s own bytecode runs,
    // which is what would close the 25 rows listed above in the default mode.
    //
    // THE DEFAULT IS OFF, AND IT IS CHOSEN BY MEASUREMENT, exactly as the
    // sibling gate `jdk_random_enabled` is.
    //
    // Correctness says turn it ON. `apps/probes/ScannerShadowSweep`, 94 rows:
    //
    // ```text
    //             flag OFF        flag ON
    //   compat    50 diff lines   0
    //   jdk-only  0               0
    // ```
    //
    // -- the flag closes all 25 wrong rows in the DEFAULT mode, which
    // `SyntheticStub` alone cannot do (`NativeKind::allowed_in` is
    // unconditionally true for `Compatible`).
    //
    // AND IT COSTS NOTHING ELSE. The whole `java.util` corpus was re-run in
    // compatible mode with the flag ON -- UtilCoverage, UtilCoverage4,
    // UtilTail2, UtilTail, Collections, MapViews, Properties, ViewIdentity,
    // Base64 and UtilUnshadowed, 1631 rows -- and every one is byte-identical
    // to its flag-OFF run. Only `ScannerShadowSweep` moves, 50 diff lines to 0.
    // A retirement that closes 25 rows is worth little if it opens others
    // somewhere the probe for THIS class cannot see, so the corpus is the
    // check, not the class's own probe.
    //
    // Throughput says leave it off. `apps/probes/ScannerBench`, A/B/B/A
    // interleaved so load drift cannot be mistaken for the effect, ns per scan
    // of a 40-item source:
    //
    // ```text
    //                       shadowed          retired
    //   next (tokens)       623985, 672046    5118757, 4769729
    //   nextInt             518312, 435840    5181001, 5303971
    //   nextLine            380821, 323801    4712739, 6249495
    // ```
    //
    // 8x on tokens, 11x on typed reads, 15x on lines, and the A pair and the B
    // pair each agree with themselves. The real `Scanner` drives a `Matcher`
    // over a `CharBuffer` and re-reads its source through `findPatternInBuffer`
    // where this crate's tokenizer walks an `Arc<str>` with a Rust regex, so the
    // gap is structural.
    //
    // So the two modes get what each is for: `--real-jdk` already has the
    // correctness, because the family is `SyntheticStub` and is dropped there,
    // and compatible mode keeps the tokenizer and its 25 recorded rows. Turning
    // this on is a throughput decision and the numbers above are its price.
    if registry.real_jdk() && jdk_scanner_enabled() {
        return;
    }
        let c = "java/util/Scanner";

    // Constructors
    registry.register(
        c,
        "<init>",
        "(Ljava/lang/String;)V",
        native_scanner_init_string, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        c,
        "<init>",
        "(Ljava/io/InputStream;)V",
        native_scanner_init_inputstream, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // The charset overloads. See `native_scanner_init_inputstream_charset` for
    // what their absence did — it was silent, which is why it lasted.
    registry.register(
        c,
        "<init>",
        "(Ljava/io/InputStream;Ljava/lang/String;)V",
        native_scanner_init_inputstream_charset, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        c,
        "<init>",
        "(Ljava/io/InputStream;Ljava/nio/charset/Charset;)V",
        native_scanner_init_inputstream_charset, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        c,
        "<init>",
        "(Ljava/nio/channels/ReadableByteChannel;)V",
        native_scanner_init_channel, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        c,
        "<init>",
        "(Ljava/nio/channels/ReadableByteChannel;Ljava/lang/String;)V",
        native_scanner_init_channel, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        c,
        "<init>",
        "(Ljava/nio/channels/ReadableByteChannel;Ljava/nio/charset/Charset;)V",
        native_scanner_init_channel, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(c, "<init>", "(Ljava/io/File;)V", native_scanner_init_file, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        c,
        "<init>",
        "(Ljava/lang/Readable;)V",
        native_scanner_init_readable, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // Token reading
    registry.register(c, "next", "()Ljava/lang/String;", native_scanner_next, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        c,
        "nextLine",
        "()Ljava/lang/String;",
        native_scanner_next_line, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(c, "nextInt", "()I", native_scanner_next_int, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(c, "nextInt", "(I)I", native_scanner_next_int, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(c, "nextLong", "()J", native_scanner_next_long, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(c, "nextLong", "(I)J", native_scanner_next_long, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(c, "nextDouble", "()D", native_scanner_next_double, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(c, "nextFloat", "()F", native_scanner_next_float, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(c, "nextBoolean", "()Z", native_scanner_next_boolean, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(c, "nextByte", "()B", native_scanner_next_byte, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(c, "nextShort", "()S", native_scanner_next_short, cratonvm_native_api::NativeKind::SyntheticStub);

    // hasNext predicates
    registry.register(c, "hasNext", "()Z", native_scanner_has_next, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(c, "hasNextLine", "()Z", native_scanner_has_next_line, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(c, "hasNextInt", "()Z", native_scanner_has_next_int, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(c, "hasNextInt", "(I)Z", native_scanner_has_next_int, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(c, "hasNextLong", "()Z", native_scanner_has_next_long, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(c, "hasNextDouble", "()Z", native_scanner_has_next_double, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(c, "hasNextFloat", "()Z", native_scanner_has_next_float, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(c, "hasNextBoolean", "()Z", native_scanner_has_next_boolean, cratonvm_native_api::NativeKind::SyntheticStub);

    // Configuration
    registry.register(
        c,
        "useDelimiter",
        "(Ljava/lang/String;)Ljava/util/Scanner;",
        native_scanner_use_delimiter_string, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        c,
        "useDelimiter",
        "(Ljava/util/regex/Pattern;)Ljava/util/Scanner;",
        native_scanner_use_delimiter_pattern, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        c,
        "useRadix",
        "(I)Ljava/util/Scanner;",
        native_scanner_use_radix, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(c, "radix", "()I", native_scanner_radix, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        c,
        "delimiter",
        "()Ljava/util/regex/Pattern;",
        native_scanner_delimiter, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(c, "close", "()V", native_scanner_close, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(c, "reset", "()Ljava/util/Scanner;", native_scanner_reset, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        c,
        "toString",
        "()Ljava/lang/String;",
        native_scanner_to_string, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // Utility
    registry.register(
        c,
        "findInLine",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_scanner_find_in_line, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        c,
        "skip",
        "(Ljava/util/regex/Pattern;)Ljava/util/Scanner;",
        native_scanner_skip, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // `findWithinHorizon` had an implementation in
    // `native-builtins/src/phases_early.rs` that NOTHING registered: its only
    // registrar, `register_t2_3_completion_natives`, has no call site, and
    // `--dump-native-registry` showed no such row among the 35 live
    // `java/util/Scanner` entries. So the call reached real JDK bytecode, which
    // reads `buf`, `matcher` and `source` — none of which these natives
    // populate — and threw `NullPointerException` in both modes, measured by
    // `probes/L3ScannerSearchProbe`. It lives here now, beside `findInLine` and
    // `skip`, which share its state accessors and its regex engine.
    //
    // Registered as INTRINSIC, with the kind STATED rather than inherited.
    // `java.util.Scanner` declares no ACC_NATIVE method, so a `Bridge` tag —
    // which contract §1.5 defines by an ACC_NATIVE target — would be wrong,
    // and the L6 ratchet says so in as many words: two new Bridge rows
    // shadowing concrete bytecode is a regression it refuses, and raising the
    // baseline is explicitly not the fix. Intrinsic is what these are: a Rust
    // fast path replicating a method that HAS real bytecode and has to match
    // it, which is the category the implementation in `phases_early.rs` used
    // before it moved here.
    //
    // The other 35 registrations in this function are `SyntheticStub` by
    // inheritance as of 2026-08-30, and these three now match them. Leaving
    // them `Intrinsic` is what made a partial refusal look like a missing
    // capability: under the enforce dial these were the only Scanner natives
    // still running, and real `nextLine` bytecode called them.
    registry.register(
        c,
        "findWithinHorizon",
        "(Ljava/lang/String;I)Ljava/lang/String;",
        native_scanner_find_within_horizon_string,
        cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        c,
        "findWithinHorizon",
        "(Ljava/util/regex/Pattern;I)Ljava/lang/String;",
        native_scanner_find_within_horizon_pattern,
        cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // `match()` is Intrinsic for the same reason as `findWithinHorizon` above:
    // `java.util.Scanner` declares no ACC_NATIVE method, so contract §1.5's
    // `Bridge` does not describe it, and the L6 ratchet refuses a new Bridge
    // row that shadows concrete bytecode.
    registry.register(
        c,
        "match",
        "()Ljava/util/regex/MatchResult;",
        native_scanner_match,
        cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // Interface dispatch: Iterator
    registry.register(c, "hasNext", "()Z", native_scanner_has_next, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(c, "next", "()Ljava/lang/Object;", native_scanner_next, cratonvm_native_api::NativeKind::SyntheticStub);

    // Interface dispatch: Closeable
    //
    // MEASURED DEAD 2026-08-20 (H11), KEPT ONLY BECAUSE A TEST OUTSIDE THIS
    // CRATE CALLS THE SLOT DIRECTLY. Both rows are unreachable through ordinary
    // dispatch and the second of them is an active hazard; the deletion is
    // written, priced and blocked. Details, so the next lane does not re-derive
    // them:
    //
    // 1. UNREACHABLE. Native dispatch keys on the RECEIVER OBJECT's runtime
    //    class, not on the constant-pool class named at the call site — the
    //    `class_name` handed to `resolve_step1_native` is `invoke.rs`'s
    //    `invoke_class`, whose non-null-object branch resolves to the
    //    receiver's own class — and the ONE fallback walk in
    //    `try_stackless_invoke` follows `superclass` links only
    //    (`cm.get_class(cid)?.superclass?`), so it never visits an INTERFACE.
    //    An interface registration is reachable only for a receiver whose
    //    runtime class name IS the interface, and nothing in this tree mints a
    //    `java/io/Closeable` or a `java/lang/AutoCloseable`.
    //
    // 2. NOT EVEN FOR A SCANNER. This function registers
    //    `java/util/Scanner.close()V` directly, ~80 lines above, with the same
    //    callback; the receiver-keyed lookup finds THAT row first for every
    //    real Scanner. The "Interface dispatch" label describes an intent the
    //    dispatcher does not implement.
    //
    // 3. MEASURED. `--dump-native-registry` under `--real-jdk` on `fe59bf9d9`,
    //    unioned over 15 corpus vectors plus a probe that runs a
    //    try-with-resources over an `AutoCloseable`-typed local holding a user
    //    class: BOTH rows report `invocations: 0`, and the user's `close()`
    //    body runs, byte-identical to HotSpot 25.0.3+9. `java/util/Scanner.
    //    close()V` is also 0 across that union; the traffic these rows were
    //    imagined to carry does not exist.
    //
    // 4. THE HAZARD IS ALREADY DEFUSED, and not by this lane. `H8-1`
    //    (`e9f08d42b`) replaced `native_scanner_close`'s `Ok(None)` decline
    //    with a real yield through `invoke_virtual_bytecode_only`, so a
    //    non-Scanner receiver that did arrive here now runs its own `close()`
    //    instead of having it swallowed. Do not re-derive the swallow from an
    //    older record: `H5-1` N2 and `HANDOFF-20260820` both describe the
    //    pre-`e9f08d42b` body. What is left is dead weight, not damage.
    //
    // 4b. AND THERE IS A SECOND, INDEPENDENT BARRIER. `H8-1` found it from
    //    source and this lane's measurement is the first run behind it: at
    //    step 6 of `execute_invoke_kind` (`invoke.rs`, the
    //    `!(declaring_is_interface && !is_static)` gate) and at `vm_exec.rs`'s
    //    `override_cb` arm, a native whose RESOLVED DECLARING class is an
    //    interface instance method is dropped outright unless the triple is on
    //    `should_force_registered_native_over_bytecode`'s list. Neither of
    //    these two triples is. So the rows are shut out at two different steps
    //    for two different reasons — receiver-keying at step 1, the
    //    interface-default gate at step 6.
    //
    // 5. RETIRED 2026-08-21 (WORKER 4). The two registrations that stood here
    //    --
    //
    //        registry.register("java/io/Closeable",       "close", "()V", native_scanner_close);
    //        registry.register("java/lang/AutoCloseable", "close", "()V", native_scanner_close);
    //
    //    -- are gone. The only thing that had been keeping them was
    //    `vm/src/vm/tests.rs`'s `auto_closeable_close_p70`, which calls the
    //    triple DIRECTLY through a helper that `panic!`s on an unregistered
    //    one; `H11-3` N1 wrote the deletion out verbatim and could not make it,
    //    because `vm/` was out of that lane's bounds. It is a two-file commit
    //    and this is the other file.
    //
    //    Trap 4 checked, not assumed (`WORKER-4` brief: retiring the winner
    //    PROMOTES the loser). MEASURED, `--dump-native-registry`, `--real-jdk`:
    //
    //        java/io/Closeable.close()V        owns_slot=True inv=0  [native-io/src/lib.rs:8050]
    //        java/lang/AutoCloseable.close()V  owns_slot=True inv=0  [native-io/src/lib.rs:8051]
    //
    //    ONE row each, from this file. `dupX = 0`, so the deletion removes two
    //    registry rows and hands nothing to anybody -- unlike `pipe.rs`'s six
    //    abstract rows, where `net_channels.rs` is waiting underneath with an
    //    incompatible body (`H11-2` §4).
    //
    //    Items 1-4b above are the evidence that nothing reached them, and they
    //    stay because they are the argument. `java/util/Scanner.close()V` is
    //    still registered ~80 lines up, with this same callback, and that is
    //    the row every real Scanner has always used.
    }

// ===========================================================================
// java.nio — ByteBuffer, Channels (Phase 23)
// ===========================================================================

// ByteBuffer layout: CratonVM's 5-field SYNTHETIC one. It is not
// `java/nio/ByteBuffer`'s layout and never was — the real one, `javap -p` on
// Eclipse Adoptium 25.0.3.9, transitive, `static` excluded, superclass first
// (W7-76-bytebuffer-alias-residuals.md re-derived it independently of W7-58
// and W7-69 and agrees with both):
//
//     java.nio.Buffer:      mark(0) position(1) limit(2) capacity(3)
//                           address(4) segment(5)
//     java.nio.ByteBuffer:  hb(6) offset(7) isReadOnly(8) bigEndian(9)
//                           nativeByteOrder(10)
//     java.nio.HeapByteBuffer: declares NO instance fields — the 11 above are
//                           the whole of it.
//
// So of the five below, only `position` and `limit` mean the same thing on
// both layouts, and that is a coincidence of declaration order.
//
// Every production reader in this module resolves BY NAME first and reaches
// these indices only when the name does not resolve — the standing W4-4
// remedy, and the reason these constants have survived. W7-76 established
// per slot whether a real receiver can reach the numeric arm at all; the
// verdicts live on the constants themselves, so the next reader does not
// re-derive them and, in particular, does not renumber one crate's copy.
pub(crate) const BB_FIELD_ARRAY: usize = 0; // byte[] backing array
pub(crate) const BB_FIELD_POS: usize = 1; // Int position
pub(crate) const BB_FIELD_LIMIT: usize = 2; // Int limit
pub(crate) const BB_FIELD_CAPACITY: usize = 3; // Int capacity
/// Synthetic-layout `mark`. **On the real layout slot 4 is
/// `java.nio.Buffer.address`, a `long`** — see [`BB_ADDRESS_SLOT`], which is
/// the same index under the name of what is really there.
///
/// Reachability, W7-76: `buf_read_mark`'s by-name read of `mark` resolves on
/// every real receiver (`Buffer.mark` is a declared `int`), so this fallback
/// is DEAD on the real layout and live only on the synthetic one, where it is
/// correct. `buf_set_mark`'s write is not dead — it is unconditional — which
/// is why that function saves and restores `address` around it.
pub(crate) const BB_FIELD_MARK: usize = 4; // Int mark (-1 = not set)
pub(crate) const BB_NUM_FIELDS: usize = 5;

/// Slot 4 again, under the name of what is really there on the real layout:
/// `java.nio.Buffer.address`.
///
/// **Deliberately the same index as [`BB_FIELD_MARK`], and deliberately not a
/// renumber.** W7-76 chose name-resolution over renumbering here for a reason
/// that is a property of the tree rather than a preference:
///
/// * On the REAL layout slot 4 *is* `address`, so `bb_resolve_direct_address`'s
///   numeric fallback is right — and it is the one fallback in this family a
///   real receiver genuinely reaches, because a real `HeapByteBuffer` carries
///   `address = ARRAY_BYTE_BASE_OFFSET = 16`, which
///   [`is_plausible_native_addr`] refuses, dropping through to this read.
/// * On `native-builtins`' six-slot synthetic layout slot 4 is *also* the
///   direct address: `servlet.rs`'s independently-declared `BB_MARK = 4`
///   carries the `NativeMemoryTable` pointer that `s2_bb_alloc_direct` seeds,
///   and those buffers are precisely the receivers this module's direct arm
///   exists to serve.
///
/// So moving `mark` off 4 in this crate alone would move the disagreement
/// rather than close it — W7-68-live-under-allocations.md §3.2's `FileChannel`
/// rule, met here in the direction where the shared index is the CORRECT one.
/// What was actually wrong was that one index carried one name while three
/// call sites held two different beliefs about it; both beliefs are now named,
/// and both are published in [`BB_SLOT_MAP`] so the census reports the wrong
/// one and stays clean on the right one.
///
/// The two readers are separated only by VALUE TYPE — `Value::Int` is a mark,
/// `Value::Long` past [`is_plausible_native_addr`] is an address. That is
/// thin, and it is what the census row is for.
pub(crate) const BB_ADDRESS_SLOT: usize = BB_FIELD_MARK;

/// The slot [`bb_resolve_heap_offset`]'s numeric fallback reads, expecting
/// `ByteBuffer.offset`. It was a bare `6` until W7-76; naming it is the point
/// of the exercise, because on the real layout **slot 6 is `hb`** — the
/// backing `byte[]` — and `offset` is 7.
///
/// W7-76's verdict: right on NO layout, and reachable on none of them.
///
/// * Real layout: 6 is `hb`, a reference. The `Value::Int` match is the only
///   thing between it and a fabricated offset — and it is unreachable anyway,
///   because `offset` is a declared `int` on every real `ByteBuffer` subclass,
///   so the by-name arm above always answers.
/// * CratonVM's five-slot synthetic layout: there is no slot 6.
/// * `native-builtins`' six-slot synthetic layout: slot 6 is `BB_NATIVE_ID`,
///   a `Long` alloc id — refused by the same match. Its typed views encode
///   their byte start at slot 4 as `-(start + 1)` (`s2_bb_int_byte_off`), not
///   here.
///
/// Not renumbered to 7, because a second numeric guess buys nothing the
/// by-name arm does not already answer on every receiver that has the field.
/// Not deleted, because `bb_get_bulk_reads_real_heap_layout_slot_hb` in this
/// file's test module drives exactly this arm, and retiring a live test on a
/// branch that cannot run `cargo` is how a silent behaviour change ships.
pub(crate) const BB_HEAP_OFFSET_FALLBACK_SLOT: usize = 6;

/// The constants above, restated as a machine-readable `(slot, field name)`
/// table for the READ-side alias census.
///
/// W7-59-layout-detector-coverage.md section 6 named exactly this gap: the
/// per-native slot maps are `const F_x: usize = k` constants "with no
/// machine-readable link to a field name", so no instrument could say "slot 0
/// is `mark`, not `hb`". This is that link, and it is `const` data — it costs
/// nothing at runtime and nothing when the flag is off.
///
/// **W7-76 corrected the shape this table publishes**, which was wrong in two
/// ways that pull in opposite directions:
///
/// 1. It was INCOMPLETE and did not say so. `bb_resolve_heap_offset`'s slot-6
///    read — the third wrong slot, and the one
///    W7-69-read-side-alias-instrument.md §6.3 files as a defect — was not in
///    the table at all, so `verify_declared_slot_maps` could sweep the map
///    clean of it forever. A table that looks total and is not reads as
///    coverage.
/// 2. It gave slot 4 ONE name where three call sites hold TWO beliefs.
///    `buf_read_mark`/`buf_set_mark` believe `mark` (true on the synthetic
///    layout, false on the real one); `bb_resolve_direct_address` believes
///    `address` (true on both, and the live one). Publishing only `mark` made
///    the sweep print one wrong-field row for slot 4, which reads as "the
///    reader that reaches slot 4 is broken" when the reader that reaches it on
///    a real receiver is correct. Both are published now: `slots` is a slice,
///    not a map, so a slot may legitimately appear twice, and
///    `already_reported` dedupes on `(class, slot, expected, site)` and so
///    keeps them apart.
///
/// Swept against the loaded `java/nio/ByteBuffer` it should therefore report
/// exactly three `wrong-field` rows — `0 hb→mark`, `4 mark→address`,
/// `6 offset→hb` — and stay clean on the other five, including the second
/// slot-4 row. Nothing here is repaired by renumbering; see [`BB_ADDRESS_SLOT`]
/// and [`BB_HEAP_OFFSET_FALLBACK_SLOT`] for why, per slot.
pub static BB_SLOT_MAP: cratonvm_native_api::read_alias::SlotMap =
    cratonvm_native_api::read_alias::SlotMap {
        class: "java/nio/ByteBuffer",
        slots: &[
            // WRONG on the real class: 0 is `Buffer.mark`. The calibration
            // case — a real `DirectByteBuffer` reaches it, because `hb`
            // resolves by name to NULL rather than failing to resolve.
            (BB_FIELD_ARRAY, "hb"),
            (BB_FIELD_POS, "position"),
            (BB_FIELD_LIMIT, "limit"),
            (BB_FIELD_CAPACITY, "capacity"),
            // WRONG on the real class: 4 is `Buffer.address`. Dead on a real
            // receiver for the READ (`buf_read_mark` resolves `mark` by name);
            // live for the WRITE (`buf_set_mark` is unconditional and
            // compensates with a save/restore).
            (BB_FIELD_MARK, "mark"),
            // The same slot, the other belief, and this one is CORRECT — the
            // deliberate non-firing control W7-69 §3 built the instrument
            // around, now published rather than only wired at the call site.
            (BB_ADDRESS_SLOT, "address"),
            (BB_SEGMENT_SLOT, "segment"),
            // WRONG on the real class: 6 is `ByteBuffer.hb`, an array, where
            // an `Int` offset is read. Unreachable on every layout this tree
            // has; see the constant.
            (BB_HEAP_OFFSET_FALLBACK_SLOT, "offset"),
        ],
        origin: "native-io/src/lib.rs BB_FIELD_*",
    };

// FileChannel: the private `{fd, position}` map used to be slots 0 and 1 of a
// class that declares FOUR, and it MOVED — W7-72-ssc-socket-and-filechannel.md.
// It now lives above every declared field,
// owned by `cratonvm_native_api::synthetic_file_channel`, which both this crate
// and `native-builtins` call. The constants that used to sit here are gone on
// purpose: a map with two owners drifts, and two crates owned this one.
//
// `javap -p java.nio.channels.FileChannel` on JDK 25.0.3.9 gives the
// transitive order
//
//     0 closeLock (Object, final)   1 closed (boolean, volatile)
//     2 interruptor (Interruptible) 3 interruptedTarget (Object, volatile)
//
// all four inherited from `java.nio.channels.spi.AbstractInterruptibleChannel`.
// So the fd was an `Int` in `closeLock` — a reference-typed slot, where
// `gc::coerce_field_value_by_descriptor` degrades it to null, so the fd never
// persisted at all in real-JDK mode — and the file position was a `Long` in
// `closed`, the boolean `AbstractInterruptibleChannel.isOpen()` returns the
// negation of. The object was never SHORT (the base allocator clamps
// `slots = requested.max(declared)`); it was mis-mapped.
//
// W7-68-live-under-allocations.md left this alone on the reading that
// `nio_file.rs`'s surviving `isOpen` reads slot 0 as the fd, so moving the map
// would make `isOpen()` answer FALSE for every open channel. **That reading of
// which registration survives is inverted** — see
// W7-72-ssc-socket-and-filechannel.md for the ordering, established by reading
// `vm_init.rs`'s two arms: the copy that reads slot 0 is registered only from
// `register_phase57_file_channel`, which is reachable only through
// `register_synthetic_overrides` (`#[cfg(feature = "synthetic-jdk")]`) and is
// overwritten even there. The winner in every configuration is the copy that
// never reads a private slot. Its body now answers `!closed` on both arms, so
// the two registrations agree and the last-write-wins outcome stops mattering
// for that triple.

// RA.1: When a real JDK `java.nio.Buffer` (or subclass) is loaded, its
// declared-field order is `mark, position, limit, capacity, address` on
// Buffer, then `hb, offset, isReadOnly` on Heap*Buffer — which does not
// match our synthetic BB_FIELD_* offsets. Writing via by-name resolution
// hits the *real* JDK slot; the hardcoded write remains so that
// synthetic-mode objects (ClassId 0 / no fields declared) still work.
//
// Both writes are issued; at most one is load-bearing in any given mode.
pub(crate) fn buf_write_metadata(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    position: i32,
    limit: i32,
    capacity: i32,
    mark: i32,
) {
    // Synthetic-mode slots
    buf_set_position(ctx, obj, position);
    buf_set_limit(ctx, obj, limit);
    ctx.set_field(obj, BB_FIELD_CAPACITY, Value::Int(capacity));
    buf_set_mark(ctx, obj, mark);
    // Real-JDK slots (no-op if class has no such field)
    ctx.set_field_by_name(obj, "position", Value::Int(position));
    ctx.set_field_by_name(obj, "limit", Value::Int(limit));
    ctx.set_field_by_name(obj, "capacity", Value::Int(capacity));
    ctx.set_field_by_name(obj, "mark", Value::Int(mark));
}

/// Write just `position`, dual-targeted (synthetic slot + real JDK name).
pub(crate) fn buf_set_position(ctx: &mut dyn NativeContext, obj: ObjectRef, v: i32) {
    ctx.set_field(obj, BB_FIELD_POS, Value::Int(v));
    ctx.set_field_by_name(obj, "position", Value::Int(v));
}

/// Write just `limit`, dual-targeted.
pub(crate) fn buf_set_limit(ctx: &mut dyn NativeContext, obj: ObjectRef, v: i32) {
    ctx.set_field(obj, BB_FIELD_LIMIT, Value::Int(v));
    ctx.set_field_by_name(obj, "limit", Value::Int(v));
}

/// Write just `mark`, dual-targeted.
///
/// AUDIT 2026-08-05: index 4 on a REAL-JDK `java.nio.Buffer` is `address`, not
/// `mark` — the hierarchy-wide order is `mark(0) position(1) limit(2)
/// capacity(3) address(4)`. The other three indices line up with their by-name
/// twins by luck; this one does not, so the synthetic-mode write silently
/// stamped the mark value onto `address`.
///
/// That is not cosmetic. `Buffer.address` is what `ScopedMemoryAccess` /
/// `Unsafe.copyMemory` read for every bulk `put(<same-kind>Buffer)`; with
/// `address = -1` the offset falls below `arrayBaseOffset` and the copy throws
/// `ArrayIndexOutOfBoundsException`. The CharBuffer half of this defect broke
/// every source file javac read (`BaseFileManager.decode` grows a CharBuffer
/// and copies the old one in) and was fixed separately; this is the same bug in
/// the ByteBuffer / typed-buffer family, where EVERY mutator routes here —
/// `position`, `limit`, `mark`, `reset`, `clear`, `flip`, `rewind`, `compact`,
/// `duplicate`.
///
/// Save and restore rather than recompute: a heap buffer's address is
/// `arrayBaseOffset + offset * scale` (so a slice's is NOT the bare base
/// offset), and a DIRECT buffer's is a real native pointer that must never be
/// synthesised. Preserving whatever the object already carries is correct for
/// all three. Synthetic-mode objects have no `address` field, the read yields a
/// non-`Long`, and nothing is restored.
pub(crate) fn buf_set_mark(ctx: &mut dyn NativeContext, obj: ObjectRef, v: i32) {
    let saved_address = ctx.get_field_by_name(obj, "address");
    // W7-69, observation only. A WRITE through an aliased slot is the same
    // species and the same call — the finding is about what slot 4 MEANS, not
    // about the direction of the access. On a real Buffer this `Int` lands on
    // `address`, which is why the save/restore below exists at all; the census
    // row names the field the restore is compensating for.
    if layout_alias::enabled() {
        read_alias::observe_read(
            &*ctx,
            obj,
            BB_FIELD_MARK,
            "mark",
            "native-io/src/lib.rs::buf_set_mark (write)",
        );
    }
    ctx.set_field(obj, BB_FIELD_MARK, Value::Int(v));
    ctx.set_field_by_name(obj, "mark", Value::Int(v));
    if let Value::Long(_) = saved_address {
        ctx.set_field_by_name(obj, "address", saved_address);
    }
}

/// Read `position`, preferring real JDK slot when present.
pub(crate) fn buf_read_position(ctx: &dyn NativeContext, obj: ObjectRef) -> i32 {
    if let Value::Int(v) = ctx.get_field_by_name(obj, "position") {
        return v;
    }
    if let Value::Int(v) = ctx.get_field(obj, BB_FIELD_POS) {
        return v;
    }
    0
}

/// Read `limit`, preferring real JDK slot when present.
pub(crate) fn buf_read_limit(ctx: &dyn NativeContext, obj: ObjectRef) -> i32 {
    if let Value::Int(v) = ctx.get_field_by_name(obj, "limit") {
        return v;
    }
    if let Value::Int(v) = ctx.get_field(obj, BB_FIELD_LIMIT) {
        return v;
    }
    0
}

/// Read `mark`, preferring real JDK slot when present.
pub(crate) fn buf_read_mark(ctx: &dyn NativeContext, obj: ObjectRef) -> i32 {
    if let Value::Int(v) = ctx.get_field_by_name(obj, "mark") {
        return v;
    }
    // W7-69, observation only. On a REAL java.nio.Buffer slot 4 is `address`
    // (a long), not `mark` — `mark` is slot 0. Reaching here on a real receiver
    // means the by-name read above did not resolve, so this fallback is about to
    // read `address` and call it `mark`. The `Value::Int` match below stops it
    // returning a pointer as a mark, which is why this has never been a visible
    // bug; the census row is the point.
    if layout_alias::enabled() {
        read_alias::observe_read(
            ctx,
            obj,
            BB_FIELD_MARK,
            "mark",
            "native-io/src/lib.rs::buf_read_mark",
        );
    }
    if let Value::Int(v) = ctx.get_field(obj, BB_FIELD_MARK) {
        return v;
    }
    -1
}

/// Read `capacity`, preferring real JDK slot when present.
///
/// This same two-step name-then-slot fallback existed inline in three places
/// (`bb_state`, `bb_storage_view`, `native_bb_set_limit`). Named once so a
/// future layout correction has one place to land instead of three.
pub(crate) fn buf_read_capacity(ctx: &dyn NativeContext, obj: ObjectRef) -> i32 {
    if let Value::Int(v) = ctx.get_field_by_name(obj, "capacity") {
        return v;
    }
    if let Value::Int(v) = ctx.get_field(obj, BB_FIELD_CAPACITY) {
        return v;
    }
    0
}

/// `(position, limit, capacity)` for any buffer, **without requiring backing
/// storage**.
///
/// This is the half of `bb_state` that `remaining()`, `hasRemaining()` and the
/// typed `toString()` actually wanted. All three asked for the whole state
/// tuple, discarded the array with `_`, and inherited its refusal — so
/// `remaining()` on a `ByteBuffer.allocateDirect(n)` raised
/// `internal error: ByteBuffer missing backing array (field 0 returned Int(-1))`
/// while computing `limit - position`, two fields it had already read
/// correctly. The `-1` is `java.nio.Buffer.mark`, which is what slot 0 is on
/// the real layout; see `bb_state` for the species.
pub(crate) fn buf_metadata(ctx: &dyn NativeContext, this: ObjectRef) -> (i32, i32, i32) {
    (
        buf_read_position(ctx, this),
        buf_read_limit(ctx, this),
        buf_read_capacity(ctx, this),
    )
}

/// `Unsafe.ARRAY_BYTE_BASE_OFFSET` as this VM publishes it — the value real
/// `HeapByteBuffer`'s ctor adds to its array-base `offset` to form
/// `Buffer.address`. `native-builtins/src/servlet.rs` declares the same
/// constant under the same name for the same reason; the two must agree,
/// because [`bb_resolve_direct_address`] and `s2_bb_direct_addr` both screen
/// `address` values and both use `16` as the heap sentinel. `pub` so
/// `servlet.rs` can define its same-named const AS this one rather than beside
/// it — the crate dependency runs `cratonvm-native-builtins` →
/// `cratonvm-native-io` and not the reverse.
pub const ARRAY_BYTE_BASE_OFFSET: i64 = 16;

/// The window a DERIVED heap buffer occupies inside its source's backing
/// array: exactly the five arguments real `HeapByteBuffer`'s package-private
/// constructor takes, in the same units.
///
/// `offset` is the ELEMENT index of the derived buffer's logical element 0
/// inside the shared array — the real-JDK `ByteBuffer.offset` field, which
/// [`bb_resolve_heap_offset`] reads and every accessor in this file already
/// honours through [`BbStorage::Heap`]'s `offset`. It is what makes a derived
/// buffer an ALIAS rather than a copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferWindow {
    /// `ByteBuffer.offset` — element index of logical element 0 in the array.
    pub offset: usize,
    pub pos: i32,
    pub lim: i32,
    pub cap: i32,
    /// `-1` for "no mark", the JDK's own sentinel.
    pub mark: i32,
}

/// `slice()`'s window — JDK 25 `HeapByteBuffer.slice()`:
///
/// ```java
/// int pos = this.position();
/// int lim = this.limit();
/// int rem = (pos <= lim ? lim - pos : 0);
/// return new HeapByteBuffer(hb, -1, 0, rem, rem, pos + offset, segment);
/// ```
///
/// Three things a copying implementation cannot express and this can:
/// `offset` COMPOSES (`pos + offset`, so a slice of a slice lands at the sum),
/// `cap` is the remaining count (not the source's), and the mark is DROPPED.
///
/// MEASURED, jdk-25.0.3+9 (`scratchpad/f26/F26AliasProbe.java`):
/// `allocate(8).position(2).slice()` → `pos/lim/cap = 0/6/6`,
/// `array() == ` the SOURCE array by identity, `arrayOffset() == 2`, and
/// `slice().slice(1,2).arrayOffset() == 3`. A buffer minted by copying can
/// only ever answer `arrayOffset() == 0` and can never satisfy the `==`.
/// `allocate(4).position(4).slice().arrayOffset()` is `4` even though the
/// slice is EMPTY, which is why `offset` is computed from `pos` unconditionally
/// and not only when something remains.
#[inline]
pub fn slice_window(src_offset: usize, src_pos: i32, src_lim: i32) -> BufferWindow {
    let pos = src_pos.max(0);
    let rem = if pos <= src_lim { src_lim - pos } else { 0 };
    BufferWindow {
        offset: src_offset.saturating_add(pos as usize),
        pos: 0,
        lim: rem,
        cap: rem,
        mark: -1,
    }
}

/// `slice(int index, int length)`'s window — JDK 25
/// `HeapByteBuffer.slice(int,int)`:
///
/// ```java
/// Objects.checkFromIndexSize(index, length, limit());
/// return new HeapByteBuffer(hb, -1, 0, length, length, index + offset, segment);
/// ```
///
/// Absolute-indexed: `index` is a LIMIT-space index and the source's
/// `position` plays no part. The bounds check is the caller's
/// ([`buffer_check_from_index_size`]) because it must run before anything is
/// allocated. MEASURED: `allocate(8).position(2).slice(3,4)` → `0/4/4`,
/// `arrayOffset() == 3` — 3, not 5, so `position` really is ignored.
#[inline]
pub fn slice_range_window(src_offset: usize, index: i32, length: i32) -> BufferWindow {
    let index = index.max(0);
    let length = length.max(0);
    BufferWindow {
        offset: src_offset.saturating_add(index as usize),
        pos: 0,
        lim: length,
        cap: length,
        mark: -1,
    }
}

/// `duplicate()` / `asReadOnlyBuffer()`'s window — JDK 25
/// `HeapByteBuffer.duplicate()`:
///
/// ```java
/// return new HeapByteBuffer(hb, this.markValue(), this.position(),
///                           this.limit(), this.capacity(), offset, segment);
/// ```
///
/// **`offset` is carried across unchanged, and that is the half this crate was
/// missing.** F21 taught `native_bb_duplicate` to share the source's array but
/// left `offset` at whatever the fresh allocation contained, i.e. `0`. That is
/// harmless only while nothing in the VM can produce a source with a non-zero
/// `offset` — and `native-builtins/src/servlet.rs`'s `ByteBuffer.slice(int,int)`
/// and `asReadOnlyBuffer()` already can (they call `s2_bb_new_heap_view`, which
/// writes `offset` by name), and neither of those two descriptors is registered
/// by `register_nio_natives`, so neither is shadowed. So
/// `bb.slice(2,4).duplicate().get(0)` read the array from index 0 instead of 2:
/// a silent WRONG VALUE, not an exception.
///
/// MEASURED: `wrap(raw,2,4).slice().duplicate().arrayOffset()` is `2` and its
/// `get(0)` equals `raw[2]`. `duplicate()` also KEEPS the mark where
/// [`slice_window`] drops it (`duplicate` then `reset()` → position 3;
/// `slice` then `reset()` → `InvalidMarkException`).
#[inline]
pub fn duplicate_window(
    src_offset: usize,
    src_pos: i32,
    src_lim: i32,
    src_cap: i32,
    src_mark: i32,
) -> BufferWindow {
    BufferWindow {
        offset: src_offset,
        pos: src_pos,
        lim: src_lim,
        cap: src_cap,
        mark: src_mark,
    }
}

/// `Buffer.address` for a heap buffer whose array-base offset is `offset` —
/// real `HeapByteBuffer`'s `this.address = ARRAY_BASE_OFFSET + off *
/// ARRAY_INDEX_SCALE`, with `ARRAY_INDEX_SCALE == 1` for `byte`.
///
/// Not a bare `16`: an aliasing view's address is base + offset, and
/// `ScopedMemoryAccess`/`Unsafe.copyMemory` read it as an array-relative
/// offset. `servlet.rs::s2_bb_new_heap_view` computes the same value.
#[inline]
pub fn heap_buffer_address(offset: usize) -> i64 {
    ARRAY_BYTE_BASE_OFFSET.saturating_add(offset as i64)
}

/// [`heap_buffer_address`] for a TYPED heap buffer, where `offset` is in
/// ELEMENTS and the address is in BYTES.
///
/// Real `HeapIntBuffer`'s protected constructor
/// (`jdk25src/java.base/java/nio/HeapIntBuffer.java:111`):
///
/// ```java
/// this.address = ARRAY_BASE_OFFSET + off * ARRAY_INDEX_SCALE;
/// ```
///
/// **This is the whole reason the `ByteBuffer` repair does not generalise by
/// itself, and it is a one-line difference that a family fix loses if it is
/// written once.** `ByteBuffer`'s `ARRAY_INDEX_SCALE` is 1, so
/// [`heap_buffer_address`] can add `offset` directly; a `LongBuffer` view at
/// element offset 3 has `address == 16 + 24`, not `16 + 3`. Getting it wrong
/// does not throw — it hands `ScopedMemoryAccess` / `Unsafe.copyMemory` an
/// array-relative offset that is short by a factor of the element width, and
/// every bulk transfer through the view reads the wrong elements silently.
///
/// This VM publishes 16 as the base offset of EVERY primitive array
/// (`alloc_typed_buffer`'s own comment: "every primitive array's base offset
/// is 16 here, matching `unsafe_array_read_bytes`'s `ABASE`"), so the base
/// term is [`ARRAY_BYTE_BASE_OFFSET`] for all seven families and only the
/// scale differs.
///
/// `saturating_mul` then `saturating_add`: an offset large enough to overflow
/// is not reachable from a real array, and a wrapped negative address would be
/// handed to [`is_plausible_native_addr`], where a large positive wrap would
/// be ACCEPTED as a process pointer. Saturating pins the failure mode to "an
/// address so large nothing maps there", which the screen also accepts —
/// hence [`typed_view_address_stays_below_the_native_pointer_floor`], which
/// pins the bound rather than the arithmetic.
#[inline]
pub fn typed_buffer_address(offset: usize, elem_width: usize) -> i64 {
    ARRAY_BYTE_BASE_OFFSET.saturating_add((offset as i64).saturating_mul(elem_width as i64))
}

/// The `address` a DIRECT typed view must carry: the source block's address
/// advanced by the window's start, in BYTES.
///
/// `rel_start` is in ELEMENTS and relative to the SOURCE's own logical element
/// 0 — the same unit and origin `tb_read_elem`'s `index` uses — so the scale
/// applies here for exactly the reason it applies in [`typed_buffer_address`].
/// A direct view has no `offset` field and no backing array; the advanced
/// address IS its offset, which is why this arm has no representability
/// question and never falls back to a copy.
///
/// MEASURED, jdk-25.0.3+9: `ByteBuffer.allocateDirect(64).asIntBuffer()` is a
/// `java.nio.DirectIntBufferS`, and its `slice()`, `duplicate()` and
/// `asReadOnlyBuffer()` are ALL still `isDirect() == true` and still share
/// content (`direct.view.slice.get(0)` reads back the 21 written through the
/// source view). A derived buffer minted as a heap COPY answers `false` to
/// `isDirect()` and shares nothing — two wrong answers from one wrong storage
/// decision.
#[inline]
pub fn typed_direct_view_address(addr: i64, rel_start: usize, elem_width: usize) -> i64 {
    addr.saturating_add((rel_start as i64).saturating_mul(elem_width as i64))
}

/// Mint a bare `java/nio/ByteBuffer` instance with the byte-order fields
/// seeded, and NO storage installed.
///
/// Extracted from [`alloc_byte_buffer`] so the aliasing constructor
/// [`alloc_byte_buffer_over`] cannot drift from it — in particular so it
/// cannot forget the `bigEndian` seed, whose absence made `order()` answer
/// LITTLE_ENDIAN (see the long note in [`alloc_byte_buffer`]).
pub(crate) fn alloc_bb_object(ctx: &mut dyn NativeContext) -> ObjectRef {
    let obj = match ctx.ensure_class_initialized("java/nio/ByteBuffer") {
        Ok(cid) => ctx.alloc_object(cid, BB_NUM_FIELDS),
        Err(_) => ctx.alloc_object(cratonvm_types::ClassId::new(0), BB_NUM_FIELDS),
    };
    seed_buffer_byte_order(ctx, obj);
    obj
}

/// Seed the two byte-order fields `java.nio.ByteBuffer`'s FIELD INITIALISERS
/// would have set, on a buffer this VM minted with a raw `alloc_object` and no
/// constructor.
///
/// ```java
/// boolean bigEndian = true;
/// boolean nativeByteOrder = (ByteOrder.nativeOrder() == ByteOrder.BIG_ENDIAN);
/// ```
///
/// javac compiles both writes into every `ByteBuffer` constructor and nothing
/// else writes them, so a constructor-less mint leaves them at the Java default
/// `false` — and `ByteBuffer.order()` is
/// `return bigEndian ? BIG_ENDIAN : LITTLE_ENDIAN`, `final`, reading the field
/// directly, so no per-subclass native can correct it afterwards. Without this
/// seed a real-JDK-mode `ByteBuffer.allocate(n)` answers LITTLE_ENDIAN where
/// HotSpot answers BIG_ENDIAN, and **every typed read through it is byteswapped
/// relative to HotSpot** — a wrong value, not an exception.
///
/// **W7-76 §8.2, converged for the two files this lane owns.** That residual
/// named FIVE sites seeding this same pair with nothing to make them agree:
/// `native-io/src/lib.rs::alloc_byte_buffer`,
/// `native-builtins/src/servlet.rs::bb_write_hb`,
/// `native-io/src/direct_buffer.rs::dbb_allocate_direct0`,
/// `native-builtins/src/charset.rs:246` and `native-builtins/src/lib.rs:5685`.
/// The first two now delegate here and cannot drift; the other three are
/// NOMINATIONS in F26-1 §6, with exact text, because they are not this lane's
/// files. `pub` for exactly the reason [`buffer_is_read_only`] and
/// [`buffer_array_access`] are: `cratonvm-native-builtins` depends on
/// `cratonvm-native-io` and not the reverse, so this is the only one of the
/// copies the others can import.
///
/// `nativeByteOrder` is `bigEndian == (native order is big)`, i.e. `false` on
/// every little-endian target — the same value as the Java default. Written
/// anyway so the pair cannot drift if a big-endian target ever appears.
#[inline]
pub fn seed_buffer_byte_order(ctx: &mut dyn NativeContext, obj: ObjectRef) {
    ctx.set_field_by_name(obj, "bigEndian", Value::Int(1));
    ctx.set_field_by_name(
        obj,
        "nativeByteOrder",
        Value::Int(if cfg!(target_endian = "big") { 1 } else { 0 }),
    );
}

/// Record `window.offset` on `buf` and report whether the layout could carry
/// it.
///
/// **This is a round-trip probe, not a layout assumption, and that is
/// deliberate.** Aliasing at a non-zero base is representable only when the
/// receiver has a real `ByteBuffer.offset` field: in real-JDK mode
/// `java.nio.ByteBuffer` declares `final int offset` (VERIFIED with `javap -p
/// java.nio.ByteBuffer` on 25.0.3+9 — the declared set is `hb, offset,
/// isReadOnly, bigEndian, nativeByteOrder`), and `ctx.alloc_object` clamps the
/// requested width UP to the declared field count, so the by-name write lands.
/// In synthetic-JDK mode there is no such field, the write is a no-op, and
/// [`bb_resolve_heap_offset`] would answer `0` for a view that needs a
/// non-zero base — silently reading the WRONG BYTES.
///
/// Reading the value back and comparing is the only test that cannot be wrong
/// about which mode it is in. `servlet.rs` answers the same question with
/// `s2_bb_synthetic_layout` (a slot-width heuristic); this is the same
/// decision taken from the field itself.
///
/// An offset of `0` always round-trips by definition — every layout represents
/// base 0, and `bb_resolve_heap_offset` answers `0` when the field is absent —
/// so it is reported representable without writing anything.
pub(crate) fn buf_try_set_heap_offset(ctx: &mut dyn NativeContext, buf: ObjectRef, offset: usize) -> bool {
    if offset == 0 {
        return true;
    }
    let Ok(as_i32) = i32::try_from(offset) else {
        return false;
    };
    ctx.set_field_by_name(buf, "offset", Value::Int(as_i32));
    matches!(ctx.get_field_by_name(buf, "offset"), Value::Int(v) if v == as_i32)
}

/// Mint an ALIASING heap `ByteBuffer` over `arr` at `window`.
///
/// `None` when the layout cannot carry `window.offset` (see
/// [`buf_try_set_heap_offset`]); callers fall back to copying, which is
/// data-correct and is what the whole family did before.
///
/// Both spellings of the backing array are written for the same reason
/// [`alloc_byte_buffer`] writes both: real heap-buffer subclasses read `hb`,
/// the synthetic layout reads slot 0, and [`bb_resolve_heap_array`] prefers
/// whichever resolves.
///
/// **`arr` is PINNED across the allocation, and that is load-bearing.** Every
/// caller resolves `arr` BEFORE calling — from `bb_storage_view` for the
/// derived views, from `args[0]` for `wrap` — and `alloc_bb_object` allocates,
/// which can trigger a collection that RELOCATES it. Storing the stale
/// `ObjectRef` afterwards installs a dangling backing array. The pre-existing
/// `native_bb_duplicate` was safe only by accident of ordering (it read the
/// array field *after* its allocation); routing the array in as a parameter
/// removes that accident, so the pin replaces it. Same idiom, same reason, as
/// `servlet.rs`'s `chars_arr_pin` in the `asCharBuffer` transcode path.
pub(crate) fn alloc_byte_buffer_over(
    ctx: &mut dyn NativeContext,
    arr: ObjectRef,
    window: BufferWindow,
) -> Option<ObjectRef> {
    let arr_pin = ctx.pin_native_root(arr);
    let obj = alloc_bb_object(ctx);
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(arr_pin);
    if !buf_try_set_heap_offset(ctx, obj, window.offset) {
        return None;
    }
    ctx.set_field(obj, BB_FIELD_ARRAY, Value::Object(Some(arr)));
    ctx.set_field_by_name(obj, "hb", Value::Object(Some(arr)));
    buf_write_metadata(ctx, obj, window.pos, window.lim, window.cap, window.mark);
    // AFTER `buf_write_metadata`: `buf_set_mark` saves and restores whatever
    // `address` it finds, so a write issued before it would survive — but it
    // would survive as the SAVED value, and the ordering that does not depend
    // on that subtlety is this one.
    ctx.set_field_by_name(
        obj,
        "address",
        Value::Long(heap_buffer_address(window.offset)),
    );
    Some(obj)
}

pub(crate) fn alloc_byte_buffer(ctx: &mut dyn NativeContext, capacity: usize) -> ObjectRef {
    // Mint + byte-order seed via `alloc_bb_object` (W7-76 §8.2 convergence —
    // see `seed_buffer_byte_order`). The seed used to sit at the BOTTOM of this
    // function; moving it to the top is safe because nothing between the two
    // points reads or writes `bigEndian`/`nativeByteOrder` (`buf_set_mark`
    // saves and restores `address` only).
    let obj = alloc_bb_object(ctx);
    // GC: nothing in Java refers to this object yet, so this Rust local is
    // its ONLY reference — and the allocation below can collect. Under the
    // moving collector that leaves the local stale; under the Generational
    // collector's NON-MOVING young sweep the object is unreachable and gets
    // ZEROED in place. Pin across the allocation and read the live address
    // back (`unpinned-native-locals` family; `native-io` was outside the
    // 2026-08-25 audit's scope, which covered `native-builtins/src`).
    let __pin = ctx.pin_native_root(obj);
    let array = ctx.new_array(ArrayElementType::Byte, capacity);
    install_byte_buffer_array(ctx, __pin, obj, array, capacity)
}

/// [`alloc_byte_buffer`] whose caller-sized backing array reclaims and
/// retries before its `OutOfMemoryError` (gc-common w13-e,
/// `common-w12e-native-io-write-side-growth-single-attempt`).
///
/// The contract is [`new_byte_array_reclaiming`]'s: the reclaim is a real,
/// moving collection, so **the caller must hold no unpinned `ObjectRef` it
/// uses afterwards** and must be reached only as a registered native. The new
/// buffer object itself is pinned across the array allocation here, exactly
/// as in [`alloc_byte_buffer`]. The callers are `ByteBuffer.allocate`, the two
/// `wrap` fallbacks, [`bb_derive_view`]'s heap copy and `StreamDecoder`'s
/// channel refill; each states its own `live` audit.
///
/// `native-builtins`' `native_heap_bytebuffer_allocate` (the `allocate` most
/// real-JDK runs dispatch to) already reclaims; this is native-io's twin.
pub(crate) fn alloc_byte_buffer_reclaiming(
    ctx: &mut dyn NativeContext,
    capacity: usize,
    what: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = alloc_bb_object(ctx);
    let pin = ctx.pin_native_root(obj);
    let array = match new_byte_array_reclaiming(ctx, capacity, what) {
        Ok(array) => array,
        Err(oome) => {
            ctx.unpin_native_roots(pin);
            return Err(oome);
        }
    };
    Ok(install_byte_buffer_array(ctx, pin, obj, array, capacity))
}

/// The tail [`alloc_byte_buffer`] and [`alloc_byte_buffer_reclaiming`] share:
/// install `array` as `obj`'s backing store and seed the heap geometry.
/// `obj` is re-read through `pin` (taken by the caller before the array
/// allocation), and `pin` is released on return.
pub(crate) fn install_byte_buffer_array(
    ctx: &mut dyn NativeContext,
    pin: usize,
    obj: ObjectRef,
    array: ObjectRef,
    capacity: usize,
) -> ObjectRef {
    let obj = ctx.read_native_pin(pin, obj);
    ctx.set_field(obj, BB_FIELD_ARRAY, Value::Object(Some(array)));
    // Real JDK Heap*Buffer backing array is named `hb`.
    ctx.set_field_by_name(obj, "hb", Value::Object(Some(array)));
    buf_write_metadata(ctx, obj, 0, capacity as i32, capacity as i32, -1);
    let obj = ctx.read_native_pin(pin, obj);
    // Real HeapByteBuffer.address is ARRAY_BYTE_BASE_OFFSET + offset. Bulk
    // copy bytecode relies on this value when ScopedMemoryAccess hands the
    // backing byte[] and offset to Unsafe.copyMemory. A fresh allocation's
    // array-base offset is 0, so this is `ARRAY_BYTE_BASE_OFFSET` — spelled
    // through [`heap_buffer_address`] so the literal `16` lives in one place.
    ctx.set_field_by_name(obj, "address", Value::Long(heap_buffer_address(0)));
    ctx.unpin_native_roots(pin);
    // W7-76's `bigEndian`/`nativeByteOrder` parity seed used to be transcribed
    // here in full. It moved to [`seed_buffer_byte_order`] (called by
    // `alloc_bb_object` at the top of both callers), which carries the whole
    // argument — including W7-76's own §8.2 note that a divergent copy of this
    // seed "is exactly what a shared helper would prevent". This lane built
    // that helper for the two files it owns.
    obj
}

/// Element-indexed storage view of a typed NIO buffer — `bb_storage_view`'s
/// twin for the `CharBuffer` / `IntBuffer` / `LongBuffer` / `FloatBuffer` /
/// `DoubleBuffer` / `ShortBuffer` families, **and the direct-buffer arm this
/// function spent its whole life without**.
///
/// ## What it used to be, and why that was a defect
///
/// It returned `(backing_array, pos, lim, cap)` and resolved the array as
/// `hb`-by-name → slot 5 → slot 0, with no fourth arm. A buffer that has no
/// backing array — every `ByteBuffer.allocateDirect` result, and every typed
/// view over one — therefore fell through to slot 0, which on the real
/// `java.nio.Buffer` layout is
///
/// ```text
///   mark(0) position(1) limit(2) capacity(3) address(4) segment(5)
///     then ByteBuffer's own:  hb(6) offset(7) isReadOnly(8) bigEndian(9) nativeByteOrder(10)
/// ```
///
/// i.e. `mark`, initialised to `-1`. That produced
/// `internal error: ByteBuffer missing backing array (field 0 returned Int(-1))`
/// — a CratonVM slot index applied to a real JDK object, landing on a field
/// with an unrelated meaning. Note in passing that the old comment's "real-JDK
/// HeapByteBuffer slot (`hb` @ 5)" was wrong twice over: on JDK 25 `hb` is
/// index 6 and index 5 is `Buffer.segment`. The slot-5 probe is kept anyway,
/// with its real justification: `segment` is the only Object-typed field
/// `Buffer` declares, so it is where `native-builtins`' typed-buffer views
/// (`s2_view_buf_fn!`, `BB_SEGMENT_SLOT`) deliberately stash their backing
/// array. Reading it as "maybe an array" is right for that population and was
/// claimed to be "harmless for a real buffer, where `segment` is null" — which
/// W7-83-segment-as-backing-array.md measured and falsified: `segment` is null
/// on `allocate` and `allocateDirect` receivers, and is a live
/// `NativeMemorySegmentImpl` on an `Arena…allocate(n).asByteBuffer()` one. The
/// arm now screens the value's KIND; see `bb_resolve_heap_array`.
///
/// ## What it is now
///
/// The same three-arm array resolution, then a **direct arm** reading
/// `address` (by name, else slot 4) exactly as `bb_storage_view` does, and
/// only then the refusal. The result is element-indexed: a heap receiver's
/// array already is, and a direct receiver's byte-addressed block is scaled by
/// [`TbView::elem`]'s width.
///
/// The return type changed from a tuple to `TbView` deliberately. This is the
/// second migration of this family — the first
/// (spring-bytebuffer-backing-storage-FIXED.md) moved the byte-oriented half
/// to `bb_storage_view` and left the rest behind, and a partial migration is
/// invisible until a non-heap buffer reaches one of the stragglers. A changed
/// return type means any site left behind fails the BUILD instead.
pub(crate) fn bb_state(ctx: &dyn NativeContext, this: ObjectRef) -> Result<TbView, MethodCallFailed> {
    let (pos, lim, cap) = buf_metadata(ctx, this);

    if let Some(arr) = bb_resolve_heap_array(ctx, this) {
        return Ok(TbView {
            storage: BbStorage::Heap {
                arr,
                offset: bb_resolve_heap_offset(ctx, this),
            },
            // The backing array knows its own element kind, so a HEAP receiver
            // never has to be asked what class it is stamped with.
            elem: ctx.heap_element_type_of(arr),
            big_endian: true, // unused: a heap element read is not byte-decoded
            pos,
            lim,
            cap,
        });
    }

    if let Some(addr) = bb_resolve_direct_address(ctx, this) {
        return Ok(TbView {
            storage: BbStorage::Direct { addr },
            elem: tb_direct_element_kind(ctx, this),
            big_endian: tb_receiver_is_big_endian(ctx, this),
            pos,
            lim,
            cap,
        });
    }

    Err(MethodCallFailed::InternalError(VmError::Internal {
        message: format!(
            "Buffer has no backing storage: hb/slot{BB_SEGMENT_SLOT}/slot{BB_FIELD_ARRAY} \
             resolved no array and address/slot{BB_ADDRESS_SLOT} no native block \
             (slot{BB_FIELD_ARRAY} returned {:?}, address {:?}) for object {this:?}",
            ctx.get_field(this, BB_FIELD_ARRAY),
            ctx.get_field_by_name(this, "address"),
        ),
    }))
}

/// Real `java.nio.Buffer.segment` — the only Object-typed field `Buffer`
/// declares, and therefore the slot `native-builtins`' typed-buffer views use
/// to carry a backing array that has no `hb` field to go in. Same constant as
/// `native-builtins/src/servlet.rs`'s `BB_SEGMENT_SLOT`, restated here rather
/// than shared because the two crates do not depend on each other; if one
/// moves, the census in W7-58-bytebuffer-direct-arm.md names both.
///
/// The slot carries TWO populations, which is why every read of it needs a kind
/// screen: `native-builtins`' views put a real backing array here, and a real
/// JDK 25 `Buffer` puts a `MemorySegment` here (non-null on every
/// `MemorySegment.asByteBuffer()` receiver). See
/// W7-83-segment-as-backing-array.md.
pub(crate) const BB_SEGMENT_SLOT: usize = 5;

/// `hb`-by-name → `segment` slot → synthetic slot 0. Extracted from the two
/// independent copies in `bb_state` and `bb_storage_view` so the two cannot
/// drift; they had already drifted in their error messages.
///
/// ## The slot-5 kind screen (W7-83-segment-as-backing-array.md)
///
/// The slot-5 arm is reached on any receiver whose `hb` is null, and on JDK 25
/// slot 5 is `java.nio.Buffer.segment` — a `MemorySegment`, not an array.
/// Measured on Eclipse Adoptium 25.0.3.9:
///
/// | receiver | `hb` | `segment` |
/// |---|---|---|
/// | `ByteBuffer.allocate(16)` | the array | `null` |
/// | `ByteBuffer.allocateDirect(16)` | `null` | `null` |
/// | `Arena.ofAuto().allocate(16).asByteBuffer()` | `null` | `jdk.internal.foreign.NativeMemorySegmentImpl` |
///
/// On that third receiver the arm used to return the `MemorySegment` and hand
/// it to `heap_element_type_of` / `get_array_element` as a backing array. That
/// is a wrong-KIND object where an array is required — a species the read-side
/// slot census has no vocabulary for, because the slot index is right and the
/// field name is right; only the value's kind is wrong.
///
/// Two things the screen must not disturb, both load-bearing:
///
/// * **The slot-5 read stays a non-firing census control.**
///   `read_alias::observe_read` is keyed on what slot 5 MEANS on the loaded
///   class, and it means `segment` — which is what this site declares. The
///   screen is applied to the VALUE after the read, so the census row is
///   unchanged and still expected clean (W7-69 §3).
/// * **`is_plausible_native_addr` is not touched.** Rejecting the segment lets
///   control reach `bb_resolve_direct_address`, and on an `Arena` buffer
///   `address` is a genuine process pointer, so the receiver resolves as
///   DIRECT — which is what it is. The `>= 0x1_0000` screen is what still stops
///   a heap buffer's `address = 16` from being dereferenced there
///   (`addr=0x10`, 51 of the 53 crashes in the 2026-08-10 H2 sweep).
///
/// The screen is on slot 5 ALONE, deliberately. `hb`-by-name resolves a field
/// the class declares as `byte[]`, so the VM's own typing covers it; slot 0 is
/// `Buffer.mark`, an `int`, already refused by the `Value::Object` match, and
/// is the backing array on the synthetic layout. Slot 5 is the only index in
/// this family that is a REFERENCE field of non-array type on a real receiver,
/// so it is the only one where an in-bounds, correctly-named read can still
/// yield the wrong kind.
pub(crate) fn bb_resolve_heap_array(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    if let Value::Object(Some(a)) = ctx.get_field_by_name(this, "hb") {
        return Some(a);
    }
    // W7-69, observation only. Slot 5 really is `Buffer.segment`, and this
    // probe is deliberate — see the constant's own doc — so the census is
    // expected to answer CLEAN here. That matters: an instrument that fires on
    // every slot read is a probe that cannot fail, and this is the arm that
    // proves it can stay quiet.
    if layout_alias::enabled() {
        read_alias::observe_read(
            ctx,
            this,
            BB_SEGMENT_SLOT,
            "segment",
            "native-io/src/lib.rs::bb_resolve_heap_array",
        );
    }
    if let Value::Object(Some(a)) = ctx.get_field(this, BB_SEGMENT_SLOT) {
        // W7-83. The census row above is about the SLOT; this is about the
        // VALUE. `native-builtins`' typed buffer views park a real backing
        // array here (there being no other Object-typed field on `Buffer`),
        // and a real JDK 25 receiver parks a `MemorySegment` here. Only the
        // first is a backing array, and only a kind question can tell them
        // apart — the class name cannot, because a heap reference array
        // reports its COMPONENT class.
        //
        // Falling through rather than returning `None` is deliberate: the
        // synthetic layout's array lives at slot 0, and a receiver that has
        // both a segment at 5 and an array at 0 must still resolve.
        if ctx.heap_kind_of(a) == ObjectKind::Array {
            return Some(a);
        }
    }
    // THE CALIBRATION CASE. This is the read W7-58-bytebuffer-direct-arm.md
    // repaired: slot 0 of a real java.nio.DirectByteBuffer is
    // `java.nio.Buffer.mark` = -1, not the backing array `hb` (which is 6).
    // The read is perfectly IN BOUNDS, so the gc::guard out-of-bounds list
    // never saw it and the allocation-width census had no vocabulary for it.
    if layout_alias::enabled() {
        read_alias::observe_read(
            ctx,
            this,
            BB_FIELD_ARRAY,
            "hb",
            "native-io/src/lib.rs::bb_resolve_heap_array",
        );
    }
    match ctx.get_field(this, BB_FIELD_ARRAY) {
        Value::Object(Some(a)) => Some(a),
        _ => None,
    }
}

/// `ByteBuffer.offset` — the element index of logical element 0 inside the
/// backing array, non-zero only for an aliasing view. Layouts with no such
/// field (bare synthetics, typed views) answer 0.
pub(crate) fn bb_resolve_heap_offset(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    if let Value::Int(v) = ctx.get_field_by_name(this, "offset") {
        if v >= 0 {
            return v as usize;
        }
    }
    // W7-69, observation only. On JDK 25 slot 6 is `ByteBuffer.hb` — the
    // backing ARRAY — and `offset` is 7. So this fallback names a reference
    // field and reads it as an `Int`; the `Value::Int` match is the only thing
    // between it and a fabricated offset.
    //
    // W7-76 named the literal and settled the verdict rather than leaving it
    // to "a different lane": the arm is right on no layout and reachable on
    // none, and is neither renumbered nor deleted. Both refusals are argued at
    // `BB_HEAP_OFFSET_FALLBACK_SLOT`.
    if layout_alias::enabled() {
        read_alias::observe_read(
            ctx,
            this,
            BB_HEAP_OFFSET_FALLBACK_SLOT,
            "offset",
            "native-io/src/lib.rs::bb_resolve_heap_offset",
        );
    }
    match ctx.get_field(this, BB_HEAP_OFFSET_FALLBACK_SLOT) {
        Value::Int(v) if v >= 0 => v as usize,
        _ => 0,
    }
}

/// A `Buffer.address` below the first mappable page is never a process
/// pointer: Linux refuses to map below `vm.mmap_min_addr` (65536 by default)
/// and Windows reserves the low 64 KiB of every address space. What DOES live
/// down there is an array-relative `Unsafe` offset — `ARRAY_BYTE_BASE_OFFSET +
/// offset` — which is exactly what `alloc_byte_buffer` seeds into every HEAP
/// buffer it mints (`Value::Long(16)`), and what the real `HeapByteBuffer`
/// constructor seeds too.
///
/// So this is not defensive padding. Without it, any heap buffer whose array
/// failed to resolve — the precise condition that brings control here — hands
/// `16` to `copy_from_native_memory` as a pointer. `addr=0x10` was 51 of the
/// 53 crashes in the 2026-08-10 three-GC-variant H2 sweep; refusing the read
/// degrades instead to this module's "no backing storage" error, which a
/// caller can see and report. `native-builtins/src/servlet.rs`'s
/// `is_plausible_native_addr` is the same guard for the same reason; this
/// family did not have it.
pub(crate) fn is_plausible_native_addr(v: i64) -> bool {
    v >= 0x1_0000
}

/// `address`-by-name → slot 4, screened by [`is_plausible_native_addr`].
pub(crate) fn bb_resolve_direct_address(ctx: &dyn NativeContext, this: ObjectRef) -> Option<i64> {
    if let Value::Long(v) = ctx.get_field_by_name(this, "address") {
        if is_plausible_native_addr(v) {
            return Some(v);
        }
    }
    // W7-69, observation only. Slot 4 on the REAL layout is `address`, so this
    // fallback is right — and W7-76 stopped it being spelled with a constant
    // whose name denies that: it reads [`BB_ADDRESS_SLOT`], which is the same
    // index under the name of what is there. The census answers CLEAN here,
    // and that is the second non-firing control: the instrument is keyed on
    // what the slot MEANS, not on what the constant is called.
    //
    // This is also the ONE numeric fallback in this family a real receiver
    // genuinely reaches. A real `HeapByteBuffer` carries
    // `address = ARRAY_BYTE_BASE_OFFSET = 16`; `is_plausible_native_addr`
    // refuses it above, control arrives here, and slot 4 hands back the same
    // `Long(16)`, which is refused again. Correct on both passes, and the
    // refusal is what keeps `addr=0x10` out of `copy_from_native_memory`.
    if layout_alias::enabled() {
        read_alias::observe_read(
            ctx,
            this,
            BB_ADDRESS_SLOT,
            "address",
            "native-io/src/lib.rs::bb_resolve_direct_address",
        );
    }
    match ctx.get_field(this, BB_ADDRESS_SLOT) {
        Value::Long(v) if is_plausible_native_addr(v) => Some(v),
        _ => None,
    }
}

/// Element kind of a DIRECT typed buffer.
///
/// Only a direct receiver needs this — a heap one is asked through
/// `heap_element_type_of`, which is the object's own truth. A direct block is
/// untyped bytes, so the element width has to come from somewhere else, and
/// the JDK itself puts it in the class: `DirectIntBufferU`,
/// `ByteBufferAsIntBufferL`, `HeapIntBuffer`. This reads the receiver's class
/// to decide how wide its elements are — it does NOT pick which native to run
/// by name, which is the defect this codebase keeps re-finding.
///
/// Unknown names answer `Byte` (width 1), which is what a bare
/// `java/nio/ByteBuffer` receiver is and the only safe default: it can never
/// scale an index past the end of a block that a wider guess would.
pub(crate) fn tb_direct_element_kind(ctx: &dyn NativeContext, this: ObjectRef) -> ArrayElementType {
    let cid = ctx.class_id_of_object(this);
    let name = ctx.class_name_of_id(cid).unwrap_or_default();
    if name.contains("CharBuffer") {
        ArrayElementType::Char
    } else if name.contains("ShortBuffer") {
        ArrayElementType::Short
    } else if name.contains("IntBuffer") {
        ArrayElementType::Int
    } else if name.contains("LongBuffer") {
        ArrayElementType::Long
    } else if name.contains("FloatBuffer") {
        ArrayElementType::Float
    } else if name.contains("DoubleBuffer") {
        ArrayElementType::Double
    } else {
        ArrayElementType::Byte
    }
}

/// Byte order of a DIRECT typed buffer's elements.
///
/// Same source the already-registered `order()` native for these classes uses
/// (`tb_abstract_view_fns!`'s `$order_fn`): real JDK 25 compiles one concrete
/// view class per endianness — `ByteBufferAsIntBufferB` / `...L`,
/// `DirectIntBufferU` (native order) — so the class name IS where the order
/// lives for a view buffer, there being no field to hold it. Factored out so
/// the accessor and the reported `order()` cannot disagree; before the direct
/// arm existed there was no accessor that could.
///
/// Every CratonVM target is little-endian, so the unsuffixed "native order"
/// case answers little-endian.
pub(crate) fn tb_receiver_is_big_endian(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let cid = ctx.class_id_of_object(this);
    let name = ctx.class_name_of_id(cid).unwrap_or_default();
    if name.ends_with('B') || name.ends_with("RB") {
        return true;
    }
    if name.ends_with('L') || name.ends_with("RL") {
        return false;
    }
    cfg!(target_endian = "big")
}

#[derive(Clone, Copy)]
pub(crate) enum BbStorage {
    Heap { arr: ObjectRef, offset: usize },
    Direct { addr: i64 },
}

#[derive(Clone, Copy)]
pub(crate) struct BbView {
    pub(crate) storage: BbStorage,
    pub(crate) pos: i32,
    pub(crate) lim: i32,
    pub(crate) cap: i32,
}

/// The THREE states `hasArray()` / `array()` / `arrayOffset()` distinguish —
/// and the reason none of them can be written as one "is the array
/// accessible?" branch.
///
/// JDK 25, `java.base/java/nio/ByteBuffer.java` L1490 / L1513 / L1541, and the
/// byte-for-byte identical bodies in `CharBuffer.java` at the SAME three line
/// numbers (one contract, generated per element type from
/// `X-Buffer.java.template`):
///
/// ```java
/// public final boolean hasArray() { return (hb != null) && !isReadOnly; }
///
/// public final byte[] array() {
///     if (hb == null)  throw new UnsupportedOperationException();
///     if (isReadOnly)  throw new ReadOnlyBufferException();
///     return hb;
/// }
///
/// public final int arrayOffset() {          // the identical split
///     if (hb == null)  throw new UnsupportedOperationException();
///     if (isReadOnly)  throw new ReadOnlyBufferException();
///     return offset;
/// }
/// ```
///
/// So `hasArray() == false` covers **two** states, and the exception classes
/// are NOT interchangeable to a caller that reads the class name. MEASURED on
/// `openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)` via
/// `probes/BufferAccessibleArrayProbe` and this lane's supplement (the probe
/// covers byte/char/int; long/float/double/short were measured separately and
/// answer identically, which is what "generated from one template" predicts):
///
/// | receiver | `hasArray()` | `array()` / `arrayOffset()` |
/// |---|---|---|
/// | `HeapByteBuffer` (`allocate(8)`) | `true` | the array / `0` |
/// | `HeapByteBufferR` (`.asReadOnlyBuffer()`) | `false` | `ReadOnlyBufferException` |
/// | `DirectByteBuffer` (`allocateDirect(8)`) | `false` | `UnsupportedOperationException` |
/// | `DirectByteBufferR` — read-only AND array-less | `false` | `UnsupportedOperationException` |
/// | `ByteBufferAsIntBufferB` — WRITABLE and array-less | `false` | `UnsupportedOperationException` |
/// | `HeapLongBufferR` / `HeapFloatBufferR` / `HeapDoubleBufferR` / `HeapShortBufferR` | `false` | `ReadOnlyBufferException` |
///
/// The last two rows are why this is not a "read-only versus not" test: a
/// writable view answers `UnsupportedOperation`, and a read-only heap buffer
/// answers `ReadOnlyBuffer`. The ORDER is load-bearing and is why this is a
/// function rather than two booleans repeated at each site: `DirectByteBufferR`
/// is read-only AND array-less at once and the JDK asks "array-less?" FIRST, so
/// it answers `UnsupportedOperation`. A read-only-first implementation gets
/// that one cell wrong and every other cell right.
///
/// **A `catch` cannot recover the distinction.** MEASURED, same JDK:
/// `java.nio.ReadOnlyBufferException.class.getSuperclass()` is
/// `java.lang.UnsupportedOperationException` — ROBE *is* a UOE (JDK 25
/// `java.base/java/nio/ReadOnlyBufferException.java:40`). So a subtype-shaped
/// assertion passes when the two are swapped in one direction and fails in the
/// other; only an exact-class comparison discriminates. (The F5-1 record says
/// the two "share no supertype below `RuntimeException`" — that is wrong, and
/// wrong in the direction that makes a test look stricter than it is.)
///
/// Transcribed from `native-builtins/src/phases_late/charset_buffers.rs`'s
/// `cb_array_access`, which a sibling lane wrote for the CharBuffer half of the
/// same contract. It is not `use`d from there because the dependency runs the
/// other way — `cratonvm-native-builtins` depends on `cratonvm-native-io`, not
/// the reverse — so this copy is `pub` and is the one the other two registrars
/// can converge onto without a new crate. See the record's NOMINATIONS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferArrayAccess {
    /// Array-backed and writable: hand the array over.
    Accessible,
    /// Array-backed but read-only: `java.nio.ReadOnlyBufferException`.
    ReadOnly,
    /// No backing array at all (direct, or a typed view): raise
    /// `java.lang.UnsupportedOperationException`.
    Absent,
}

/// [`BufferArrayAccess`] from the two inputs the JDK bodies read, in the JDK's
/// order. Split out from the four natives that need it so the ordering has one
/// definition and one test instead of four transcriptions that can drift — the
/// way the ByteBuffer and CharBuffer halves of this family already did.
#[inline]
pub fn buffer_array_access(has_hb: bool, read_only: bool) -> BufferArrayAccess {
    if !has_hb {
        // FIRST, per the JDK body. Not `else if`-after-read-only.
        BufferArrayAccess::Absent
    } else if read_only {
        BufferArrayAccess::ReadOnly
    } else {
        BufferArrayAccess::Accessible
    }
}

/// True when the buffer's `isReadOnly` FIELD is set.
///
/// The field, not the `isReadOnly()` METHOD, because that is what the JDK's own
/// `hasArray`/`array`/`arrayOffset` bodies read. The two can legitimately
/// disagree: `ByteBufferAsIntBufferRB` &c. override the method to return `true`
/// and never write the field. That is not a hazard here — those classes carry
/// no `hb` either, so the `Absent` arm answers first and this is never
/// consulted (MEASURED: `ByteBuffer.allocate(8).asReadOnlyBuffer().asIntBuffer()`
/// answers `isReadOnly() == true` and `array()` throws
/// **UnsupportedOperationException**, not `ReadOnlyBufferException`).
///
/// Byte-identical in shape to `native-builtins/src/servlet.rs`'s
/// `s2_bb_is_read_only` and `charset_buffers.rs`'s `cb_is_read_only`, so the
/// three cannot drift. A layout with no such field answers a non-`Int` and
/// lands on `false`, which is right for every buffer this crate mints: the only
/// writer is the typed `$ro_fn` macro arm, which sets `isReadOnly = 1` by name.
///
/// `pub` for the same reason [`buffer_array_access`] is: F14-1's N1 named this
/// as one of THREE byte-identical transcriptions
/// (`servlet.rs::s2_bb_is_read_only`, `charset_buffers.rs::cb_is_read_only`,
/// this) and `cratonvm-native-builtins` depends on `cratonvm-native-io` and not
/// the reverse, so this is the only one of the three the other two can import.
/// The other two are now one-line delegations to it and the set cannot drift.
///
/// Interpreter round i5 wave 105, lane L5: a `--synthetic-jdk` stub buffer
/// (the fabricated 10-slot `java/nio/ByteBuffer`, no `isReadOnly` field, so
/// the by-name read answers `Object(None)`) keeps the flag in
/// [`BB_STUB_READ_ONLY_SLOT`] ([`write_buffer_read_only`]). Before, a stub's
/// `asReadOnlyBuffer()` answered `isReadOnly() == false` and `hasArray() ==
/// true`, so `ClassLoader.defineClass(String, ByteBuffer, ...)` took the
/// array branch HotSpot does not take for it (probe
/// `L5W101DefineClassByteBufferSynthetic`, row `heap-readonly`).
#[inline]
pub fn buffer_is_read_only(ctx: &dyn NativeContext, buf: ObjectRef) -> bool {
    match ctx.get_field_by_name(buf, "isReadOnly") {
        Value::Int(v) => v != 0,
        Value::Object(None) if ctx.object_num_fields(buf) > BB_STUB_READ_ONLY_SLOT => {
            matches!(ctx.get_field(buf, BB_STUB_READ_ONLY_SLOT), Value::Int(v) if v != 0)
        }
        _ => false,
    }
}

/// The slot a `--synthetic-jdk` stub buffer keeps its read-only flag in: the
/// fabricated `java/nio/Buffer`'s third field (`_f2`). The stub's slots 0..5
/// are the indexed `BB_*` layout, 6 and 7 are `native-builtins`'
/// `BB_NATIVE_ID` / `BB_DIRECT_FLAG` (and this crate's offset fallback), so 8
/// is the first slot nothing else names. Only used when the receiver has no
/// `isReadOnly` field and is wider than this index.
pub const BB_STUB_READ_ONLY_SLOT: usize = 8;

/// The slot a `--synthetic-jdk` stub heap view (the fabricated 10-slot
/// `java/nio/ByteBuffer`, no `offset` field) keeps its array-base offset in:
/// the slot [`bb_resolve_heap_offset`]'s by-index fallback already reads
/// (`BB_HEAP_OFFSET_FALLBACK_SLOT`, the fabricated `Buffer._f0`). Interpreter
/// round i5 wave 106, lane L5: `servlet.rs`'s `s2_bb_new_heap_view` writes an
/// `Int` here for a stub view at a non-zero base (`slice()` of a positioned
/// stub buffer), and `s2_bb_heap_base` reads it back. Before, such a view had
/// no storage at all and its first `get` raised `internal error: ByteBuffer
/// missing backing storage`. Nothing else writes the slot on a stub (the
/// six-slot layout's `BB_NATIVE_ID` constant is declared but never used), and
/// on a real layout the by-name `offset` read answers first.
pub const BB_STUB_HEAP_OFFSET_SLOT: usize = BB_HEAP_OFFSET_FALLBACK_SLOT;

/// Write a buffer's read-only flag: by name, and, on a `--synthetic-jdk` stub
/// with no `isReadOnly` field, into [`BB_STUB_READ_ONLY_SLOT`] (see
/// [`buffer_is_read_only`]).
pub fn write_buffer_read_only(ctx: &mut dyn NativeContext, buf: ObjectRef, read_only: bool) {
    ctx.set_field_by_name(buf, "isReadOnly", Value::Int(i32::from(read_only)));
    if matches!(ctx.get_field_by_name(buf, "isReadOnly"), Value::Object(None))
        && ctx.object_num_fields(buf) > BB_STUB_READ_ONLY_SLOT
    {
        ctx.set_field(buf, BB_STUB_READ_ONLY_SLOT, Value::Int(i32::from(read_only)));
    }
}

/// Which of the two shapes a view-producing buffer method has, for the purpose
/// of the derived buffer's `isReadOnly` flag.
///
/// **Read-only-ness is CONTAGIOUS in the JDK and `asReadOnlyBuffer()` is
/// one-way.** Both halves MEASURED on `openjdk 25.0.3 2026-04-21 LTS
/// (25.0.3+9-LTS)`, all seven buffer families (byte/char/int/long/float/
/// double/short), `scratchpad/f21/F21ViewContagionProbe.java`:
///
/// ```text
/// <fam>.ro.dup.isReadOnly        OK true      <- duplicate()
/// <fam>.ro.slice.isReadOnly      OK true      <- slice()
/// <fam>.ro.slice(1,2).isReadOnly OK true      <- slice(int,int)
/// <fam>.ro.aro.isReadOnly        OK true      <- asReadOnlyBuffer()
/// <fam>.w.dup.isReadOnly         OK false     <- and NOT contagious upward
/// <fam>.w.slice.isReadOnly       OK false
/// <fam>.w.aro.isReadOnly         OK true      <- unconditional
/// <fam>.ro.dup.aro.isReadOnly    OK true      <- no route back to writable
/// ```
///
/// The six typed families' transcripts are byte-identical to one another and
/// differ from `byte`'s in exactly one column (`order`, which is `BIG_ENDIAN`
/// for ByteBuffer and `ByteOrder.nativeOrder()` for the typed heap families) —
/// verified by mechanical diff, not by the "one template" argument.
///
/// The JDK also has no operation anywhere that clears the flag: there is no
/// `asWritableBuffer`, and `duplicate()` of a read-only buffer re-enters
/// `HeapByteBufferR`'s constructor with `isReadOnly = true` again. So `Inherit`
/// really is inheritance and not a copy that some other path can undo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferDerivation {
    /// `duplicate()`, `slice()`, `slice(int,int)`: the derived buffer's
    /// read-only-ness is exactly the source's.
    Inherit,
    /// `asReadOnlyBuffer()`: read-only whatever the source was.
    ForceReadOnly,
}

/// The derived buffer's `isReadOnly` flag, from the derivation shape and the
/// source's flag.
///
/// A function rather than two `if`s at each of the eleven view-producing
/// natives in this file, for the same reason [`buffer_array_access`] is one:
/// eleven transcriptions of a two-line rule is how [`buffer_array_access`]'s
/// own contract came to exist in three drifted copies.
///
/// **The severity, and why this is not merely a wrong flag.** F14-1 closed the
/// hole where `hasArray()` → `array()` handed a caller a mutable alias to
/// read-only storage. Without this propagation the same capability re-opens one
/// call later: `readOnlyBuffer.duplicate()` came back with `isReadOnly` unset,
/// so `hasArray()` answered `true` again, `array()` handed the array over
/// again, and every write through it corrupted the read-only buffer with no
/// exception anywhere — reached by following the documented protocol, on a
/// receiver the caller obtained from a read-only one.
#[inline]
pub fn buffer_view_read_only(derivation: BufferDerivation, source_read_only: bool) -> bool {
    match derivation {
        BufferDerivation::Inherit => source_read_only,
        BufferDerivation::ForceReadOnly => true,
    }
}

/// Stamp `derived` — a buffer just minted from `src` by `derivation` — with the
/// `isReadOnly` flag [`buffer_view_read_only`] says it must have.
///
/// Writes the flag on BOTH outcomes rather than only when it is set. Neither
/// [`alloc_byte_buffer`] nor [`alloc_typed_buffer`] writes `isReadOnly` at all,
/// so an unwritten flag is whatever the allocation left in the slot; the
/// explicit `0` is what makes "writable source ⇒ writable view" a decision this
/// file makes rather than a default it inherits.
#[inline]
pub(crate) fn buf_stamp_read_only(
    ctx: &mut dyn NativeContext,
    src: ObjectRef,
    derived: ObjectRef,
    derivation: BufferDerivation,
) {
    // The read is BOUND before the call rather than nested inside it, and that
    // is a borrowck requirement, not a style choice: `buf_write_read_only`
    // takes `ctx` as `&mut dyn` and `buffer_is_read_only` takes it as `&dyn`,
    // so as sibling arguments to one expression the two borrows overlap
    // (E0502). Rust already evaluates arguments left to right, so this binding
    // does not MOVE the read — it happened before the callee's body either
    // way; it only ends the shared borrow before the mutable one starts.
    let src_read_only = buffer_is_read_only(ctx, src);
    buf_write_read_only(ctx, derived, derivation, src_read_only);
}

/// [`buf_stamp_read_only`] for a caller that has ALREADY read the source's
/// flag — the two arguments in the other order, one of them a `bool` instead
/// of an `ObjectRef`.
///
/// Not a convenience. `slice()` and `duplicate()` allocate between reading the
/// source and stamping the result, and an allocation can trigger a collection
/// that RELOCATES the source; passing the `ObjectRef` across that gap and
/// dereferencing it afterwards is the hazard `alloc_byte_buffer_over`'s pin
/// exists for. Reading the flag first turns the thing carried across the
/// allocation into a `bool`, which no collector can move — cheaper than a pin
/// and impossible to get wrong.
#[inline]
pub(crate) fn buf_write_read_only(
    ctx: &mut dyn NativeContext,
    derived: ObjectRef,
    derivation: BufferDerivation,
    source_read_only: bool,
) {
    let ro = buffer_view_read_only(derivation, source_read_only);
    write_buffer_read_only(ctx, derived, ro);
}

/// `throw new UnsupportedOperationException()` — the NO-ARGUMENT constructor,
/// so `getMessage()` is null exactly as HotSpot's is (MEASURED: all four of
/// `DirectByteBuffer.array()`, `DirectByteBuffer.arrayOffset()`,
/// `asCharBuffer().array()` and `CharBuffer.wrap(String).array()` report
/// `getMessage() == null`). The EMPTY string is the documented spelling for
/// that in `types/src/error.rs`, which maps `""` to `None` so the `()V` ctor is
/// used; `Some("")` would set a non-null empty detail message.
///
/// This replaced a `"ByteBuffer has no backing array"` detail message, which
/// was the same defect this lane fixes in `servlet.rs` (record N3).
///
/// `pub` for the same reason [`buffer_array_access`] and
/// [`buffer_is_read_only`] are: `servlet.rs::s2_bb_no_backing_array` and
/// `charset_buffers.rs::cb_no_backing_array` were the other two copies of this
/// three-line body, and only this one is importable by the other two. Both are
/// now one-line delegations. The EMPTY-string spelling is the whole point of
/// sharing it — a `Some("")` here would give `getMessage()` a non-null empty
/// string, and a test written as `matches!(.., UnsupportedOperationException { .. })`
/// is blind to that by construction, which is how the wrong message survived a
/// prior repair of this same family.
#[inline]
pub fn buffer_no_backing_array() -> MethodCallFailed {
    RuntimeError::UnsupportedOperationException {
        message: String::new(),
    }
    .into()
}

/// Refuse a WRITE through a read-only buffer — the guard every `put*` /
/// `compact` native in this file must run FIRST.
///
/// **This is the most severe defect class this codebase triages: a wrong
/// CAPABILITY, silent.** Before this, `ByteBuffer.allocate(8)
/// .asReadOnlyBuffer().put(0, (byte) 1)` SUCCEEDED and the byte landed in the
/// source's backing array — with no exception at any point, reached by
/// following the documented protocol on a receiver whose entire contract is
/// that it cannot be written. F14-1 closed the `array()` route to the same
/// storage and F21-1 closed the `duplicate()`-of-a-read-only route; the direct
/// route, `put` itself, was never checked by anything in this crate.
///
/// MEASURED on `openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)`
/// (`scratchpad/f37/F37TypedAliasProbe.java`, 691 rows). Every write path of
/// every family answers `java.nio.ReadOnlyBufferException` with a **null**
/// detail message, on every read-only receiver shape:
///
/// ```text
/// bb.ro.put(b) / put(i,b) / put(byte[]) / put(byte[],0,2) / put(ByteBuffer)
/// bb.ro.putInt / putInt(0,i) / putShort / putChar / putLong / putFloat / putDouble
/// bb.ro.compact()
/// bb.ro.duplicate().put(b)          <- and through every derived buffer
/// bb.ro.slice().put(b)
/// bb.roDirect.put(b)                <- DirectByteBufferR, same class
/// <fam>.ro.put(x) / put(i,x) / put(arr) / put(arr,0,2) / put(buf) / compact()
/// <fam>.roView.put(x) …             <- ByteBufferAs<T>BufferRB
/// <fam>.roDirectView.put(x)         <- Direct<T>BufferRS
/// Char.ro.put(String) / append(CharSequence)
/// CharBuffer.wrap(String).put(c)    <- StringCharBuffer, isReadOnly() true
/// ```
///
/// **`ReadOnlyBufferException`, NOT `UnsupportedOperationException`, and the
/// distinction is measured rather than reasoned.** [`buffer_array_access`]'s
/// contract is that the ARRAY-LESS check runs first, so `array()` /
/// `arrayOffset()` on a `ByteBufferAsIntBufferRB` answer
/// `UnsupportedOperationException` even though the receiver is read-only. That
/// ordering is specific to those two accessors: JDK 25's
/// `ByteBufferAsIntBufferRB.put(int)` is a bare `throw new
/// ReadOnlyBufferException();` (`jdk25src/java.base/java/nio/
/// ByteBufferAsIntBufferRB.java:166`), and so is `HeapIntBufferR`'s, and the
/// transcript above confirms it for the array-LESS read-only view too. So the
/// answer here is `ReadOnlyBufferException` on every shape and there is no
/// second arm to write.
///
/// `ReadOnlyBufferException` **extends** `UnsupportedOperationException`
/// (`jdk25src/java.base/java/nio/ReadOnlyBufferException.java:40`; MEASURED
/// `ROBE.getSuperclass() == java.lang.UnsupportedOperationException`,
/// `ROBE instanceof UOE == true`, `UOE instanceof ROBE == false`), so an
/// `instanceof`-shaped or `matches!`-shaped assertion on this pair
/// discriminates in ONE direction only and would let a
/// `buffer_no_backing_array` mutant live. Every test for this compares an
/// exact class name.
///
/// **The negative control is the reason this reads the FIELD.** A writable
/// buffer must still write: `bb.w.put(b)` and `bb.w.compact()` both succeed on
/// HotSpot (MEASURED), and `alloc_byte_buffer` / `alloc_typed_buffer` never
/// write `isReadOnly` at all, so a freshly minted buffer's flag is the
/// allocation's zero and this guard is inert for it. The only writers of a `1`
/// are `buf_write_read_only`'s `ForceReadOnly` arm and the `Inherit` arm on a
/// read-only source — i.e. the flag is set exactly when the JDK sets it.
#[inline]
pub fn buffer_check_writable(
    ctx: &dyn NativeContext,
    buf: ObjectRef,
) -> Result<(), MethodCallFailed> {
    if buffer_is_read_only(ctx, buf) {
        // `RuntimeError::ReadOnlyBufferException` is a unit variant that maps
        // to the no-argument constructor, so `getMessage()` is null exactly as
        // HotSpot's is on all 40+ ROBE rows measured. Spelling a detail
        // message here would be the same defect `buffer_no_backing_array`
        // records ("direct buffer has no backing array" surviving a repair).
        return Err(RuntimeError::ReadOnlyBufferException.into());
    }
    Ok(())
}

/// Element-indexed view returned by [`bb_state`]: `pos`/`lim`/`cap` are in
/// ELEMENTS (as every typed-buffer accessor uses them), and `storage` is
/// whichever of the two kinds the receiver actually has.
#[derive(Clone, Copy)]
pub(crate) struct TbView {
    pub(crate) storage: BbStorage,
    /// Element kind, so a DIRECT read knows both how many bytes to move and
    /// which `Value` variant to produce. Read from the backing array for a
    /// heap receiver; from the receiver's class for a direct one.
    pub(crate) elem: ArrayElementType,
    /// Element byte order — consulted only on the direct arm. A heap element
    /// read goes through `get_array_element`, which is not byte-decoded.
    pub(crate) big_endian: bool,
    pub(crate) pos: i32,
    pub(crate) lim: i32,
    pub(crate) cap: i32,
}

/// Bytes per element of `elem`. `Reference` cannot occur in a typed NIO buffer
/// and answers 1 rather than panicking; a wrong-but-narrow scale mis-reads,
/// where a panic takes the VM down.
pub(crate) fn tb_elem_width(elem: ArrayElementType) -> usize {
    match elem {
        ArrayElementType::Boolean | ArrayElementType::Byte | ArrayElementType::Reference => 1,
        ArrayElementType::Char | ArrayElementType::Short => 2,
        ArrayElementType::Int | ArrayElementType::Float => 4,
        ArrayElementType::Long | ArrayElementType::Double => 8,
    }
}

/// Read element `index` (an ELEMENT index, not a byte offset) from either
/// storage kind, in the `Value` variant the element's kind calls for — the
/// same variant `ctx.get_array_element` yields for a heap array of that kind,
/// so a caller cannot tell the two arms apart.
pub(crate) fn tb_read_elem(
    ctx: &dyn NativeContext,
    view: TbView,
    index: usize,
) -> Result<Value, MethodCallFailed> {
    match view.storage {
        BbStorage::Heap { arr, offset } => Ok(ctx.get_array_element(arr, offset + index)),
        BbStorage::Direct { addr } => {
            let width = tb_elem_width(view.elem);
            let mut raw = [0u8; 8];
            let byte_off = (index as i64).saturating_mul(width as i64);
            if !ctx.copy_from_native_memory(addr.saturating_add(byte_off), &mut raw[..width]) {
                return Err(MethodCallFailed::InternalError(VmError::Internal {
                    message: format!(
                        "Buffer direct element read failed at address 0x{:x} (+{} bytes)",
                        addr, byte_off
                    ),
                }));
            }
            let bits: u64 = if view.big_endian {
                raw[..width].iter().fold(0u64, |a, &b| (a << 8) | b as u64)
            } else {
                raw[..width]
                    .iter()
                    .rev()
                    .fold(0u64, |a, &b| (a << 8) | b as u64)
            };
            Ok(match view.elem {
                ArrayElementType::Long => Value::Long(bits as i64),
                ArrayElementType::Double => Value::Double(f64::from_bits(bits)),
                ArrayElementType::Float => Value::Float(f32::from_bits(bits as u32)),
                // Char is UNSIGNED and Short is SIGNED — the two differ only
                // here, and getting it wrong is invisible until a value with
                // the high bit set goes through.
                ArrayElementType::Char => Value::Int(bits as u16 as i32),
                ArrayElementType::Short => Value::Int(bits as u16 as i16 as i32),
                ArrayElementType::Int => Value::Int(bits as u32 as i32),
                _ => Value::Int(bits as u8 as i8 as i32),
            })
        }
    }
}

/// Write element `index` through either storage kind. The inverse of
/// [`tb_read_elem`], and deliberately tolerant of the `Value` variant a caller
/// hands over (`Int` where a `Float` was expected, etc.) because the typed
/// natives forward whatever the interpreter pushed.
pub(crate) fn tb_write_elem(
    ctx: &mut dyn NativeContext,
    view: TbView,
    index: usize,
    value: Value,
) -> Result<(), MethodCallFailed> {
    match view.storage {
        BbStorage::Heap { arr, offset } => {
            ctx.set_array_element(arr, offset + index, value);
            Ok(())
        }
        BbStorage::Direct { addr } => {
            let width = tb_elem_width(view.elem);
            let bits: u64 = match value {
                Value::Long(v) => v as u64,
                Value::Double(v) => v.to_bits(),
                Value::Float(v) => v.to_bits() as u64,
                Value::Int(v) => v as u32 as u64,
                _ => 0,
            };
            let mut raw = [0u8; 8];
            for i in 0..width {
                let shift = if view.big_endian {
                    8 * (width - 1 - i)
                } else {
                    8 * i
                };
                raw[i] = (bits >> shift) as u8;
            }
            let byte_off = (index as i64).saturating_mul(width as i64);
            if ctx.copy_to_native_memory(addr.saturating_add(byte_off), &raw[..width]) {
                Ok(())
            } else {
                Err(MethodCallFailed::InternalError(VmError::Internal {
                    message: format!(
                        "Buffer direct element write failed at address 0x{:x} (+{} bytes)",
                        addr, byte_off
                    ),
                }))
            }
        }
    }
}

/// Byte-indexed storage view — the `ByteBuffer` half of the same resolution
/// [`bb_state`] does for the typed families. Both now share
/// `bb_resolve_heap_array` / `bb_resolve_heap_offset` /
/// `bb_resolve_direct_address` rather than each carrying its own copy; the two
/// copies had already drifted apart in their error text, and one of them
/// (`bb_state`'s) had no direct arm at all.
///
/// The one behaviour change here is the address screen: this used to accept
/// any non-zero `address`, including the `16` that `alloc_byte_buffer` seeds
/// into every HEAP buffer as `ARRAY_BYTE_BASE_OFFSET`. See
/// [`is_plausible_native_addr`] for why dereferencing that is not a wrong
/// answer but a SIGSEGV.
pub(crate) fn bb_storage_view(ctx: &dyn NativeContext, this: ObjectRef) -> Result<BbView, MethodCallFailed> {
    let pos = buf_read_position(ctx, this).max(0);
    let lim = buf_read_limit(ctx, this).max(pos);
    let cap = match buf_read_capacity(ctx, this) {
        0 => lim,
        v => v.max(0),
    };

    if let Some(arr) = bb_resolve_heap_array(ctx, this) {
        return Ok(BbView {
            storage: BbStorage::Heap {
                arr,
                offset: bb_resolve_heap_offset(ctx, this),
            },
            pos,
            lim,
            cap,
        });
    }

    let Some(addr) = bb_resolve_direct_address(ctx, this) else {
        return Err(MethodCallFailed::InternalError(VmError::Internal {
            message: format!(
                "ByteBuffer missing backing storage (hb/slot{BB_SEGMENT_SLOT}/address absent; \
                 field {BB_FIELD_ARRAY} returned {:?}, address {:?}) for object {this:?}",
                ctx.get_field(this, BB_FIELD_ARRAY),
                ctx.get_field_by_name(this, "address"),
            ),
        }));
    };
    Ok(BbView {
        storage: BbStorage::Direct { addr },
        pos,
        lim,
        cap,
    })
}

pub(crate) fn bb_read_byte(
    ctx: &dyn NativeContext,
    view: BbView,
    index: usize,
) -> Result<u8, MethodCallFailed> {
    match view.storage {
        BbStorage::Heap { arr, offset } => Ok(match ctx.get_array_element(arr, offset + index) {
            Value::Int(v) => v as u8,
            _ => 0,
        }),
        BbStorage::Direct { addr } => {
            let mut b = [0u8; 1];
            if ctx.copy_from_native_memory(addr.saturating_add(index as i64), &mut b) {
                Ok(b[0])
            } else {
                Err(MethodCallFailed::InternalError(VmError::Internal {
                    message: format!("ByteBuffer direct read failed at address 0x{:x}", addr),
                }))
            }
        }
    }
}

pub(crate) fn bb_write_byte(
    ctx: &mut dyn NativeContext,
    view: BbView,
    index: usize,
    byte: u8,
) -> Result<(), MethodCallFailed> {
    match view.storage {
        BbStorage::Heap { arr, offset } => {
            ctx.set_array_element(arr, offset + index, Value::Int(byte as i8 as i32));
            Ok(())
        }
        BbStorage::Direct { addr } => {
            if ctx.copy_to_native_memory(addr.saturating_add(index as i64), &[byte]) {
                Ok(())
            } else {
                Err(MethodCallFailed::InternalError(VmError::Internal {
                    message: format!("ByteBuffer direct write failed at address 0x{:x}", addr),
                }))
            }
        }
    }
}

pub(crate) fn bb_read_bytes(
    ctx: &dyn NativeContext,
    view: BbView,
    start: usize,
    out: &mut [u8],
) -> Result<(), MethodCallFailed> {
    match view.storage {
        BbStorage::Heap { arr, offset } => {
            for (i, byte) in out.iter_mut().enumerate() {
                *byte = match ctx.get_array_element(arr, offset + start + i) {
                    Value::Int(v) => v as u8,
                    _ => 0,
                };
            }
            Ok(())
        }
        BbStorage::Direct { addr } => {
            if ctx.copy_from_native_memory(addr.saturating_add(start as i64), out) {
                Ok(())
            } else {
                Err(MethodCallFailed::InternalError(VmError::Internal {
                    message: format!("ByteBuffer direct bulk read failed at address 0x{:x}", addr),
                }))
            }
        }
    }
}

pub(crate) fn bb_write_bytes(
    ctx: &mut dyn NativeContext,
    view: BbView,
    start: usize,
    data: &[u8],
) -> Result<(), MethodCallFailed> {
    match view.storage {
        BbStorage::Heap { arr, offset } => {
            // One copy into the array payload; the per-element store below is
            // the fallback for a context that declines the bulk write (it
            // answers `false` for an out-of-range request, which the loop then
            // reports the way the element store always did).
            if !ctx.write_byte_array_from(arr, offset + start, data) {
                for (i, &byte) in data.iter().enumerate() {
                    ctx.set_array_element(arr, offset + start + i, Value::Int(byte as i8 as i32));
                }
            }
            Ok(())
        }
        BbStorage::Direct { addr } => {
            if ctx.copy_to_native_memory(addr.saturating_add(start as i64), data) {
                Ok(())
            } else {
                Err(MethodCallFailed::InternalError(VmError::Internal {
                    message: format!(
                        "ByteBuffer direct bulk write failed at address 0x{:x}",
                        addr
                    ),
                }))
            }
        }
    }
}

/// JDK-faithful bounds test for a typed-buffer ABSOLUTE accessor: a `width`-byte
/// read/write at `index` is valid iff `0 <= index` and `index + width <= bound`.
/// Uses checked arithmetic so a large positive `index` (near `i32::MAX`) cannot
/// overflow `index + width` into a negative that would slip past a naive
/// `index + width > bound` test (and panic in a debug build). Rejecting a
/// negative `index` is the lower-bound check the abs accessors were missing.
pub(crate) fn abs_access_in_bounds(index: i32, width: i32, bound: i32) -> bool {
    index >= 0 && index.checked_add(width).map_or(false, |end| end <= bound)
}

/// JDK-faithful bounds test for an element-indexed typed-buffer absolute
/// accessor (IntBuffer/LongBuffer/FloatBuffer/…/CharBuffer get(i)/put(i,v)):
/// the element at `idx` is in range iff `0 <= idx < bound` (one element wide).
/// Rejecting `idx < 0` is the lower-bound check these accessors were missing —
/// without it a negative Java index is cast to a huge `usize` and silently
/// reads/writes nothing at the GC layer instead of throwing.
pub(crate) fn tb_index_in_bounds(idx: i32, bound: i32) -> bool {
    idx >= 0 && idx < bound
}

/// What an out-of-range ABSOLUTE buffer accessor raises.
///
/// `java.nio.Buffer.checkIndex` hands `Preconditions` a formatter of its own
/// whose whole body is `new IndexOutOfBoundsException()`, so the class is
/// `IndexOutOfBoundsException` and `getMessage()` is null — verified against
/// HotSpot in `probes/PreconditionsFormatterProbe`.
///
/// These sites used to raise `IllegalArgumentException` carrying the *string*
/// `"IndexOutOfBoundsException"`, a stand-in from when `RuntimeError` had no
/// way to express the real class. `IllegalArgumentException` is not in the
/// `IndexOutOfBoundsException` hierarchy at all, so `catch
/// (IndexOutOfBoundsException)` around a buffer access did not see it and the
/// throw escaped as an unrelated failure.
pub(crate) fn buffer_index_out_of_bounds() -> MethodCallFailed {
    RuntimeError::ioobe_no_message().into()
}

/// `Objects.checkFromIndexSize(from, size, length)` for the buffer natives
/// that shadow the bytecode which would have called it — `slice(index, length)`
/// and the array-side check of the bulk accessors.
///
/// Unlike [`buffer_index_out_of_bounds`], these DO carry the
/// `Preconditions.outOfBoundsMessage` text: their real callers reach
/// `Preconditions` through `Objects`, whose `null` formatter puts that text on
/// the exception.
pub(crate) fn buffer_check_from_index_size(from: i32, size: i32, length: i32) -> Result<(), MethodCallFailed> {
    let bad =
        from < 0 || size < 0 || length < 0 || i64::from(from) + i64::from(size) > i64::from(length);
    if !bad {
        return Ok(());
    }
    Err(RuntimeError::ioobe(
        cratonvm_types::error::out_of_bounds_message::check_from_index_size(
            i64::from(from),
            i64::from(size),
            i64::from(length),
        ),
    )
    .into())
}

