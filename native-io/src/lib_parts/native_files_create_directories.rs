#[allow(unused_imports)]
use super::super::*;

pub(crate) fn native_files_create_directories(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let s = validated_path(&files_path_str(ctx, args))?;
    if let Err(e) = std::fs::create_dir_all(&s) {
        return Err(io_err(e));
    }
    let path = match args.first() {
        Some(v) => *v,
        _ => Value::Object(None),
    };
    Ok(Some(path))
}

pub(crate) fn native_files_read_all_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s = validated_path(&files_path_str(ctx, args))?;
    let bytes = std::fs::read(&s).map_err(|e| io_err_nio(e, &s))?;
    // gc-common w9-d: file-sized, so reclaim and retry before the OOME. The
    // path argument was consumed into `s`; nothing else is held, and this
    // native is registered-only (`Files.readAllBytes`).
    let arr = new_byte_array_reclaiming(ctx, bytes.len(), "byte[] (Files.readAllBytes)")?;
    // AUDIT 2026-05-29: bulk copy via NativeContext::write_byte_array_from
    // instead of a per-element `set_array_element` loop. The VM override
    // uses `ptr::copy_nonoverlapping`, so a multi-MB file is one memcpy
    // rather than millions of `Value::Int` boxes + dispatches. The array
    // was just allocated to exactly `bytes.len()`, so the bounds check
    // inside the intrinsic always succeeds here.
    ctx.write_byte_array_from(arr, 0, &bytes);
    Ok(Some(Value::Object(Some(arr))))
}

pub(crate) fn native_files_read_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s = validated_path(&files_path_str(ctx, args))?;
    let content = std::fs::read_to_string(&s).map_err(|e| io_err_nio(e, &s))?;
    let result = ctx.create_string(&content);
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_files_write_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s = validated_path(&files_path_str(ctx, args))?;
    let arr = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(args.first().copied()),
    };
    let len = ctx.array_length(arr);
    // AUDIT 2026-05-29: bulk read via NativeContext::read_byte_array_into
    // instead of a per-element `get_array_element` loop. The VM override
    // memcpys from the array's raw payload, turning a multi-MB write into
    // one copy rather than millions of dispatches. Sized to the full array
    // length so every byte is captured (the intrinsic returns the count
    // copied, which equals `len` here).
    let mut bytes = vec![0u8; len];
    let n = ctx.read_byte_array_into(arr, 0, &mut bytes);
    bytes.truncate(n);
    std::fs::write(&s, &bytes).map_err(io_err)?;
    Ok(args.first().copied())
}

pub(crate) fn native_files_write_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s = validated_path(&files_path_str(ctx, args))?;
    let content = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    std::fs::write(&s, content.as_bytes()).map_err(io_err)?;
    Ok(args.first().copied())
}

pub(crate) fn native_files_read_all_lines(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s = validated_path(&files_path_str(ctx, args))?;
    let content = std::fs::read_to_string(&s).map_err(io_err)?;
    let lines: Vec<&str> = content.lines().collect();
    // Return as ArrayList
    let list = match ctx.ensure_class_initialized("java/util/ArrayList") {
        Ok(cid) => ctx.alloc_object(cid, 2),
        Err(_) => ctx.alloc_object(cratonvm_types::ClassId::new(0), 2),
    };
    // GC: `al_init` and `al_add` pin what they are given, but the address
    // this local carries is stale from the PREVIOUS iteration's
    // `create_string` — the callee can only pin the address it is handed.
    let list_pin = ctx.pin_native_root(list);
    let mut list = list;
    al_init(ctx, list);
    for line in &lines {
        let line_str = ctx.create_string(line);
        list = ctx.read_native_pin(list_pin, list);
        al_add(ctx, list, Value::Object(Some(line_str)));
    }
    list = ctx.read_native_pin(list_pin, list);
    ctx.unpin_native_roots(list_pin);
    Ok(Some(Value::Object(Some(list))))
}

pub(crate) fn native_files_copy(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let src = validated_path(&files_path_str(ctx, args))?;
    let dst = match args.get(1) {
        Some(Value::Object(Some(o))) => read_path_str(ctx, *o),
        _ => String::new(),
    };
    let dst = validated_path(&dst)?;
    // Java `Files.copy(Path,Path,CopyOption...)` semantics: copying a DIRECTORY
    // creates an (empty) directory at the target — it does NOT open the source
    // as a file. `std::fs::copy` only handles regular files; on a directory it
    // fails ("Access denied / os error 5" on Windows, because it opens the dir
    // for reading), which broke every `TomcatBaseTest.recursiveCopy` (the whole
    // `catalina.webresources` cluster — `preVisitDirectory` does
    // `Files.copy(dir, …)`). Branch on the source kind; be lenient if the target
    // dir already exists, mirroring the file path's overwrite behaviour.
    let src_is_dir = std::fs::symlink_metadata(&src)
        .map(|m| m.is_dir())
        .unwrap_or(false);
    if src_is_dir {
        match std::fs::create_dir(&dst) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_err(e)),
        }
    } else {
        std::fs::copy(&src, &dst).map_err(io_err)?;
    }
    Ok(args.get(1).copied())
}

pub(crate) fn native_files_move(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let src = validated_path(&files_path_str(ctx, args))?;
    let dst = match args.get(1) {
        Some(Value::Object(Some(o))) => read_path_str(ctx, *o),
        _ => String::new(),
    };
    let dst = validated_path(&dst)?;
    std::fs::rename(&src, &dst).map_err(io_err)?;
    Ok(args.get(1).copied())
}

pub(crate) fn native_files_is_readable(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s = files_path_str(_ctx, args);
    Ok(Some(Value::Int(if std::path::Path::new(&s).exists() {
        1
    } else {
        0
    })))
}

pub(crate) fn native_files_is_writable(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s = files_path_str(_ctx, args);
    let writable = std::fs::metadata(&s)
        .map(|m| !m.permissions().readonly())
        .unwrap_or(false);
    Ok(Some(Value::Int(if writable { 1 } else { 0 })))
}

// ===========================================================================
// Phase 36: I/O extras — RandomAccessFile, CharArrayReader/Writer
// ===========================================================================

/// Gate: when real-RAF is enabled the synthetic `java.io.RandomAccessFile`
/// natives are NOT registered, so RAF runs its real JDK bytecode (real ctor →
/// `new FileDescriptor(); open0(...)` + the open0/read0/seek0/length0 platform
/// primitives). Read once and cached. See `register_io_extras_natives`.
///
/// Real-RAF is now the DEFAULT. The synthetic RAF path is broken: two synthetic
/// implementations (this crate's `register_io_extras_natives` and
/// native-builtins `register_phase57_random_access_file`) both register RAF
/// methods and their handle models conflict, leaving `this.fd` null →
/// `length()`=0, `read()`=-1, and commons-compress's seek-from-EOF computes a
/// negative offset ("seek before beginning of file"). The SEGV/Cleaner crashes
/// that originally justified gating real-RAF behind opt-in are FIXED (see
/// app-jvm-bugs/real-raf-segv-root-cause.md, 2026-06-02). Opt back
/// into the (broken) synthetic path with `CRATONVM_SYNTHETIC_RAF=1`.
pub(crate) fn real_raf_enabled() -> bool {
    !io_flags().synthetic_raf_forced
}

pub(crate) fn register_io_extras_natives(registry: &mut NativeMethodRegistry) {
    //
    // MODE GATE: when real-RAF is on, skip these synthetic natives so
    // `java.io.RandomAccessFile` runs its REAL JDK bytecode (real ctor ->
    // `new FileDescriptor(); open0(...)`, plus the native-io platform primitives
    // open0/read0/write0/seek0/...). The synthetic `<init>` otherwise shadows the
    // real ctor via native-override priority, leaving `this.fd` null. Real-RAF is
    // required for DaCapo luindex (FSDirectory.sync -> RAF.getFD().sync()).
    //
    // THREE CORRECTIONS to what this comment said until 2026-08-20 (H8-1):
    //
    //  1. The knob is `CRATONVM_SYNTHETIC_RAF=1`, not `CRATONVM_REAL_RAF=1`.
    //     The latter is RETIRED — `types/tests/flag_declaration_guard.rs`
    //     carries a row saying so, and `types/src/flag_groups.rs` declares
    //     only `off_key: CRATONVM_SYNTHETIC_RAF`. Setting `CRATONVM_REAL_RAF`
    //     does nothing at all.
    //  2. "Default (unset) = synthetic" is backwards. `real_raf_enabled()` is
    //     `!io_flags().synthetic_raf_forced`, so the DEFAULT is real-RAF and
    //     this whole block is skipped unless the operator opts back in. The
    //     doc comment on `real_raf_enabled` has said so since 2026-06-02;
    //     these lines were never updated with it.
    //  3. The block is not "gated rather than removed because the real ctor's
    //     FileCleanable/Cleaner/PhantomReference path still has a separate
    //     crash under sustained load that is being diagnosed". That crash is
    //     FIXED — see the `real_raf_enabled` doc comment below and
    //     app-jvm-bugs/real-raf-segv-root-cause.md, 2026-06-02 — and the same
    //     doc comment calls the surviving synthetic path BROKEN and describes
    //     the flag as an opt-in back into it. Whatever the reason for keeping
    //     the block is, it is not the one stated here; this comment does not
    //     invent a replacement.
    //
    // ONE OF THESE 18 TRIPLES USED TO ESCAPE THE GATE.
    // `getFilePointer()J` is also registered by
    // `random_access_file::register_random_access_file_natives`, called six
    // lines after this function in `register_io_natives`, and that
    // registration was unconditional — so this synthetic row was dead in BOTH
    // settings of the flag and the synthetic arm's `getFilePointer()` answered
    // a constant 0. The gate is now mirrored at that call site. It is the only
    // one: the other nine rows over there are `open0`/`read0`/`readBytes0`/
    // `write0`/`writeBytes0`/`seek0`/`length0`/`setLength0`/`initIDs`, and no
    // name on this list matches any of them.
    if !real_raf_enabled() {
        let raf = "java/io/RandomAccessFile";
        registry.register(
            raf,
            "<init>",
            "(Ljava/lang/String;Ljava/lang/String;)V",
            native_raf_init, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            raf,
            "<init>",
            "(Ljava/io/File;Ljava/lang/String;)V",
            native_raf_init_file, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(raf, "read", "()I", native_raf_read, cratonvm_native_api::NativeKind::Bridge);
        registry.register(raf, "read", "([BII)I", native_raf_read_bulk, cratonvm_native_api::NativeKind::Bridge);
        registry.register(raf, "write", "(I)V", native_raf_write, cratonvm_native_api::NativeKind::Bridge);
        registry.register(raf, "write", "([BII)V", native_raf_write_bulk, cratonvm_native_api::NativeKind::Bridge);
        registry.register(raf, "seek", "(J)V", native_raf_seek, cratonvm_native_api::NativeKind::Bridge);
        registry.register(raf, "getFilePointer", "()J", native_raf_get_file_pointer, cratonvm_native_api::NativeKind::Bridge);
        registry.register(raf, "length", "()J", native_raf_length, cratonvm_native_api::NativeKind::Bridge);
        registry.register(raf, "close", "()V", native_raf_close, cratonvm_native_api::NativeKind::Bridge);
        registry.register(raf, "readInt", "()I", native_raf_read_int, cratonvm_native_api::NativeKind::Bridge);
        registry.register(raf, "readLong", "()J", native_raf_read_long, cratonvm_native_api::NativeKind::Bridge);
        registry.register(raf, "writeInt", "(I)V", native_raf_write_int, cratonvm_native_api::NativeKind::Bridge);
        registry.register(raf, "writeLong", "(J)V", native_raf_write_long, cratonvm_native_api::NativeKind::Bridge);
        registry.register(raf, "readFully", "([B)V", native_raf_read_fully, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            raf,
            "readLine",
            "()Ljava/lang/String;",
            native_raf_read_line, cratonvm_native_api::NativeKind::Bridge,
        );
        // `readUTF` was bound to `native_raf_read_line` with the one-word
        // comment "simplified". It is not a simplification of `readLine`, it is
        // a different wire format — see `native_raf_read_utf`, which reads the
        // 2-byte length prefix and decodes modified UTF-8, and `writeUTF`, which
        // had no registration here at all and so could not round-trip.
        registry.register(raf, "readUTF", "()Ljava/lang/String;", native_raf_read_utf, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            raf,
            "writeUTF",
            "(Ljava/lang/String;)V",
            native_raf_write_utf, cratonvm_native_api::NativeKind::Bridge,
        );
    } // end !real_raf_enabled()

    // RDR-MIGRATION 2026-06-01: CharArrayReader synthetic natives (3-field
    // buf/pos/count) shadowed real CharArrayReader bytecode (buf/pos/markedPos/
    // count) and only implemented `read()I` — a real BufferedReader wrapping it
    // calls `read([CII)I`, which had no native and ran real bytecode against
    // the wrong field layout. Run the whole CharArrayReader as real bytecode
    // (self-contained, no native primitives) and keep the synthetic natives
    // under `synthetic-jdk`.
    //
    // The cfg alone is not the guard: a `--features synthetic-jdk` binary run
    // against a real JDK still satisfies it while facing the real class. Slot
    // 0 here (`CAR_FIELD_BUF`) is real `Reader.lock` on a genuine
    // `CharArrayReader` — the same slot-0-is-`lock` collision already found
    // and fixed for `InputStreamReader`/`StringReader`
    // (synthetic-jdk-feature-shadows-real-classes-in-real-jdk-mode-20260917.md),
    // so add the runtime predicate too.
    #[cfg(feature = "synthetic-jdk")]
    if !registry.drops_real_layout_synthetic() {
        // CharArrayReader = 3-field synthetic (buf=0, pos=1, count=2)
        let car = "java/io/CharArrayReader";
        registry.register(car, "<init>", "([C)V", native_car_init, cratonvm_native_api::NativeKind::Bridge);
        registry.register(car, "<init>", "([CII)V", native_car_init_off, cratonvm_native_api::NativeKind::Bridge);
        registry.register(car, "read", "()I", native_car_read, cratonvm_native_api::NativeKind::Bridge);
        registry.register(car, "ready", "()Z", native_car_ready, cratonvm_native_api::NativeKind::Bridge);
        registry.register(car, "close", "()V", native_car_close, cratonvm_native_api::NativeKind::Bridge);
    }

    // CharArrayWriter: the synthetic 2-field carrier (buf=0, count=1) shadowed
    // only SOME methods (`<init>()V`, `write(I)V`, `write([CII)V`, …) — the
    // unshadowed ones (`write(String)`, `append`, `writeTo`, …) ran real JDK
    // bytecode against a REAL `CharArrayWriter` whose field layout is
    // `Writer.lock` + `buf` + `count` (NOT the synthetic slots 0/1). Result:
    // the synthetic `<init>` never set the inherited `lock`, so `write(String)`
    // NPE'd on `synchronized (lock)` (monitorenter on null), and the slot-based
    // bulk write corrupted/no-op'd. This broke Jasper's `JspReader`
    // (`CharArrayWriter.write(buf,0,n)` → `toCharArray()` returned empty) →
    // every JSP compiled to an EMPTY servlet (HTTP 200, 0-byte body) →
    // `TestPageContext` "contains on null". The real `CharArrayWriter` bytecode
    // is simple and self-contained (its ctor chains through `Writer()` which
    // sets `lock = this`), so run it. Keep the synthetic carrier only under
    // `synthetic-jdk` (mirrors the LineNumberReader migration below).
    //
    // The cfg alone is not the guard (see the CharArrayReader comment above,
    // and synthetic-jdk-feature-shadows-real-classes-in-real-jdk-mode-20260917.md):
    // a `--features synthetic-jdk` binary run against a real JDK still
    // satisfies it. `CAW_FIELD_BUF` is slot 0, which on a genuine
    // `CharArrayWriter` is inherited `Writer.lock` — exactly the collision
    // this comment already diagnoses (the Jasper `JspReader` failure), so add
    // the runtime predicate too.
    #[cfg(feature = "synthetic-jdk")]
    if !registry.drops_real_layout_synthetic() {
        // CharArrayWriter = 2-field synthetic (buf=0, count=1)
        let caw = "java/io/CharArrayWriter";
        registry.register(caw, "<init>", "()V", native_caw_init, cratonvm_native_api::NativeKind::Bridge);
        registry.register(caw, "write", "(I)V", native_caw_write, cratonvm_native_api::NativeKind::Bridge);
        registry.register(caw, "write", "([CII)V", native_caw_write_bulk, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            caw,
            "toString",
            "()Ljava/lang/String;",
            native_caw_to_string, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(caw, "toCharArray", "()[C", native_caw_to_char_array, cratonvm_native_api::NativeKind::Bridge);
        registry.register(caw, "size", "()I", native_caw_size, cratonvm_native_api::NativeKind::Bridge);
        registry.register(caw, "reset", "()V", native_caw_reset, cratonvm_native_api::NativeKind::Bridge);
        // KEEP (real JDK body is empty) — audited wave 4, 2026-07-28.
        // `java.io.CharArrayWriter.flush()` is `public void flush() {}` and
        // `close()` is `public void close() {}` in java.base. The sink is the
        // `char[] buf` above, so there is nothing to push or release, and the
        // class documents that "Close the stream. This method does not release
        // the buffer, since its contents might still be required" — a closed
        // CharArrayWriter must keep serving `toCharArray`/`toString` AND keep
        // accepting writes. No-op is the specified behaviour; clearing the
        // buffer or throwing here would break `reset`-free reuse.
        registry.register(caw, "flush", "()V", native_noop_void, cratonvm_native_api::NativeKind::Bridge);
        registry.register(caw, "close", "()V", native_noop_void, cratonvm_native_api::NativeKind::Bridge);
    } // end #[cfg(feature = "synthetic-jdk")] synthetic CharArrayWriter natives

    // RDR-MIGRATION 2026-06-01: LineNumberReader extends BufferedReader; its
    // synthetic readLine/<init> natives (4-field in/lineNumber/pos/content)
    // shadowed the real bytecode and broke once the rest of the Reader stack
    // went real. Real LineNumberReader bytecode builds on real BufferedReader,
    // which now works, so run it as real bytecode and keep the synthetic
    // natives under `synthetic-jdk`.
    //
    // The cfg alone is not the guard (see the CharArrayReader/CharArrayWriter
    // comments above, and
    // synthetic-jdk-feature-shadows-real-classes-in-real-jdk-mode-20260917.md):
    // a `--features synthetic-jdk` binary run against a real JDK still
    // satisfies it. `LNR_FIELD_IN` is slot 0, which on a real object in this
    // Reader chain is inherited `Reader.lock`, so `<init>` clobbers the
    // monitor instead of setting up the wrapped Reader — add the runtime
    // predicate too.
    #[cfg(feature = "synthetic-jdk")]
    if !registry.drops_real_layout_synthetic() {
        // LineNumberReader = 4-field synthetic (in=0, lineNumber=1, pos=2, content=3)
        let lnr = "java/io/LineNumberReader";
        registry.register(lnr, "<init>", "(Ljava/io/Reader;)V", native_lnr_init, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            lnr,
            "readLine",
            "()Ljava/lang/String;",
            native_lnr_read_line, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(lnr, "getLineNumber", "()I", native_lnr_get_line_number, cratonvm_native_api::NativeKind::Bridge);
        registry.register(lnr, "setLineNumber", "(I)V", native_lnr_set_line_number, cratonvm_native_api::NativeKind::Bridge);
        // Real `LineNumberReader` inherits `BufferedReader.close()`, which
        // closes the Reader it wraps. The no-op that used to sit here leaked
        // that Reader (and any file handle behind it) for the process lifetime.
        // Slot 0 is `in` per the synthetic layout above; clearing it afterwards
        // keeps `close()` idempotent.
        registry.register(lnr, "close", "()V", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            if let Value::Object(Some(inner)) = ctx.get_field(this, 0) {
                // Clear before dispatching — the nested `close()` can trigger a
                // moving GC that relocates `this`, stranding a later write.
                // …and `BufferedReader.close()` is `in.close()` in a
                // `try`/`finally` that only nulls its own fields, under
                // `throws IOException` with no `catch`, so the delegated
                // failure PROPAGATES.
                // W7-57-close-flush-swallow-sweep.md
                ctx.set_field(this, 0, Value::Object(None));
                ctx.invoke_virtual(inner, "close", "()V", &[])?;
            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge);
    }
    }

// H8-1 §2.3/§5.3 (2026-09-21 follow-up): this block used to keep its fd and
// path in absolute slots 0/1 of the RandomAccessFile receiver, on the theory
// that this class is fabricated whenever the block is live. It is NOT --
// `CRATONVM_SYNTHETIC_RAF=1` re-registers these natives on the SAME real
// `java.io.RandomAccessFile` class real-JDK mode already loaded (`javap`:
// `fd: FileDescriptor` at slot 0, `rw: boolean` at slot 1), and this
// interpreter now DESTROYS a field store whose value's kind (reference vs.
// primitive) contradicts the slot's declared descriptor rather than silently
// miswriting it (`G30-1-the-silent-reference-slot-coercion-20260817.md`).
// `Value::Int(fd)` into slot 0 is primitive-into-reference; `Value::Object
// (path)` into slot 1 is pointer-into-primitive. Both stores were being
// dropped, so every synthetic-RAF read fell through to its `_ => `
// fallback -- `getFilePointer()` returned the constant 0 that `H8-1` H8-B
// diagnosed, but not for the reason it named: the gate H8-B added was correct,
// the FIELD WRITE it gated never landed.
//
// H8-1 N2 named the fix generically ("either retire the synthetic path or
// make it write a real FileDescriptor"); the one applied here is the pattern
// already used elsewhere in this tree for a flag on a real JDK class (see
// memory: SSLContext's `contextSpi`-slot flag moved to an identity-hash-keyed
// side table for the same reason). A real `FileDescriptor` object would also
// work but touches every read site to unwrap it; the side table changes only
// the four functions that used to read or write a slot.
///
/// gc-common w16-c (`common-w15c-native-io-identity-keyed-tables-outside-the-weak-sweep`
/// row 7): the row is WEAK on its `RandomAccessFile` (`owner`). A file
/// dropped without `close()` loses its row at the next collection, and its fd
/// is queued on `io_orphan_fds` and closed by this VM's next synthetic-RAF
/// constructor or `close()` (HotSpot's `FileCleanable` closes it too).
/// `forget_vm_io_side_tables` drops a disposed VM's rows.
pub(crate) struct RafState {
    pub(crate) fd: u32,
    /// `fd`'s `FileDescriptorTable::generation` when the row was filed (JIT
    /// r23 wave 3 review): a dead or replaced row's fd is parked for a LATE
    /// close, which must not reach a later owner of a reused OS key (the
    /// file's `getChannel()` may have closed `fd` meanwhile).
    pub(crate) generation: u64,
    pub(crate) path: String,
    pub(crate) owner: usize,
}

impl IoWeakRow for RafState {
    fn owner(&self) -> usize {
        self.owner
    }
    fn set_owner(&mut self, owner: usize) {
        self.owner = owner;
    }
}

/// `(vm_identity, identity_hash_code, current address)`, the same
/// disambiguation `ScanKey` uses -- see [`scan_key`] and [`IoRowKey`]. The
/// address half is gc-common w29-c: two LIVE files of one VM with equal
/// identity hashes used to share ONE row, so the second constructor REPLACED
/// the first file's fd (queueing it for close) and the first file's reads and
/// writes went to the second's file.
pub(crate) type RafKey = IoRowKey;

/// `this` must be CURRENT: every synthetic-RAF native calls this with its
/// receiver before any GC point.
pub(crate) fn raf_key(ctx: &dyn NativeContext, this: ObjectRef) -> RafKey {
    io_side_row_key(ctx, this)
}

pub(crate) fn raf_states() -> &'static Mutex<HashMap<RafKey, RafState>> {
    static TABLE: OnceLock<Mutex<HashMap<RafKey, RafState>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `this` must be CURRENT: it becomes the row's weak owner. A constructor
/// that re-initialises a live file replaces its row; the replaced fd (never
/// closed before) is queued like a dead file's.
pub(crate) fn raf_set_state(ctx: &dyn NativeContext, this: ObjectRef, fd: u32, path: String) {
    let key = raf_key(ctx, this);
    let row = RafState {
        fd,
        generation: ctx.fd_table().generation(fd).unwrap_or(0),
        path,
        owner: io_side_owner(this),
    };
    let replaced = raf_states().lock().insert(key, row);
    if let Some(old) = replaced.filter(|old| old.fd != fd) {
        queue_orphan_fds_checked(key.0, vec![(old.fd, old.generation)]);
    }
}

pub(crate) fn raf_fd(ctx: &dyn NativeContext, this: ObjectRef) -> Option<u32> {
    let key = raf_key(ctx, this);
    raf_states().lock().get(&key).map(|s| s.fd)
}

pub(crate) fn raf_path(ctx: &dyn NativeContext, this: ObjectRef) -> Option<String> {
    let key = raf_key(ctx, this);
    raf_states().lock().get(&key).map(|s| s.path.clone())
}

// RAF natives use `fd_table.open_read_write(...)` so that the underlying
// `FileEntry::FileReadWrite` variant supports `rw_seek`/`rw_read`/`rw_write`
// (the `open_read`/`open_write` variants used here previously do not, which
// turned `seek(J)V` into a silent no-op and broke any caller that walked
// the file backward — e.g. Spring Boot 2's internal jar reader scanning
// for the ZIP End-Of-Central-Directory record).
pub(crate) fn native_raf_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let path = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let mode = match args.get(2) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => "r".to_string(),
    };
    let writable = mode.contains('w');
    reject_directory_open(&path)?;
    // Files that died unclosed since this VM last opened or closed one
    // (gc-common w16-c, `io_orphan_fds`).
    close_orphan_fds(ctx);
    let fd = ctx
        .fd_table()
        .open_read_write(&path, writable)
        .map_err(io_err)?;
    // Side table, not a field store -- see the block comment above
    // `RafState`. No allocation follows, so `this` needs no pin.
    raf_set_state(ctx, this, fd, path);
    Ok(None)
}

pub(crate) fn native_raf_init_file(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let file = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let path = match ctx.get_field(file, 0) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let mode = match args.get(2) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => "r".to_string(),
    };
    let writable = mode.contains('w');
    reject_directory_open(&path)?;
    // Files that died unclosed since this VM last opened or closed one
    // (gc-common w16-c, `io_orphan_fds`).
    close_orphan_fds(ctx);
    let fd = ctx
        .fd_table()
        .open_read_write(&path, writable)
        .map_err(io_err)?;
    // Side table, not a field store -- see the block comment above
    // `RafState`. No allocation follows, so `this` needs no pin.
    raf_set_state(ctx, this, fd, path);
    Ok(None)
}

pub(crate) fn native_raf_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let Some(fd) = raf_fd(ctx, this) else {
        return Ok(Some(Value::Int(-1)));
    };
    let mut buf = [0u8; 1];
    // Same as `native_raf_read_bulk`: `-1` is `RandomAccessFile.read()`'s
    // "the end of the file has been reached", and the method separately
    // declares "@throws IOException if an I/O error occurs". Answering EOF for
    // a failed read merges the two states the contract keeps apart.
    match ctx.fd_table().rw_read(fd, &mut buf) {
        Ok(0) => Ok(Some(Value::Int(-1))),
        Ok(_) => Ok(Some(Value::Int(buf[0] as i32))),
        Err(e) => Err(io_err(e)),
    }
}

pub(crate) fn native_raf_read_bulk(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let buf = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
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
    let Some(fd) = raf_fd(ctx, this) else {
        return Ok(Some(Value::Int(-1)));
    };
    if len == 0 {
        return Ok(Some(Value::Int(0)));
    }
    let mut tmp = vec![0u8; len];
    // `Err(_) => -1` reported an I/O FAILURE as end-of-file, the one value a
    // read loop is built to stop on: "@return the total number of bytes read
    // into the buffer, or -1 if there is no more data because the end of the
    // file has been reached" (`RandomAccessFile.read(byte[],int,int)`), which
    // also declares "@throws IOException If the first byte cannot be read for
    // any reason other than end of file". A caller cannot tell the two apart,
    // so a truncated read looked like a complete file.
    let n = match ctx.fd_table().rw_read(fd, &mut tmp) {
        Ok(0) => return Ok(Some(Value::Int(-1))),
        Ok(n) => n,
        Err(e) => return Err(io_err(e)),
    };
    for i in 0..n {
        ctx.set_array_element(buf, off + i, Value::Int(tmp[i] as i8 as i32));
    }
    Ok(Some(Value::Int(n as i32)))
}

pub(crate) fn native_raf_write(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let b = match args.get(1) {
        Some(Value::Int(v)) => *v as u8,
        _ => 0,
    };
    let Some(fd) = raf_fd(ctx, this) else {
        return Ok(None);
    };
    // Every `RandomAccessFile` write in this file declares "@throws IOException
    // if an I/O error occurs" and returns void — the exception is the ONLY
    // channel it has, so `let _ =` left the method literally unable to report
    // anything. A record-appending loop against a full volume completed
    // silently.
    ctx.fd_table().rw_write(fd, &[b]).map_err(io_err)?;
    Ok(None)
}

pub(crate) fn native_raf_write_bulk(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let buf = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let off = match args.get(2) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let len = match args.get(3) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let Some(fd) = raf_fd(ctx, this) else {
        return Ok(None);
    };
    let mut bytes = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Int(b) = ctx.get_array_element(buf, off + i) {
            bytes.push(b as u8);
        }
    }
    ctx.fd_table().rw_write(fd, &bytes).map_err(io_err)?;
    Ok(None)
}

pub(crate) fn native_raf_seek(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // RKC23B: the operand-stack tag for category-2 long arguments crossing
    // an `invokevirtual` boundary may arrive as `Double` (same 64-bit
    // payload, different Value tag) when the upstream method passed the
    // long via `lload_<n>` from a slot the JIT has cached as a double.
    // Accept either tag and reinterpret the bits as i64. Same defensive
    // pattern is applied to every J-typed RAF native below.
    let pos = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Double(v)) => i64::from_le_bytes(v.to_le_bytes()),
        _ => 0,
    };
    // `RandomAccessFile.seek(long)`: "@throws IOException if pos is less than 0
    // or if an I/O error occurs." Note the refusal is an `IOException` here,
    // not the `IllegalArgumentException` `FileChannel.position(long)` raises
    // for the same input — the two classes genuinely differ, so this cannot be
    // shared with the channel-side check. `pos.max(0)` answered a successful
    // seek to the start of the file, which is exactly what commons-compress's
    // seek-from-EOF arithmetic produces when its length source is wrong: the
    // next `read` then returned the FIRST record instead of the one asked for.
    if pos < 0 {
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IOException {
                message: format!("Negative seek offset: {pos}"),
            },
        )));
    }
    let Some(fd) = raf_fd(ctx, this) else {
        return Ok(None);
    };
    ctx.fd_table()
        .rw_seek(fd, std::io::SeekFrom::Start(pos as u64))
        .map_err(io_err)?;
    Ok(None)
}

pub(crate) fn native_raf_get_file_pointer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let Some(fd) = raf_fd(ctx, this) else {
        return Ok(Some(Value::Long(0)));
    };
    let pos = ctx.fd_table().rw_position(fd).unwrap_or(0);
    Ok(Some(Value::Long(pos as i64)))
}

pub(crate) fn native_raf_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let path = raf_path(ctx, this).unwrap_or_default();
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    Ok(Some(Value::Long(size as i64)))
}

pub(crate) fn native_raf_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let Some(fd) = raf_fd(ctx, this) else {
        return Ok(None);
    };
    // Drop the side-table entry so a long-running process opening many
    // synthetic-RAF instances does not grow this table forever. A double
    // close() re-enters here with the entry already gone, which is fine:
    // `raf_fd` above already returned early in that case.
    let key = raf_key(ctx, this);
    raf_states().lock().remove(&key);
    let _ = ctx.fd_table().close(fd);
    close_orphan_fds(ctx);
    Ok(None)
}

pub(crate) fn native_raf_read_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let Some(fd) = raf_fd(ctx, this) else {
        return Ok(Some(Value::Int(0)));
    };
    let mut bytes = [0u8; 4];
    for b in &mut bytes {
        let val = ctx.fd_table().read_byte(fd).unwrap_or(0);
        *b = val as u8;
    }
    Ok(Some(Value::Int(i32::from_be_bytes(bytes))))
}

pub(crate) fn native_raf_read_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let Some(fd) = raf_fd(ctx, this) else {
        return Ok(Some(Value::Long(0)));
    };
    let mut bytes = [0u8; 8];
    for b in &mut bytes {
        let val = ctx.fd_table().read_byte(fd).unwrap_or(0);
        *b = val as u8;
    }
    Ok(Some(Value::Long(i64::from_be_bytes(bytes))))
}

pub(crate) fn native_raf_write_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let v = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let Some(fd) = raf_fd(ctx, this) else {
        return Ok(None);
    };
    // `DataOutput.writeInt`/`writeLong` are void and declare "@throws
    // IOException if an I/O error occurs"; discarding the result left them
    // unable to say anything but success. See `native_raf_write`.
    ctx.fd_table()
        .write_bytes(fd, &v.to_be_bytes())
        .map_err(io_err)?;
    Ok(None)
}

pub(crate) fn native_raf_write_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let v = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let Some(fd) = raf_fd(ctx, this) else {
        return Ok(None);
    };
    // `DataOutput.writeInt`/`writeLong` are void and declare "@throws
    // IOException if an I/O error occurs"; discarding the result left them
    // unable to say anything but success. See `native_raf_write`.
    ctx.fd_table()
        .write_bytes(fd, &v.to_be_bytes())
        .map_err(io_err)?;
    Ok(None)
}

pub(crate) fn native_raf_read_fully(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let buf = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let Some(fd) = raf_fd(ctx, this) else {
        return Ok(None);
    };
    let len = ctx.array_length(buf);
    // PERF FIX (2026-07-13, STW-takeover-cluster residual investigation):
    // was one fd_table().read_byte() call per byte; bulk-read into a scratch
    // buffer instead (same anti-pattern found and fixed in
    // native_dis_read_bytes/dis_read_fully_impl above, though this one is
    // native-to-native rather than native-to-bytecode so it's cheaper per
    // iteration — still O(len) syscalls instead of O(len/chunk)).
    let mut scratch = vec![0u8; len.min(65536)];
    let mut filled = 0usize;
    while filled < len {
        let want = (len - filled).min(scratch.len());
        let n = ctx
            .fd_table()
            .read_bytes(fd, &mut scratch[..want])
            .unwrap_or(0);
        if n == 0 {
            break;
        }
        for (i, &b) in scratch[..n].iter().enumerate() {
            ctx.set_array_element(buf, filled + i, Value::Int(b as i32));
        }
        filled += n;
    }
    Ok(None)
}

pub(crate) fn native_raf_read_line(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let Some(fd) = raf_fd(ctx, this) else {
        return Ok(Some(Value::Object(None)));
    };
    match ctx.fd_table().read_line(fd) {
        Ok(Some(line)) => {
            let s = ctx.create_string(&line);
            Ok(Some(Value::Object(Some(s))))
        }
        _ => Ok(Some(Value::Object(None))),
    }
}

/// `RandomAccessFile.readUTF()` — `DataInput`'s framed record, not a line.
///
/// # The registration this replaces
///
/// `readUTF()Ljava/lang/String;` was bound to [`native_raf_read_line`], with the
/// word `simplified` for a comment. The two methods do not read the same shape
/// of data: `readLine` scans to the next `\n`/`\r` and returns everything before
/// it, while `readUTF` reads a **2-byte big-endian unsigned length** and then
/// exactly that many bytes of modified UTF-8. So the old binding consumed the
/// length prefix as though it were text, stopped at whichever payload byte
/// happened to be `0x0A`, and returned a string that shares no bytes with the
/// record that was written — and, having left the file position mid-record, it
/// corrupted every subsequent read on the same handle rather than only its own.
/// `DataInput.readUTF` is documented to throw `EOFException` "if this input
/// stream reaches the end before reading all the bytes" and
/// `UTFDataFormatException` "if the bytes do not represent a valid modified
/// UTF-8 encoding of a string"; the old binding could produce neither.
///
/// The decode is [`decode_modified_utf8`], the same one `DataInputStream.readUTF`
/// uses in this file — one spelling of the wire format per crate is the whole
/// point of the export. See W7-8-fabricated-success-io-sweep.md.
pub(crate) fn native_raf_read_utf(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let Some(fd) = raf_fd(ctx, this) else {
        return Ok(Some(Value::Object(None)));
    };
    let mut len_buf = [0u8; 2];
    raf_read_exact(ctx, fd, &mut len_buf)?;
    let len = u16::from_be_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    raf_read_exact(ctx, fd, &mut payload)?;
    // JO3-3 (round 15 wave 4, lane jdkonly5): the real `RandomAccessFile`
    // body is `DataInputStream.readUTF(this)`, the same decode.
    if data_utf_units_exact() {
        return read_utf_payload_units(ctx, &payload);
    }
    // `RandomAccessFile.readUTF` owes the same `UTFDataFormatException` as
    // `DataInputStream.readUTF` -- both implement `DataInput`, whose javadoc
    // names it -- so both go through the one helper rather than each spelling
    // its own refusal.
    let s = match decode_modified_utf8(&payload) {
        Ok(s) => s,
        Err(e) => return Err(utf_data_format_error(ctx, &e)),
    };
    let obj = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(obj))))
}

/// Fill `buf` completely or fail, the way `DataInput.readFully` is specified to.
///
/// A short read here is an `EOFException` in the JDK, never a short answer:
/// `readUTF`'s length prefix has already committed the caller to a record of a
/// stated size, so returning fewer bytes would hand back a truncated string
/// indistinguishable from a complete one. `RuntimeError` has no `EOFException`
/// variant, so this reports the supertype with the JDK's own wording; a caller
/// catching `IOException` — which is every caller of a `DataInput` — is
/// unaffected, and one catching `EOFException` specifically was getting a
/// silently truncated string before, not a narrower exception.
pub(crate) fn raf_read_exact(
    ctx: &mut dyn NativeContext,
    fd: u32,
    buf: &mut [u8],
) -> Result<(), MethodCallFailed> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let n = ctx
            .fd_table()
            .read_bytes(fd, &mut buf[filled..])
            .map_err(io_err)?;
        if n == 0 {
            // JO3-3: `readUnsignedShort` / `readFully` under the real body
            // throw a message-less `EOFException` (its only caller is
            // `readUTF`).
            if data_utf_units_exact() {
                return Err(eof_exception());
            }
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::IOException {
                    message: "readUTF: end of file".to_string(),
                },
            )));
        }
        filled += n;
    }
    Ok(())
}

/// `RandomAccessFile.writeUTF(String)` — the encoding half of the same record.
///
/// This triple had **no** registration in this file at all while `readUTF` had a
/// wrong one, which is the asymmetry that let the defect stand: nothing this
/// crate wrote could be read back through its own reader, so no round trip
/// existed to fail.
///
/// Two things the JDK specifies and a naive body gets wrong, both of which the
/// `nio_file.rs` twin still gets wrong (recorded, not fixed here — that file
/// belongs to another lane):
///
/// * the payload is **modified** UTF-8, so `U+0000` is `C0 80` and a
///   supplementary character is a six-byte surrogate pair, not `s.as_bytes()`;
/// * the 2-byte prefix is the length of THAT encoding, and
///   `DataOutput.writeUTF` says "If this number is larger than 65535, then a
///   `UTFDataFormatException` is thrown" — a refusal, never a truncation, since
///   a truncated record cuts at a byte index and can land mid-sequence.
pub(crate) fn native_raf_write_utf(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    if data_utf_units_exact() {
        // JO3-3 (round 15 wave 4, lane jdkonly5): the real body is
        // `DataOutputStream.writeUTF(str, this)` -- `writeUTF(null)` is an
        // NPE (it wrote an empty record), the text is encoded unit by unit,
        // and the refusal is a `UTFDataFormatException` with `tooLongMsg`'s
        // wording (it was a plain `IOException`). The refusal is decided
        // before any byte is written, as the JDK's is.
        let str_obj = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Err(write_utf_null_npe()),
        };
        let Some(fd) = raf_fd(ctx, this) else {
            return Ok(None);
        };
        let bytes = write_utf_payload_units(ctx, str_obj)?;
        let len = bytes.len() as u16;
        ctx.fd_table()
            .write_bytes(fd, &len.to_be_bytes())
            .map_err(io_err)?;
        ctx.fd_table().write_bytes(fd, &bytes).map_err(io_err)?;
        return Ok(None);
    }
    let s = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let Some(fd) = raf_fd(ctx, this) else {
        return Ok(None);
    };
    let bytes = encode_modified_utf8(&s);
    if bytes.len() > 65535 {
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IOException {
                message: format!(
                    "writeUTF: encoded string too long ({} bytes, max 65535)",
                    bytes.len()
                ),
            },
        )));
    }
    let len = bytes.len() as u16;
    ctx.fd_table()
        .write_bytes(fd, &len.to_be_bytes())
        .map_err(io_err)?;
    ctx.fd_table().write_bytes(fd, &bytes).map_err(io_err)?;
    Ok(None)
}

// --- CharArrayReader ---
pub(crate) const CAR_FIELD_BUF: usize = 0;
pub(crate) const CAR_FIELD_POS: usize = 1;
pub(crate) const CAR_FIELD_COUNT: usize = 2;

pub(crate) fn native_car_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let buf = args.get(1).copied().unwrap_or(Value::Object(None));
    let len = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.array_length(*o),
        _ => 0,
    };
    ctx.set_field(this, CAR_FIELD_BUF, buf);
    ctx.set_field(this, CAR_FIELD_POS, Value::Int(0));
    ctx.set_field(this, CAR_FIELD_COUNT, Value::Int(len as i32));
    Ok(None)
}

pub(crate) fn native_car_init_off(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let buf = args.get(1).copied().unwrap_or(Value::Object(None));
    let off = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let len = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    ctx.set_field(this, CAR_FIELD_BUF, buf);
    ctx.set_field(this, CAR_FIELD_POS, Value::Int(off));
    ctx.set_field(this, CAR_FIELD_COUNT, Value::Int(off + len));
    Ok(None)
}

/// `CharArrayReader.close()` — the real one nulls `buf`, and `ensureOpen()`
/// then makes every subsequent `read`/`ready` throw. Null the synthetic slot 0
/// (which also releases the backing `char[]`, the only resource this reader
/// holds) and zero the cursor; `native_car_read`/`native_car_ready` read a null
/// buffer as "closed" and throw. Idempotent — a second close finds slot 0
/// already null.
pub(crate) fn native_car_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.set_field(this, CAR_FIELD_BUF, Value::Object(None));
    ctx.set_field(this, CAR_FIELD_POS, Value::Int(0));
    ctx.set_field(this, CAR_FIELD_COUNT, Value::Int(0));
    Ok(None)
}

pub(crate) fn native_car_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    // `ensureOpen()`: a null `buf` means `close()` ran. Checked BEFORE the
    // cursor so a closed reader throws instead of quietly reporting EOF, which
    // is what distinguishes it from a merely exhausted one (buf still present,
    // `pos >= count`).
    let buf = match ctx.get_field(this, CAR_FIELD_BUF) {
        Value::Object(Some(o)) => o,
        _ => return Err(ioe_stream_closed()),
    };
    let pos = match ctx.get_field(this, CAR_FIELD_POS) {
        Value::Int(v) => v as usize,
        _ => 0,
    };
    let count = match ctx.get_field(this, CAR_FIELD_COUNT) {
        Value::Int(v) => v as usize,
        _ => 0,
    };
    if pos >= count {
        return Ok(Some(Value::Int(-1)));
    }
    let ch = match ctx.get_array_element(buf, pos) {
        Value::Int(v) => v,
        _ => -1,
    };
    ctx.set_field(this, CAR_FIELD_POS, Value::Int((pos + 1) as i32));
    Ok(Some(Value::Int(ch)))
}

pub(crate) fn native_car_ready(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // Same `ensureOpen()` check as `native_car_read`.
    if !matches!(ctx.get_field(this, CAR_FIELD_BUF), Value::Object(Some(_))) {
        return Err(ioe_stream_closed());
    }
    let pos = match ctx.get_field(this, CAR_FIELD_POS) {
        Value::Int(v) => v,
        _ => 0,
    };
    let count = match ctx.get_field(this, CAR_FIELD_COUNT) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(if pos < count { 1 } else { 0 })))
}

// --- CharArrayWriter ---
pub(crate) const CAW_FIELD_BUF: usize = 0;
pub(crate) const CAW_FIELD_COUNT: usize = 1;

pub(crate) fn native_caw_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let this_pin = ctx.pin_native_root(this);
    let buf = ctx.new_array(cratonvm_types::ArrayElementType::Char, 32);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, CAW_FIELD_BUF, Value::Object(Some(buf)));
    ctx.set_field(this, CAW_FIELD_COUNT, Value::Int(0));
    Ok(None)
}

/// Make `this`'s buffer hold at least `needed` chars.
///
/// gc-common w13-e (`common-w12e-native-io-write-side-growth-single-attempt`):
/// the growth reclaims and retries before its `OutOfMemoryError`, as the
/// JDK's `Arrays.copyOf` in `CharArrayWriter.write` does. The reclaim is a
/// moving collection, so `this` is `&mut` and comes back re-read through its
/// pin; the old buffer is pinned here. A caller must pin and re-read every
/// other `ObjectRef` it uses afterwards (`write(char[],int,int)`'s source).
pub(crate) fn caw_ensure_capacity(
    ctx: &mut dyn NativeContext,
    this: &mut ObjectRef,
    needed: usize,
) -> Result<(), MethodCallFailed> {
    let buf = match ctx.get_field(*this, CAW_FIELD_BUF) {
        Value::Object(Some(o)) => o,
        _ => return Ok(()),
    };
    let cap = ctx.array_length(buf);
    if needed <= cap {
        return Ok(());
    }
    // GC: `this` is read again for the count and written at the end, and the
    // old buffer is copied out — both across the growth allocation.
    let base = ctx.pin_native_root(*this);
    let buf_pin = ctx.pin_native_root(buf);
    let new_buf = match new_prim_array_reclaiming(
        ctx,
        cratonvm_types::ArrayElementType::Char,
        grown_array_capacity(cap, needed),
        "char[] (CharArrayWriter growth)",
    ) {
        Ok(new_buf) => new_buf,
        Err(oome) => {
            ctx.unpin_native_roots(base);
            return Err(oome);
        }
    };
    *this = ctx.read_native_pin(base, *this);
    let buf = ctx.read_native_pin(buf_pin, buf);
    let count = match ctx.get_field(*this, CAW_FIELD_COUNT) {
        Value::Int(v) => (v.max(0) as usize).min(cap),
        _ => 0,
    };
    if !ctx.bulk_array_copy(buf, 0, new_buf, 0, count) {
        for i in 0..count {
            let v = ctx.get_array_element(buf, i);
            ctx.set_array_element(new_buf, i, v);
        }
    }
    ctx.set_field(*this, CAW_FIELD_BUF, Value::Object(Some(new_buf)));
    ctx.unpin_native_roots(base);
    Ok(())
}

pub(crate) fn native_caw_write(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let this_pin = ctx.pin_native_root(this);
    let ch = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let count = match ctx.get_field(this, CAW_FIELD_COUNT) {
        Value::Int(v) => v as usize,
        _ => 0,
    };
    // gc-common w13-e: the growth can reclaim (a moving collection). `live`
    // audit: `this` is pinned and re-read; nothing else is held.
    let mut grown = this;
    if let Err(oome) = caw_ensure_capacity(ctx, &mut grown, count + 1) {
        ctx.unpin_native_roots(this_pin);
        return Err(oome);
    }
    let this = ctx.read_native_pin(this_pin, grown);
    let buf = match ctx.get_field(this, CAW_FIELD_BUF) {
        Value::Object(Some(o)) => o,
        _ => return Ok(None),
    };
    ctx.set_array_element(buf, count, Value::Int(ch));
    ctx.set_field(this, CAW_FIELD_COUNT, Value::Int((count + 1) as i32));
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub(crate) fn native_caw_write_bulk(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let this_pin = ctx.pin_native_root(this);
    let src = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let src_pin = ctx.pin_native_root(src);
    let off_i = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let len_i = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    // `Objects.checkFromIndexSize(off, len, c.length)`, as the JDK's
    // `CharArrayWriter.write(char[],int,int)` does first. Without it a
    // negative `len` became a ~2^64 `usize`, `count + len` overflowed, and the
    // copy loop ran off the end of both arrays (gc-common w13-e, found while
    // auditing this growth call site).
    if let Err(e) = check_array_bounds(off_i, len_i, ctx.array_length(src)) {
        ctx.unpin_native_roots(this_pin);
        return Err(e);
    }
    let off = off_i as usize;
    let len = len_i as usize;
    let count = match ctx.get_field(this, CAW_FIELD_COUNT) {
        Value::Int(v) => v as usize,
        _ => 0,
    };
    // gc-common w13-e: the growth can reclaim (a moving collection). `live`
    // audit: `this` and the source `src` are pinned and re-read after it.
    let mut grown = this;
    if let Err(oome) = caw_ensure_capacity(ctx, &mut grown, count + len) {
        ctx.unpin_native_roots(this_pin);
        return Err(oome);
    }
    let this = ctx.read_native_pin(this_pin, grown);
    let buf = match ctx.get_field(this, CAW_FIELD_BUF) {
        Value::Object(Some(o)) => o,
        _ => return Ok(None),
    };
    let src = ctx.read_native_pin(src_pin, src);
    if !ctx.bulk_array_copy(src, off, buf, count, len) {
        for i in 0..len {
            let v = ctx.get_array_element(src, off + i);
            ctx.set_array_element(buf, count + i, v);
        }
    }
    ctx.set_field(this, CAW_FIELD_COUNT, Value::Int((count + len) as i32));
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub(crate) fn native_caw_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let buf = match ctx.get_field(this, CAW_FIELD_BUF) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let count = match ctx.get_field(this, CAW_FIELD_COUNT) {
        Value::Int(v) => v as usize,
        _ => 0,
    };
    let mut chars = Vec::with_capacity(count);
    for i in 0..count {
        if let Value::Int(ch) = ctx.get_array_element(buf, i) {
            chars.push(ch as u16);
        }
    }
    let s = String::from_utf16_lossy(&chars);
    let result = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_caw_to_char_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let buf = match ctx.get_field(this, CAW_FIELD_BUF) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let buf_pin = ctx.pin_native_root(buf);
    let count = match ctx.get_field(this, CAW_FIELD_COUNT) {
        Value::Int(v) => v as usize,
        _ => 0,
    };
    // gc-common w12-e: the `char[]` twin of `ByteArrayOutputStream.toByteArray`
    // (see `native_baos_to_byte_array`): caller-sized, so reclaim and retry
    // before the `OutOfMemoryError`. `live` audit: `buf` is pinned above and
    // re-read below; `this` is not used after this point. Registered-only
    // (`CharArrayWriter.toCharArray`).
    let arr = match new_prim_array_reclaiming(
        ctx,
        cratonvm_types::ArrayElementType::Char,
        count,
        "char[] (CharArrayWriter.toCharArray)",
    ) {
        Ok(arr) => arr,
        Err(oome) => {
            ctx.unpin_native_roots(buf_pin);
            return Err(oome);
        }
    };
    let buf = ctx.read_native_pin(buf_pin, buf);
    if !ctx.bulk_array_copy(buf, 0, arr, 0, count) {
        for i in 0..count {
            let v = ctx.get_array_element(buf, i);
            ctx.set_array_element(arr, i, v);
        }
    }
    ctx.unpin_native_roots(buf_pin);
    Ok(Some(Value::Object(Some(arr))))
}

pub(crate) fn native_caw_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(ctx.get_field(this, CAW_FIELD_COUNT)))
}

pub(crate) fn native_caw_reset(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.set_field(this, CAW_FIELD_COUNT, Value::Int(0));
    Ok(None)
}

// --- LineNumberReader ---
pub(crate) const LNR_FIELD_IN: usize = 0;
pub(crate) const LNR_FIELD_LINE_NUM: usize = 1;
pub(crate) const LNR_FIELD_POS: usize = 2;
pub(crate) const LNR_FIELD_CONTENT: usize = 3;

pub(crate) fn native_lnr_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.set_field(
        this,
        LNR_FIELD_IN,
        args.get(1).copied().unwrap_or(Value::Object(None)),
    );
    ctx.set_field(this, LNR_FIELD_LINE_NUM, Value::Int(0));
    ctx.set_field(this, LNR_FIELD_POS, Value::Int(0));
    ctx.set_field(this, LNR_FIELD_CONTENT, Value::Object(None));
    Ok(None)
}

pub(crate) fn native_lnr_read_line(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let this_pin = ctx.pin_native_root(this);
    // Try to read from underlying reader via StringReader-like content field
    let content = match ctx.get_field(this, LNR_FIELD_CONTENT) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => {
            // Try to read from inner reader if it's a StringReader
            let inner = match ctx.get_field(this, LNR_FIELD_IN) {
                Value::Object(Some(o)) => o,
                _ => return Ok(Some(Value::Object(None))),
            };
            // Read all content from inner (assume StringReader layout)
            let all = match ctx.get_field(inner, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => return Ok(Some(Value::Object(None))),
            };
            let s = ctx.create_string(&all);
            let this = ctx.read_native_pin(this_pin, this);
            ctx.set_field(this, LNR_FIELD_CONTENT, Value::Object(Some(s)));
            all
        }
    };
    let pos = match ctx.get_field(this, LNR_FIELD_POS) {
        Value::Int(v) => v as usize,
        _ => 0,
    };
    if pos >= content.len() {
        return Ok(Some(Value::Object(None)));
    }
    let rest = &content[pos..];
    let (line, advance) = if let Some(nl) = rest.find('\n') {
        let line_end = if nl > 0 && rest.as_bytes().get(nl - 1) == Some(&b'\r') {
            nl - 1
        } else {
            nl
        };
        (&rest[..line_end], nl + 1)
    } else {
        (rest, rest.len())
    };
    ctx.set_field(this, LNR_FIELD_POS, Value::Int((pos + advance) as i32));
    let line_num = match ctx.get_field(this, LNR_FIELD_LINE_NUM) {
        Value::Int(v) => v,
        _ => 0,
    };
    ctx.set_field(this, LNR_FIELD_LINE_NUM, Value::Int(line_num + 1));
    let result = ctx.create_string(line);
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_lnr_get_line_number(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(ctx.get_field(this, LNR_FIELD_LINE_NUM)))
}

pub(crate) fn native_lnr_set_line_number(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.set_field(
        this,
        LNR_FIELD_LINE_NUM,
        args.get(1).copied().unwrap_or(Value::Int(0)),
    );
    Ok(None)
}

// ===========================================================================
// Phase 40: BufferedInputStream, BufferedOutputStream, PipedInputStream,
//           PipedOutputStream
// ===========================================================================

// BufferedInputStream: 4-field synthetic (in=0, buf=1 byte[], pos=2, count=3)
pub(crate) const BIS_FIELD_IN: usize = 0;
pub(crate) const BIS_FIELD_BUF: usize = 1;
pub(crate) const BIS_FIELD_POS: usize = 2;
pub(crate) const BIS_FIELD_COUNT: usize = 3;
pub(crate) const _BIS_NUM_FIELDS: usize = 4;

// BufferedOutputStream: 3-field synthetic (out=0, buf=1 byte[], count=2).
// Real-JDK BufferedOutputStream extends FilterOutputStream which has an
// extra `closed:boolean` field at slot 1, pushing `buf` to slot 2 and
// `count` to slot 3 in the real layout. Use bos_slots() at runtime to
// pick the right slot indices; falls back to the legacy 0/1/2 when the
// class isn't resolvable (synthetic-stub mode).
pub(crate) const BOS_FIELD_OUT: usize = 0;
pub(crate) const BOS_FIELD_BUF: usize = 1;
pub(crate) const BOS_FIELD_COUNT: usize = 2;
pub(crate) const _BOS_NUM_FIELDS: usize = 3;

/// Resolve BufferedOutputStream's `out` (inherited from FilterOutputStream),
/// `buf` and `count` slot indices via the real-JDK class metadata, falling
/// back to the legacy synthetic layout when the class isn't loaded as a
/// real-JDK class.
///
/// `out` MUST be resolved, not hardcoded to 0: in the real-JDK compact layout
/// `buf` can land at slot 0, so storing `out` at a hardcoded slot 0 in
/// `native_bos_init` and then `buf` at the resolved `buf` slot 0 CLOBBERS
/// `out` with the byte[] buffer. A subsequent `out.write(int)` then dispatches
/// `write(I)V` against the byte[] (whose only methods are Object's) →
/// `NoSuchMethodError: java/lang/Object.write(I)V` (seen wrapping a real
/// java.net.Socket output stream under CRATONVM_REAL_NET_SOCKETS).
pub(crate) fn bos_slots(ctx: &dyn NativeContext) -> (usize, usize, usize) {
    let out = ctx
        .resolve_field_index("java/io/BufferedOutputStream", "out")
        .or_else(|| ctx.resolve_field_index("java/io/FilterOutputStream", "out"))
        .unwrap_or(BOS_FIELD_OUT);
    let buf = ctx
        .resolve_field_index("java/io/BufferedOutputStream", "buf")
        .unwrap_or(BOS_FIELD_BUF);
    let count = ctx
        .resolve_field_index("java/io/BufferedOutputStream", "count")
        .unwrap_or(BOS_FIELD_COUNT);
    (out, buf, count)
}

pub(crate) fn bos_has_buffer_slots(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    buf_slot: usize,
    count_slot: usize,
) -> bool {
    let fields = ctx.object_num_fields(this);
    buf_slot < fields && count_slot < fields
}

pub(crate) fn bos_inner(ctx: &dyn NativeContext, this: ObjectRef, out_slot: usize) -> Option<ObjectRef> {
    if out_slot >= ctx.object_num_fields(this) {
        return None;
    }
    match ctx.get_field(this, out_slot) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

/// The pending bytes of one side-buffered stream: a receiver with no
/// `buf`/`count` slots of its own. In practice that is `PipedOutputStream`,
/// whose natives below share these (a real `PipedOutputStream` declares only
/// `sink`), plus any fabricated `BufferedOutputStream` stub narrower than the
/// synthetic layout.
///
/// gc-common w14-e (`common-w13e-bos-side-buffers-process-wide-keyed-by-identity-hash`):
///
/// - The table is keyed by `(vm_identity, identity_hash_code)`
///   ([`bos_side_key`]), the convention `ScanKey` / `RafKey` document above.
///   It used to be keyed by the bare 31-bit hash, so two VMs' streams with
///   equal hashes (`libcratonvm`, `cratonvm-embed`, every test binary)
///   buffered into ONE row and flushed each other's bytes.
/// - Each row names its stream (`owner`), the weak owner that
///   [`gc_sweep_io_side_tables`] judges after a collection. A stream dropped
///   without `close()` used to keep its row, and its unflushed bytes, for the
///   life of the process; `PipedOutputStream.close()` never evicted at all.
///   The sweep drops a dead stream's row and discards its bytes, exactly as
///   HotSpot discards an unclosed `BufferedOutputStream`'s `buf`.
/// - [`forget_vm_io_side_tables`] drops a disposed VM's rows.
///
/// Since gc-common w29-c the table key is the full [`IoRowKey`] (the bucket
/// plus the stream's current address), so two LIVE streams of one VM with
/// equal identity hashes no longer buffer into, and flush, one row. The old
/// objection -- "a lookup that missed on a stale `owner` would throw away a
/// LIVE stream's unflushed bytes" -- no longer holds: the stop-the-world
/// epilogue re-files a moved stream's row before any mutator resumes (see
/// [`IoRowKey`]). The natives below compute the bucket once and read the
/// owner from a CURRENT receiver at every table access, re-reading `this`
/// through its pin after each Java call.
///
/// No `ObjectRef` is stored: `owner` is a plain address, re-addressed by the
/// sweep through the collection's pointer map, and never dereferenced.
pub(crate) struct BosSideBuffer {
    pub(crate) bytes: Vec<u8>,
    pub(crate) capacity: usize,
    /// The stream's address: where the last native call on it found it, or
    /// where the last collection moved it.
    pub(crate) owner: usize,
}

/// `BufferedOutputStream`'s `DEFAULT_INITIAL_BUFFER_SIZE`, and the capacity of
/// a row created by a write that reached a stream no native constructor saw.
pub(crate) const BOS_SIDE_DEFAULT_CAPACITY: usize = 8192;

impl BosSideBuffer {
    pub(crate) fn new(capacity: usize, owner: usize) -> Self {
        Self {
            // At most one default buffer is reserved up front. `capacity` is
            // the caller's `new BufferedOutputStream(out, size)`, and a
            // `Vec::with_capacity(Integer.MAX_VALUE)` the host allocator
            // refuses ABORTS the process (`handle_alloc_error`) where HotSpot
            // raises an `OutOfMemoryError`. The Vec grows as bytes arrive, and
            // a flush empties it before it passes `capacity`.
            bytes: Vec::with_capacity(capacity.min(BOS_SIDE_DEFAULT_CAPACITY)),
            capacity,
            owner,
        }
    }
}

impl IoWeakRow for BosSideBuffer {
    fn owner(&self) -> usize {
        self.owner
    }
    fn set_owner(&mut self, owner: usize) {
        self.owner = owner;
    }
}

/// `(vm_identity, identity_hash_code)`: a side buffer's bucket; the table key
/// is its [`IoRowKey`] (gc-common w29-c). See [`BosSideBuffer`].
pub(crate) type BosSideKey = IoSideKey;

pub(crate) fn bos_side_buffers() -> &'static Mutex<HashMap<IoRowKey, BosSideBuffer>> {
    static BUFS: OnceLock<Mutex<HashMap<IoRowKey, BosSideBuffer>>> = OnceLock::new();
    BUFS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Computed before the table lock is taken, never inside its expression. The
/// bucket survives a Java call; pair it with the receiver's CURRENT address
/// ([`bos_side_owner`]) at each table access.
pub(crate) fn bos_side_key(ctx: &dyn NativeContext, this: ObjectRef) -> BosSideKey {
    io_side_key(ctx, this)
}

/// The address a row records as its owner. Only meaningful for a `this` that
/// is current: read before any Java call, or re-read through a pin after one.
pub(crate) fn bos_side_owner(this: ObjectRef) -> usize {
    io_side_owner(this)
}

/// The row of the stream in `key`'s bucket at `owner` (its current address).
/// A write that reaches a stream no native constructor initialised creates
/// the row, with `capacity`.
pub(crate) fn bos_side_row(
    bufs: &mut HashMap<IoRowKey, BosSideBuffer>,
    key: BosSideKey,
    owner: usize,
    capacity: usize,
) -> &mut BosSideBuffer {
    bufs.entry(io_row_key(key, owner))
        .or_insert_with(|| BosSideBuffer::new(capacity, owner))
}

/// Put bytes a failed flush took back in front of the row's, so a later flush
/// delivers them in order. `BufferedOutputStream.flushBuffer()` is
/// `out.write(buf, 0, count); count = 0;`: when the write throws, `count` is
/// unchanged and the bytes stay buffered. A row closed meanwhile keeps nothing.
/// `owner` is the stream's CURRENT address (re-read after the failed call).
pub(crate) fn bos_side_restore(key: BosSideKey, owner: usize, mut bytes: Vec<u8>) {
    let mut bufs = bos_side_buffers().lock();
    if let Some(row) = bufs.get_mut(&io_row_key(key, owner)) {
        bytes.extend_from_slice(&row.bytes);
        row.bytes = bytes;
    }
}

pub(crate) fn bos_side_init(ctx: &dyn NativeContext, this: ObjectRef, capacity: usize) {
    let key = io_side_row_key(ctx, this);
    let row = BosSideBuffer::new(capacity.max(1), bos_side_owner(this));
    bos_side_buffers().lock().insert(key, row);
}

pub(crate) fn bos_side_flush(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    out_slot: usize,
    flush_inner: bool,
) -> MethodCallResult {
    let key = bos_side_key(ctx, this);
    let bytes = {
        let mut bufs = bos_side_buffers().lock();
        bufs.get_mut(&io_row_key(key, bos_side_owner(this)))
            .map(|state| std::mem::take(&mut state.bytes))
            .unwrap_or_default()
    };
    if bos_inner(ctx, this, out_slot).is_none() {
        return Ok(None);
    }
    // GC (gc-common w14-e): the allocation and both Java calls below can move
    // `this` and the stream it wraps. `this` is pinned and re-read after each,
    // and `out` is re-read from it: the wrapped stream used to be held as a
    // bare local across the allocation and across `write`, and was then the
    // receiver of `flush`.
    let this_pin = ctx.pin_native_root(this);
    let mut this = this;
    if !bytes.is_empty() {
        // Caller-sized (up to the stream's capacity), so a refusal reclaims
        // and retries before the `OutOfMemoryError`, the ladder w13-e gave
        // the sized constructor. This was the infallible `new_array`. `live`
        // audit: `this` is pinned; the bytes are Rust-side.
        let arr = match new_byte_array_reclaiming(
            ctx,
            bytes.len(),
            "byte[] (BufferedOutputStream side-buffer flush)",
        ) {
            Ok(arr) => arr,
            Err(oome) => {
                // The reclaiming allocation may have collected: the row is
                // filed under the stream's CURRENT address (gc-common w29-c).
                let owner = bos_side_owner(ctx.read_native_pin(this_pin, this));
                bos_side_restore(key, owner, bytes);
                ctx.unpin_native_roots(this_pin);
                return Err(oome);
            }
        };
        // One bulk copy (the VM override is a memcpy) where this used to box
        // one `Value` per byte.
        let copied = ctx.write_byte_array_from(arr, 0, &bytes);
        debug_assert!(copied, "a fresh byte[{}] holds the buffer", bytes.len());
        this = ctx.read_native_pin(this_pin, this);
        let Some(inner) = bos_inner(ctx, this, out_slot) else {
            ctx.unpin_native_roots(this_pin);
            return Ok(None);
        };
        let written = ctx.invoke_virtual(
            inner,
            "write",
            "([BII)V",
            &[
                Value::Object(Some(arr)),
                Value::Int(0),
                Value::Int(bytes.len() as i32),
            ],
        );
        if let Err(e) = written {
            let owner = bos_side_owner(ctx.read_native_pin(this_pin, this));
            bos_side_restore(key, owner, bytes);
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
        this = ctx.read_native_pin(this_pin, this);
    }
    if flush_inner {
        if let Some(inner) = bos_inner(ctx, this, out_slot) {
            if let Err(e) = ctx.invoke_virtual(inner, "flush", "()V", &[]) {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        }
    }
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub(crate) fn bos_side_write_byte(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    out_slot: usize,
    byte_val: i32,
) -> MethodCallResult {
    let key = bos_side_key(ctx, this);
    let mut owner = bos_side_owner(this);
    let should_flush = {
        let mut bufs = bos_side_buffers().lock();
        let state = bos_side_row(&mut bufs, key, owner, BOS_SIDE_DEFAULT_CAPACITY);
        state.bytes.len() >= state.capacity
    };
    if should_flush {
        // The flush runs Java and can move `this`; the row is re-stamped
        // below with the address it has afterwards.
        let this_pin = ctx.pin_native_root(this);
        let flushed = bos_side_flush(ctx, this, out_slot, false);
        owner = bos_side_owner(ctx.read_native_pin(this_pin, this));
        ctx.unpin_native_roots(this_pin);
        flushed?;
    }
    let mut bufs = bos_side_buffers().lock();
    bos_side_row(&mut bufs, key, owner, BOS_SIDE_DEFAULT_CAPACITY)
        .bytes
        .push((byte_val & 0xFF) as u8);
    Ok(None)
}

pub(crate) fn bos_side_write_bulk(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    out_slot: usize,
    src: ObjectRef,
    off: usize,
    len: usize,
) -> MethodCallResult {
    let key = bos_side_key(ctx, this);
    let capacity = {
        let mut bufs = bos_side_buffers().lock();
        let row = bos_side_row(
            &mut bufs,
            key,
            bos_side_owner(this),
            BOS_SIDE_DEFAULT_CAPACITY,
        );
        row.capacity
    };
    // GC: `bos_side_flush` re-enters Java (it calls `write` on the wrapped
    // stream), so both `this` and the caller's `src` array are pre-call
    // addresses from here down. An error return leaves the pins to
    // `safe_native_call`, which truncates to the native-call floor.
    let base = ctx.pin_native_root(this);
    let src_pin = ctx.pin_native_root(src);
    if len >= capacity {
        bos_side_flush(ctx, this, out_slot, false)?;
        let this = ctx.read_native_pin(base, this);
        if let Some(inner) = bos_inner(ctx, this, out_slot) {
            let src = ctx.read_native_pin(src_pin, src);
            ctx.invoke_virtual(
                inner,
                "write",
                "([BII)V",
                &[
                    Value::Object(Some(src)),
                    Value::Int(off as i32),
                    Value::Int(len as i32),
                ],
            )?;
        }
        ctx.unpin_native_roots(base);
        return Ok(None);
    }

    // `this` is still current: nothing since the top of this function can
    // collect (the pins above are not GC points).
    let row_key = io_row_key(key, bos_side_owner(this));
    let needs_flush = {
        let bufs = bos_side_buffers().lock();
        bufs.get(&row_key)
            .map(|state| len > state.capacity.saturating_sub(state.bytes.len()))
            .unwrap_or(false)
    };
    if needs_flush {
        bos_side_flush(ctx, this, out_slot, false)?;
    }

    let mut bytes = vec![0u8; len];
    let src = ctx.read_native_pin(src_pin, src);
    ctx.read_byte_array_into(src, off, &mut bytes);
    let owner = bos_side_owner(ctx.read_native_pin(base, this));
    ctx.unpin_native_roots(base);
    let mut bufs = bos_side_buffers().lock();
    bos_side_row(&mut bufs, key, owner, capacity)
        .bytes
        .extend_from_slice(&bytes);
    Ok(None)
}

/// The VM's post-collection hook for native-io's weak side tables (gc-common
/// w14-e): drop `vm`'s rows whose owning object did not survive a collection,
/// re-address the ones whose owner moved, and return how many were dropped.
/// Other VMs' rows are never judged.
///
/// - `pointer_map`: the collection's `old -> new` map. A row whose owner is a
///   key follows it and is kept. Pass an empty map for a reclamation that
///   moved nothing.
/// - `is_live(addr)`: the verdict for an owner the map does not name, as the
///   table holds it. Inside a stop-the-world epilogue that is
///   `addr_keyed::survived_in_place`; for a concurrent reclamation the same,
///   or a pure "not in a span this reclamation freed" range test. It is the
///   `is_live` contract of `native-builtins`' `gc_sweep_locale_rows` (the
///   wave-11 weak-row mechanism), and it is called from the same three
///   places: `process_references_after_gc` and both `addr_keyed.rs` sweeps
///   (`docs/feature-designs/profile-guided-inlining.md`).
///
/// The tables, all keyed by [`IoRowKey`] (`(vm_identity, identity_hash_code,
/// owner address)` since gc-common w29-c; a moved owner's rows are re-filed
/// under its new address here, before any mutator resumes):
///
/// - the `BufferedOutputStream` / `PipedOutputStream` side buffers
///   (gc-common w14-e; through [`sweep_io_weak_rows`] since w29-c);
/// - since gc-common w15-c
///   (`common-w14e-reader-and-watch-side-tables-keyed-by-bare-identity-hash`),
///   through [`sweep_io_weak_rows`]: the `InputStreamReader` UTF-8 carry-over
///   (`ISR_PENDING`), the `BufferedReader` fill buffers (`br_buf_table`), the
///   `StringReader` content (`SR_STATE`) and the `WatchService` platform
///   watchers (`WATCH_SERVICES`). A dead `WatchService`'s watcher is CLOSED
///   (dropped) here, after every table lock is released.
/// - since gc-common w16-c
///   (`common-w15c-native-io-identity-keyed-tables-outside-the-weak-sweep`):
///   the `StreamDecoder` / `StreamEncoder` state (`stream_decoder.rs`
///   `sd_table`, `stream_encoder.rs` `se_table`), the `Scanner` input and
///   last match (`SCAN_SOURCES`, `SCAN_MATCHES`), the `DatagramChannel` fd,
///   connected, non-blocking and option rows (`dc_fds`,
///   `dc_connected_channels`, `dc_nonblocking_channels`, `dc_option_state`)
///   and the synthetic `RandomAccessFile` rows (`raf_states`). A dead
///   channel's or file's fd cannot be closed here (the fd table is reachable
///   only through a `NativeContext`), so it is parked on [`io_orphan_fds`]
///   for that VM's next DatagramChannel / synthetic-RAF native.
///
/// A later native-io table of the same shape joins here, so the VM keeps one
/// call per site.
pub fn gc_sweep_io_side_tables(
    vm: usize,
    pointer_map: &cratonvm_types::PointerMap,
    is_live: &dyn Fn(usize) -> bool,
) -> usize {
    let mut dropped = sweep_io_weak_rows(bos_side_buffers(), vm, pointer_map, is_live).len();
    dropped += sweep_io_weak_rows(isr_pending(), vm, pointer_map, is_live).len();
    dropped += sweep_io_weak_rows(br_buf_table(), vm, pointer_map, is_live).len();
    dropped += sweep_io_weak_rows(sr_state(), vm, pointer_map, is_live).len();
    let dead_watchers = sweep_io_weak_rows(watch_services(), vm, pointer_map, is_live);
    dropped += dead_watchers.len();
    close_dropped_watch_services(dead_watchers);
    dropped += crate::stream_decoder::gc_sweep_decoder_rows(vm, pointer_map, is_live);
    dropped += crate::stream_encoder::gc_sweep_encoder_rows(vm, pointer_map, is_live);
    dropped += sweep_io_weak_rows(scan_sources(), vm, pointer_map, is_live).len();
    dropped += sweep_io_weak_rows(scan_matches(), vm, pointer_map, is_live).len();
    let dead_channels = sweep_io_weak_rows(dc_fds(), vm, pointer_map, is_live);
    dropped += dead_channels.len();
    // Generation-checked (JIT r23 w4b L6): a dead channel's socket may have
    // been closed meanwhile and its id reused.
    queue_orphan_fds_checked(
        vm,
        dead_channels.into_iter().map(|row| (row.fd, row.gen)).collect(),
    );
    dropped += sweep_io_weak_rows(dc_connected_channels(), vm, pointer_map, is_live).len();
    dropped += sweep_io_weak_rows(dc_nonblocking_channels(), vm, pointer_map, is_live).len();
    dropped += sweep_io_weak_rows(dc_option_state(), vm, pointer_map, is_live).len();
    let dead_files = sweep_io_weak_rows(raf_states(), vm, pointer_map, is_live);
    dropped += dead_files.len();
    // Generation-checked (JIT r23 wave 3 review): a synthetic RAF's fd can be
    // closed meanwhile through its `getChannel()`, and the OS may have handed
    // the value to a later file.
    queue_orphan_fds_checked(
        vm,
        dead_files
            .into_iter()
            .map(|row| (row.fd, row.generation))
            .collect(),
    );
    dropped
}

/// Drop every row `vm` filed in native-io's weak side tables (VM teardown,
/// from `vm/src/vm/vm_init.rs::release_vm_native_state`). Returns how many
/// were dropped. Idempotent. The teardown twin of [`gc_sweep_io_side_tables`]:
/// the same tables, and a disposed VM's `WatchService` watchers are closed.
///
/// Since gc-common w16-c also the `DatagramChannel.socket()` fallback cache
/// (`dc_socket_cache`, whose global roots die with the VM's root table) and
/// the VM's parked orphan fds (`io_orphan_fds`). No fd is closed here: the
/// VM's fd table, and every OS handle in it, is dropped with the VM.
pub fn forget_vm_io_side_tables(vm: usize) -> usize {
    let mut dropped = forget_vm_io_weak_rows(bos_side_buffers(), vm).len();
    dropped += forget_vm_io_weak_rows(isr_pending(), vm).len();
    dropped += forget_vm_io_weak_rows(br_buf_table(), vm).len();
    dropped += forget_vm_io_weak_rows(sr_state(), vm).len();
    let dead_watchers = forget_vm_io_weak_rows(watch_services(), vm);
    dropped += dead_watchers.len();
    close_dropped_watch_services(dead_watchers);
    dropped += crate::stream_decoder::forget_vm_decoder_rows(vm);
    dropped += crate::stream_encoder::forget_vm_encoder_rows(vm);
    dropped += forget_vm_io_weak_rows(scan_sources(), vm).len();
    dropped += forget_vm_io_weak_rows(scan_matches(), vm).len();
    dropped += forget_vm_io_weak_rows(dc_fds(), vm).len();
    dropped += forget_vm_io_weak_rows(dc_connected_channels(), vm).len();
    dropped += forget_vm_io_weak_rows(dc_nonblocking_channels(), vm).len();
    dropped += forget_vm_io_weak_rows(dc_option_state(), vm).len();
    dropped += forget_vm_io_weak_rows(dc_socket_cache(), vm).len();
    dropped += forget_vm_io_weak_rows(raf_states(), vm).len();
    io_orphan_fds().lock().remove(&vm);
    // gc-common w20-f: the VM's cached jimage arrays (global roots in its own
    // table). Not counted: they are a cache, not a weak side-table row. (The
    // direct mapping is the process's and is never unmapped: zr z8/fm.)
    jimage_map_cache()
        .lock()
        .retain(|(owner, _), _| *owner != vm);
    dropped
}

/// `(vm_identity, identity_hash_code)`: the BUCKET of a native-io weak
/// side-table row (gc-common w15-c, after `BosSideKey`, `ScanKey` and
/// `RafKey`). Since gc-common w29-c a row is found by its full [`IoRowKey`],
/// the bucket plus its owner's current address.
///
/// Identity hashes are per VM, so a bare hash made two VMs' objects share a
/// row (`libcratonvm`, `cratonvm-embed`, every test binary): one VM's
/// `StringReader.<init>` replaced another's live content, and one VM's
/// `WatchService.close()` tore down another's watcher. The hash survives a
/// moving collection and the address does not, so a bucket computed before a
/// GC point stays valid after it; the owner half of an [`IoRowKey`] must be
/// read from a CURRENT reference (see [`io_side_owner`]).
pub(crate) type IoSideKey = (usize, i32);

/// `(vm_identity, identity_hash_code, owner address)`: the key of every
/// native-io weak side table (gc-common w29-c,
/// `common-w28b-remaining-identity-hash-keyed-side-tables` rank 8, route R2).
///
/// The hash alone is not an object's identity: identity hashes are 32 bits,
/// minted lazily from a per-heap counter that wraps, so two LIVE objects of
/// one VM can share one. Keyed by [`IoSideKey`] alone, such a pair shared one
/// row: a `RandomAccessFile` or `DatagramChannel` read, wrote and closed the
/// other's descriptor, a reader drained the other's buffered input, a stream
/// flushed its bytes into the other's sink. Adding the owner's current address
/// gives each live object its own row; a dead object's row can no longer be
/// served to a newcomer either (it would need the same hash AND the same
/// address before the sweep ran).
///
/// Why comparing the address is sound (it was not, when the rows first named
/// their owner): no backend lets a native see an object at its new address
/// before the row is re-addressed. Every moving collection goes through the
/// one stop-the-world door, whose epilogue
/// (`vm/src/runtime/interpreter/gc_and_alloc.rs::process_references_after_gc`)
/// calls [`gc_sweep_io_side_tables`] with the collection's pointer map before
/// any mutator resumes, and that sweep re-files every moved row under its new
/// address ([`sweep_io_weak_rows`]); the `ZipFile` / `JarFile` tables go the
/// same way through `native-builtins`' `zip_streams::gc_sweep_zip_rows` ->
/// `zip_real_jar::gc_sweep_jar_rows`, in the same epilogue. ZGC relocates only
/// in that pause, G1's concurrent phase only marks, and the Generational
/// concurrent old-gen cycle is mark-sweep, whose two hooks
/// (`vm/src/memory/addr_keyed.rs`) call the sweep with nothing moved. The
/// lock-key registry relies on the same argument
/// (`docs/feature-designs/real-jdk-mode.md`).
/// So a lookup that misses on the address is a caller holding a STALE copy of
/// a moved object, a bug the stale-handle / stale-receiver audits gate: it
/// loses that object's row for the call, never serves another object's.
pub(crate) type IoRowKey = (usize, i32, usize);

/// Computed before the table lock is taken, never inside its expression.
pub(crate) fn io_side_key(ctx: &dyn NativeContext, this: ObjectRef) -> IoSideKey {
    (ctx.vm_identity(), ctx.identity_hash_code(this))
}

/// The row key of the object in `bucket` whose current address is `owner`.
#[inline]
pub(crate) fn io_row_key(bucket: IoSideKey, owner: usize) -> IoRowKey {
    (bucket.0, bucket.1, owner)
}

/// [`io_row_key`] for a `this` that is CURRENT (see [`io_side_owner`]).
/// Computed before the table lock is taken, never inside its expression.
pub(crate) fn io_side_row_key(ctx: &dyn NativeContext, this: ObjectRef) -> IoRowKey {
    io_row_key(io_side_key(ctx, this), io_side_owner(this))
}

/// The address a row records as its owner. Only meaningful for a `this` that
/// is current: read before any GC-capable step, or re-read through a pin
/// after one. A stale address would miss the object's row (and, as a row's
/// owner, let the sweep judge a LIVE owner dead).
pub(crate) fn io_side_owner(this: ObjectRef) -> usize {
    this.as_ptr() as usize
}

/// A table key that names the VM that filed the row: [`IoRowKey`] for the
/// weak rows, [`IoSideKey`] for the `DatagramChannel.socket()` fallback cache
/// (a bucket of rows that carry their own discriminator).
pub(crate) trait IoTableKey: Copy + Eq + std::hash::Hash {
    fn vm(&self) -> usize;
}

impl IoTableKey for IoSideKey {
    fn vm(&self) -> usize {
        self.0
    }
}

impl IoTableKey for IoRowKey {
    fn vm(&self) -> usize {
        self.0
    }
}

/// A row of a weak native-io side table: it names the address of the object
/// that owns it, which [`sweep_io_weak_rows`] follows through a collection's
/// pointer map or judges dead. No `ObjectRef` is stored; the owner is a plain
/// address that is never dereferenced.
///
/// The owner is also the third element of the row's [`IoRowKey`], and the two
/// always agree: a row is filed under its owner's address and re-filed by the
/// sweep when the owner moves. Lookups therefore DO compare the owner with the
/// receiver (gc-common w29-c). This used to be refused on the ground that "a
/// lookup that missed on a stale owner would throw away a LIVE object's
/// state"; that premise fell when the stop-the-world epilogue began
/// re-addressing every row before any mutator resumes (see [`IoRowKey`]), and
/// the refusal left two LIVE objects of one VM with equal identity hashes
/// sharing a row.
pub(crate) trait IoWeakRow {
    fn owner(&self) -> usize;
    fn set_owner(&mut self, owner: usize);
}

/// The sweep half of a weak native-io side table: re-file `vm`'s rows whose
/// owner moved (a key of `pointer_map`) under the owner's new address, drop
/// the rows whose owner `is_live` judges dead, and hand the dropped rows back
/// so the caller can release what they own (a `WatchService`'s watcher, a
/// channel's or file's fd) with no table lock held.
///
/// - Every moved row is taken out before any is re-filed, so a chain of moves
///   within one collection (A to where B was, B elsewhere) re-files each row
///   exactly once. A row found at a moved row's destination belonged to an
///   object that died there (a live object cannot overlap one moved in), so
///   it is displaced and handed back as dropped; moved rows are never judged.
/// - `is_live` is evaluated with the table lock RELEASED, so the predicate may
///   take heap locks without nesting under this one. A dead row is removed by
///   its full key, which names the owner that was judged.
/// - Other VMs' rows are never judged or moved.
pub(crate) fn sweep_io_weak_rows<R: IoWeakRow, S: std::hash::BuildHasher>(
    table: &Mutex<HashMap<IoRowKey, R, S>>,
    vm: usize,
    pointer_map: &cratonvm_types::PointerMap,
    is_live: &dyn Fn(usize) -> bool,
) -> Vec<R> {
    let mut dropped = Vec::new();
    let judged: Vec<IoRowKey> = {
        let mut rows = table.lock();
        let mut judged = Vec::new();
        let mut moved: Vec<(IoRowKey, usize)> = Vec::new();
        for key in rows.keys() {
            if key.0 != vm {
                continue;
            }
            match pointer_map.get(&key.2) {
                Some(&to) => moved.push((*key, to)),
                None => judged.push(*key),
            }
        }
        if !moved.is_empty() {
            let taken: Vec<(IoRowKey, R)> = moved
                .into_iter()
                .filter_map(|(key, to)| {
                    rows.remove(&key).map(|mut row| {
                        row.set_owner(to);
                        (io_row_key((key.0, key.1), to), row)
                    })
                })
                .collect();
            for (key, row) in taken {
                if let Some(displaced) = rows.insert(key, row) {
                    judged.retain(|k| *k != key);
                    dropped.push(displaced);
                }
            }
        }
        judged
    };
    let dead: Vec<IoRowKey> = judged.into_iter().filter(|key| !is_live(key.2)).collect();
    if dead.is_empty() {
        return dropped;
    }
    let mut rows = table.lock();
    dropped.extend(dead.into_iter().filter_map(|key| rows.remove(&key)));
    dropped
}

/// The teardown half: remove every row `vm` filed and hand them back, so the
/// caller releases what they own with no table lock held. Idempotent.
pub(crate) fn forget_vm_io_weak_rows<K: IoTableKey, R, S: std::hash::BuildHasher>(
    table: &Mutex<HashMap<K, R, S>>,
    vm: usize,
) -> Vec<R> {
    let mut rows = table.lock();
    let keys: Vec<K> = rows.keys().filter(|key| key.vm() == vm).copied().collect();
    keys.into_iter()
        .filter_map(|key| rows.remove(&key))
        .collect()
}

pub(crate) fn register_buffered_stream_natives(registry: &mut NativeMethodRegistry) {
    // The synthetic 4-field overrides (in/buf/pos/count) collide with the
    // real JDK 25 BIS field layout (initialSize/buf/count/pos/markpos/
    // marklimit on top of `in` inherited from FilterInputStream). Storing
    // into wrong slots leaves `buf` null and `markpos` 0, so `BIS.read()`
    // returns -1 immediately and `DataInputStream(BIS(FIS(tzdb.dat)))
    // .readByte()` reports EOF, which throws StreamCorruptedException
    // out of `ZoneInfoFile.load`. Letting the real bytecode run uses
    // `Unsafe.compareAndSetReference` (already implemented) to lazily
    // allocate `buf`, and the FIS read-bytes native already works.
    //
    // We deliberately leave BOS/PIS/POS untouched — those are still served
    // by their existing synthetic natives because they don't sit in the
    // JDK boot path. If a future regression appears for those streams we
    // should drop them too rather than adding more layout-coupled hacks.
    // BIS naming is preserved here for grep-discoverability of the fix.
    let _bis_dropped_overrides = "java/io/BufferedInputStream";

    // BufferedOutputStream
    //
    // The two constructors are `SyntheticStub`, stated; the read/write/flush
    // natives below stay on the ambient category. The note above about BIS
    // ends "if a future regression appears for those streams we should drop
    // them too rather than adding more layout-coupled hacks" — this is that
    // regression, and this is that drop, scoped to the constructors.
    //
    // `java.lang.ProcessImpl` builds the child's stdin as
    // `new ProcessPipeOutputStream(fd)` -> `super(new FileOutputStream(...))`
    // -> `BufferedOutputStream(OutputStream)`. These shims set `out` and stop;
    // the real constructor also runs `super(out)`, and `FilterOutputStream`'s
    // constructor is where `private final Object closeLock = new Object()`
    // lives. Skipping it leaves `closeLock` null, and `FilterOutputStream
    // .close()` opens with `synchronized (closeLock)` — so the FIRST
    // `Process.destroy()` in `--real-jdk` died with
    //
    //   NullPointerException: Cannot enter synchronized block because
    //                         "this.closeLock" is null
    //
    // out of `ProcessImpl.destroy`, whose own `try { stdin.close(); } catch
    // (IOException ignored)` cannot catch an NPE. Measured on the first build
    // that let the real `ProcessImpl` run.
    //
    // Restated, strict mode drops both and the real constructor chain runs:
    // `out`, `buf`, `maxBufSize`, `closed` and `closeLock` all get their real
    // values, and the surviving write/flush natives resolve `out`/`buf`/`count`
    // by NAME (see `bos_slots`), so they read the real layout unchanged.
    // Compatible mode keeps the shims and is untouched.
    let bos = "java/io/BufferedOutputStream";
    registry.register(
        bos,
        "<init>",
        "(Ljava/io/OutputStream;)V",
        native_bos_init,
        cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        bos,
        "<init>",
        "(Ljava/io/OutputStream;I)V",
        native_bos_init_size,
        cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(bos, "write", "(I)V", native_bos_write, cratonvm_native_api::NativeKind::Bridge);
    registry.register(bos, "write", "([BII)V", native_bos_write_bulk, cratonvm_native_api::NativeKind::Bridge);
    registry.register(bos, "flush", "()V", native_bos_flush, cratonvm_native_api::NativeKind::Bridge);
    registry.register(bos, "close", "()V", native_bos_close, cratonvm_native_api::NativeKind::Bridge);

    // PipedInputStream/PipedOutputStream — simplified as ByteArrayI/O pair
    let pis = "java/io/PipedInputStream";
    registry.register(pis, "<init>", "()V", native_pis_init, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        pis,
        "<init>",
        "(Ljava/io/PipedOutputStream;)V",
        native_pis_init_connected, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(pis, "read", "()I", native_bis_read, cratonvm_native_api::NativeKind::Bridge); // same BAIS-like read
    registry.register(pis, "available", "()I", native_bis_available, cratonvm_native_api::NativeKind::Bridge);
    registry.register(pis, "close", "()V", native_bis_noop, cratonvm_native_api::NativeKind::Bridge);

    let pos = "java/io/PipedOutputStream";
    registry.register(pos, "<init>", "()V", native_pos_init, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        pos,
        "<init>",
        "(Ljava/io/PipedInputStream;)V",
        native_pos_init_connected, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(pos, "write", "(I)V", native_bos_write, cratonvm_native_api::NativeKind::Bridge); // same buffered write
}

pub(crate) fn native_bis_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let this_pin = ctx.pin_native_root(this);
    let inner = args.get(1).cloned().unwrap_or(Value::Object(None));
    let buf = ctx.new_array(ArrayElementType::Byte, 8192);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, BIS_FIELD_IN, inner);
    ctx.set_field(this, BIS_FIELD_BUF, Value::Object(Some(buf)));
    ctx.set_field(this, BIS_FIELD_POS, Value::Int(0));
    ctx.set_field(this, BIS_FIELD_COUNT, Value::Int(0));
    Ok(None)
}

#[allow(dead_code)]
pub(crate) fn native_bis_init_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let inner = args.get(1).cloned().unwrap_or(Value::Object(None));
    let size = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 8192,
    };
    // Unregistered (`#[allow(dead_code)]`: `register_buffered_stream_natives`
    // lets the real `BufferedInputStream` bytecode run), kept in step with its
    // registered twin `native_bos_init_size` by gc-common w13-e so a future
    // registration inherits neither defect: the JDK refuses `size <= 0`
    // (`IllegalArgumentException("Buffer size <= 0")`, where this clamped to
    // 1), and the caller-sized buffer reclaims before its OOME.
    if size <= 0 {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "Buffer size <= 0".into(),
            }
            .into(),
        );
    }
    // GC: `this` and the wrapped stream are stored after the allocation, so
    // both are pinned across it and re-read; released on both exits.
    let this_pin = ctx.pin_native_root(this);
    let inner_pin = match inner {
        Value::Object(Some(o)) => Some(ctx.pin_native_root(o)),
        _ => None,
    };
    let buf = match new_byte_array_reclaiming(
        ctx,
        size as usize,
        "byte[] (BufferedInputStream(in, size))",
    ) {
        Ok(buf) => buf,
        Err(oome) => {
            ctx.unpin_native_roots(this_pin);
            return Err(oome);
        }
    };
    let inner = match (inner, inner_pin) {
        (Value::Object(Some(o)), Some(pin)) => Value::Object(Some(ctx.read_native_pin(pin, o))),
        (other, _) => other,
    };
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, BIS_FIELD_IN, inner);
    ctx.set_field(this, BIS_FIELD_BUF, Value::Object(Some(buf)));
    ctx.set_field(this, BIS_FIELD_POS, Value::Int(0));
    ctx.set_field(this, BIS_FIELD_COUNT, Value::Int(0));
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

/// Fill the BIS buffer from the inner stream. Returns the new count (0 means EOF).
pub(crate) fn bis_fill(ctx: &mut dyn NativeContext, this: ObjectRef) -> Result<i32, MethodCallFailed> {
    let buf = match ctx.get_field(this, BIS_FIELD_BUF) {
        Value::Object(Some(b)) => b,
        _ => return Ok(0),
    };
    let inner = match ctx.get_field(this, BIS_FIELD_IN) {
        Value::Object(Some(o)) => o,
        _ => return Ok(0),
    };
    let buf_len = ctx.array_length(buf) as i32;
    // GC: every `invoke_virtual` below runs the wrapped stream's Java, which
    // allocates and can collect — and the single-byte fallback does it once
    // per byte. `this`, the buffer being filled and the inner stream are all
    // held across those calls, so all three are pinned and read back.
    let base = ctx.pin_native_root(this);
    let buf_pin = ctx.pin_native_root(buf);
    let inner_pin = ctx.pin_native_root(inner);
    let mut buf = buf;
    let mut inner = inner;
    // Try bulk read into our buffer
    let n = ctx.invoke_virtual(
        inner,
        "read",
        "([BII)I",
        &[Value::Object(Some(buf)), Value::Int(0), Value::Int(buf_len)],
    )?;
    let bytes_read = match n {
        Some(Value::Int(v)) if v > 0 => v,
        _ => {
            // Bulk read not available or returned EOF/0 — fall back to single-byte reads
            // This handles streams that only implement read()I
            let mut filled = 0i32;
            while filled < buf_len {
                inner = ctx.read_native_pin(inner_pin, inner);
                let r = ctx.invoke_virtual(inner, "read", "()I", &[])?;
                match r {
                    Some(Value::Int(b)) if b >= 0 => {
                        buf = ctx.read_native_pin(buf_pin, buf);
                        ctx.set_array_element(buf, filled as usize, Value::Int(b));
                        filled += 1;
                        // After first byte, only continue if more data available
                        if filled == 1 {
                            continue; // always read at least 1 byte
                        }
                        // Check if inner stream has more data available
                        inner = ctx.read_native_pin(inner_pin, inner);
                        let avail = ctx.invoke_virtual(inner, "available", "()I", &[])?;
                        match avail {
                            Some(Value::Int(a)) if a > 0 => continue,
                            _ => break,
                        }
                    }
                    _ => break,
                }
            }
            if filled == 0 {
                let this = ctx.read_native_pin(base, this);
                ctx.set_field(this, BIS_FIELD_POS, Value::Int(0));
                ctx.set_field(this, BIS_FIELD_COUNT, Value::Int(0));
                ctx.unpin_native_roots(base);
                return Ok(0);
            }
            filled
        }
    };
    let this = ctx.read_native_pin(base, this);
    ctx.set_field(this, BIS_FIELD_POS, Value::Int(0));
    ctx.set_field(this, BIS_FIELD_COUNT, Value::Int(bytes_read));
    ctx.unpin_native_roots(base);
    Ok(bytes_read)
}

pub(crate) fn native_bis_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let this_pin = ctx.pin_native_root(this);
    let pos = match ctx.get_field(this, BIS_FIELD_POS) {
        Value::Int(v) => v,
        _ => 0,
    };
    let count = match ctx.get_field(this, BIS_FIELD_COUNT) {
        Value::Int(v) => v,
        _ => 0,
    };
    if pos < count {
        let buf = match ctx.get_field(this, BIS_FIELD_BUF) {
            Value::Object(Some(b)) => b,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let byte_val = match ctx.get_array_element(buf, pos as usize) {
            Value::Int(b) => b & 0xFF,
            _ => 0,
        };
        ctx.set_field(this, BIS_FIELD_POS, Value::Int(pos + 1));
        return Ok(Some(Value::Int(byte_val)));
    }
    // Buffer empty — refill from inner stream
    let new_count = bis_fill(ctx, this)?;
    if new_count == 0 {
        return Ok(Some(Value::Int(-1))); // EOF
    }
    let this = ctx.read_native_pin(this_pin, this);
    // Read first byte from freshly filled buffer
    let buf = match ctx.get_field(this, BIS_FIELD_BUF) {
        Value::Object(Some(b)) => b,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let byte_val = match ctx.get_array_element(buf, 0) {
        Value::Int(b) => b & 0xFF,
        _ => 0,
    };
    ctx.set_field(this, BIS_FIELD_POS, Value::Int(1));
    Ok(Some(Value::Int(byte_val)))
}

#[allow(dead_code)]
pub(crate) fn native_bis_read_bulk(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let this_pin = ctx.pin_native_root(this);
    let dest = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let dest_pin = ctx.pin_native_root(dest);
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
    let mut total = 0usize;
    while total < len {
        let pos = match ctx.get_field(this, BIS_FIELD_POS) {
            Value::Int(v) => v,
            _ => 0,
        };
        let count = match ctx.get_field(this, BIS_FIELD_COUNT) {
            Value::Int(v) => v,
            _ => 0,
        };
        if pos >= count {
            // Buffer empty — refill
            let new_count = bis_fill(ctx, this)?;
            if new_count == 0 {
                break; // EOF
            }
            continue; // re-check pos/count after fill
        }
        let this = ctx.read_native_pin(this_pin, this);
        // Copy from buffer to dest
        let buf = match ctx.get_field(this, BIS_FIELD_BUF) {
            Value::Object(Some(b)) => b,
            _ => break,
        };
        let avail = (count - pos) as usize;
        let to_copy = avail.min(len - total);
        for i in 0..to_copy {
            let b = ctx.get_array_element(buf, (pos as usize) + i);
            let dest = ctx.read_native_pin(dest_pin, dest);
            ctx.set_array_element(dest, off + total + i, b);
        }
        ctx.set_field(this, BIS_FIELD_POS, Value::Int(pos + to_copy as i32));
        total += to_copy;
    }
    if total == 0 {
        Ok(Some(Value::Int(-1)))
    } else {
        Ok(Some(Value::Int(total as i32)))
    }
}

pub(crate) fn native_bis_available(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let pos = match ctx.get_field(this, BIS_FIELD_POS) {
        Value::Int(v) => v,
        _ => 0,
    };
    let count = match ctx.get_field(this, BIS_FIELD_COUNT) {
        Value::Int(v) => v,
        _ => 0,
    };
    let buffered = (count - pos).max(0);
    // Also query inner stream
    let inner_avail = match ctx.get_field(this, BIS_FIELD_IN) {
        Value::Object(Some(o)) => match ctx.invoke_virtual(o, "available", "()I", &[]) {
            Ok(Some(Value::Int(a))) => a,
            _ => 0,
        },
        _ => 0,
    };
    Ok(Some(Value::Int(buffered + inner_avail)))
}

#[allow(dead_code)]
pub(crate) fn native_bis_skip(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let n = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let pos = match ctx.get_field(this, BIS_FIELD_POS) {
        Value::Int(v) => v,
        _ => 0,
    };
    let count = match ctx.get_field(this, BIS_FIELD_COUNT) {
        Value::Int(v) => v,
        _ => 0,
    };
    let avail = (count - pos) as i64;
    let skipped = n.min(avail);
    ctx.set_field(this, BIS_FIELD_POS, Value::Int(pos + skipped as i32));
    Ok(Some(Value::Long(skipped)))
}

pub(crate) fn native_bis_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

#[allow(dead_code)]
pub(crate) fn native_bis_mark_supported(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(1)))
}

// BufferedOutputStream
//
// All natives use `bos_slots()` to resolve `buf` and `count` at runtime
// rather than the legacy hardcoded slots. Real-JDK BufferedOutputStream
// extends FilterOutputStream which puts `closed:boolean` at slot 1,
// shifting `buf` to slot 2 and `count` to slot 3 — using BOS_FIELD_BUF=1
// and BOS_FIELD_COUNT=2 unconditionally wrote `buf` into `closed` and
// stored the would-be count in `buf`, so the `buf` field stayed null and
// every flush iterated 0 elements. DaCapo's `Benchmark.extractFileResource`
// (which uses BufferedOutputStream around FileOutputStream to copy
// embedded jars from dacapo.jar to scratch/jar/) wrote 0 bytes for every
// benchmark, leaving DacapoClassLoader unable to find the benchmark class
// and surfacing as a cryptic `InvocationTargetException` in
// `TestHarness.runBenchmark`. Resolving the slots by name fixes the write.
pub(crate) fn native_bos_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let this_pin = ctx.pin_native_root(this);
    let inner = args.get(1).cloned().unwrap_or(Value::Object(None));
    let buf = ctx.new_array(ArrayElementType::Byte, 8192);
    let (out_slot, buf_slot, count_slot) = bos_slots(ctx);
    let this = ctx.read_native_pin(this_pin, this);
    if out_slot < ctx.object_num_fields(this) {
        ctx.set_field(this, out_slot, inner);
    }
    if bos_has_buffer_slots(ctx, this, buf_slot, count_slot) {
        ctx.set_field(this, buf_slot, Value::Object(Some(buf)));
        ctx.set_field(this, count_slot, Value::Int(0));
    } else {
        bos_side_init(ctx, this, 8192);
    }
    // Wave 80 (lane L6): `FilterOutputStream.closeLock`, see
    // [`backfill_close_lock`].
    backfill_close_lock(ctx, this);
    Ok(None)
}

pub(crate) fn native_bos_init_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let inner = args.get(1).cloned().unwrap_or(Value::Object(None));
    let size = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 8192,
    };
    // `if (size <= 0) throw new IllegalArgumentException("Buffer size <= 0");`
    // is the constructor's first line. MEASURED: `--real-jdk` already refused
    // (it drops this SyntheticStub and runs the real bytecode) while the
    // DEFAULT mode accepted 0 and -1 — the sixth place in this campaign where
    // strict mode is right and the default is not. A zero-size buffer here was
    // silently rounded up to one byte, so the caller got a "buffered" stream
    // that flushes on every single byte.
    if size <= 0 {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "Buffer size <= 0".into(),
            }
            .into(),
        );
    }
    let (out_slot, buf_slot, count_slot) = bos_slots(ctx);
    if !bos_has_buffer_slots(ctx, this, buf_slot, count_slot) {
        // No `buf`/`count` slots: the bytes live in the side buffer, so the
        // `byte[size]` this used to allocate first was dropped unused -- a
        // caller-sized allocation that could only fail (gc-common w13-e).
        // Nothing below allocates.
        if out_slot < ctx.object_num_fields(this) {
            ctx.set_field(this, out_slot, inner);
        }
        bos_side_init(ctx, this, size as usize);
        // Wave 80 (lane L6): see [`backfill_close_lock`].
        backfill_close_lock(ctx, this);
        return Ok(None);
    }
    // GC: `this` and the wrapped stream `out` are stored after the allocation,
    // so both are pinned across it and re-read; released on both exits. `out`
    // used to be held as a bare `Value`, which was safe only while the
    // allocation could not collect.
    let this_pin = ctx.pin_native_root(this);
    let inner_pin = match inner {
        Value::Object(Some(o)) => Some(ctx.pin_native_root(o)),
        _ => None,
    };
    // gc-common w13-e (`common-w12e-native-io-write-side-growth-single-attempt`):
    // caller-sized, so a refusal reclaims and retries before the
    // `OutOfMemoryError`, as HotSpot's `new byte[size]` does. `live` audit:
    // `this` and `out` are pinned above and re-read below; nothing else is
    // held. Registered-only (`BufferedOutputStream(OutputStream, int)`,
    // `SyntheticStub`, compatible mode).
    let buf = match new_byte_array_reclaiming(
        ctx,
        size as usize,
        "byte[] (BufferedOutputStream(out, size))",
    ) {
        Ok(buf) => buf,
        Err(oome) => {
            ctx.unpin_native_roots(this_pin);
            return Err(oome);
        }
    };
    let inner = match (inner, inner_pin) {
        (Value::Object(Some(o)), Some(pin)) => Value::Object(Some(ctx.read_native_pin(pin, o))),
        (other, _) => other,
    };
    let this = ctx.read_native_pin(this_pin, this);
    if out_slot < ctx.object_num_fields(this) {
        ctx.set_field(this, out_slot, inner);
    }
    ctx.set_field(this, buf_slot, Value::Object(Some(buf)));
    ctx.set_field(this, count_slot, Value::Int(0));
    ctx.unpin_native_roots(this_pin);
    // Wave 80 (lane L6): see [`backfill_close_lock`].
    backfill_close_lock(ctx, this);
    Ok(None)
}

pub(crate) fn bos_write_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

thread_local! {
    pub(crate) static BOS_WRITE_LOCK_HELD: std::cell::Cell<bool> = std::cell::Cell::new(false);
}

pub(crate) fn with_bos_write_lock<R>(f: impl FnOnce() -> R) -> R {
    if BOS_WRITE_LOCK_HELD.with(|held| held.get()) {
        return f();
    }
    let _guard = bos_write_lock().lock();
    BOS_WRITE_LOCK_HELD.with(|held| held.set(true));
    let result = f();
    BOS_WRITE_LOCK_HELD.with(|held| held.set(false));
    result
}

pub(crate) fn native_bos_write(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    with_bos_write_lock(|| native_bos_write_locked(ctx, args))
}

pub(crate) fn native_bos_write_locked(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let byte_val = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let (_out_slot, buf_slot, count_slot) = bos_slots(ctx);
    if !bos_has_buffer_slots(ctx, this, buf_slot, count_slot) {
        let (out_slot, _, _) = bos_slots(ctx);
        return bos_side_write_byte(ctx, this, out_slot, byte_val);
    }
    let count = match ctx.get_field(this, count_slot) {
        Value::Int(v) => v,
        _ => 0,
    };
    let buf = match ctx.get_field(this, buf_slot) {
        Value::Object(Some(b)) => b,
        _ => return Ok(None),
    };
    let buf_len = ctx.array_length(buf) as i32;
    if count >= buf_len {
        // Real BufferedOutputStream.implWrite(int): flush the full buffer,
        // then store the byte INTO the now-empty buffer. The byte must NOT
        // be passed straight through to the inner stream as a 1-byte
        // `write(int)` — that changes the chunk boundaries the inner stream
        // observes. Seen as OutputStreamPublisherTests.chunkSize() (buffer
        // size 3) receiving chunks "foo","b","arb","a","z" instead of
        // "foo","bar","baz": the byte that triggered the flush skipped the
        // buffer entirely and arrived as its own 1-byte write.
        //
        // GC SAFETY: `this` must be pinned across `native_bos_flush`, which
        // invokes arbitrary overridable `OutputStream.write()` bytecode that
        // can trigger a GC and move `this` (see native_bos_flush_locked for
        // the full writeup of this hazard class).
        let this_pin = ctx.pin_native_root(this);
        let flush_result = native_bos_flush(ctx, args);
        let this = ctx.read_native_pin(this_pin, this);
        ctx.unpin_native_roots(this_pin);
        flush_result?;
        // Re-read `buf` after the flush's invoke_virtual rather than holding
        // the array oop across it (same stale-native-local discipline as
        // native_bos_flush_locked); flush reset `count` to 0.
        let buf = match ctx.get_field(this, buf_slot) {
            Value::Object(Some(b)) => b,
            _ => return Ok(None),
        };
        ctx.set_array_element(buf, 0, Value::Int(byte_val & 0xFF));
        ctx.set_field(this, count_slot, Value::Int(1));
    } else {
        ctx.set_array_element(buf, count as usize, Value::Int(byte_val & 0xFF));
        ctx.set_field(this, count_slot, Value::Int(count + 1));
    }
    Ok(None)
}

pub(crate) fn native_bos_write_bulk(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    with_bos_write_lock(|| native_bos_write_bulk_locked(ctx, args))
}

pub(crate) fn native_bos_write_bulk_locked(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // MEASURED: `write(null, 0, 1)` was a silent no-op and `write(b, -1, 1)`
    // wrote nothing and returned normally, where HotSpot raises NPE and
    // ArrayIndexOutOfBoundsException. The buffered case reaches
    // `System.arraycopy`, which is where both come from — and which is why the
    // type here is the ARRAY subclass while `FileOutputStream`'s is the plain
    // superclass. The two really do differ; they were both wrong here in
    // different directions.
    let src = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
            )
        }
    };
    let off_i = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let len_i = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let src_len = ctx.array_length(src) as i32;
    if off_i < 0 || len_i < 0 || off_i.checked_add(len_i).map_or(true, |e| e > src_len) {
        // The TYPE depends on which branch of `implWrite` the length picks,
        // and HotSpot really does answer both:
        //
        //   new BufferedOutputStream(sink, 4).write(b /*len 2*/, -1, 1)
        //       len < buf.length  -> System.arraycopy -> ArrayIndexOutOfBounds
        //   ... .write(b, 0, 9)   len >= buf.length -> out.write(b, 0, 9)
        //                          -> Objects.checkFromIndexSize -> IndexOutOfBounds
        //
        // MEASURED, all three rows. A single type here is wrong for one of
        // them whichever one is chosen, so the branch is reproduced rather
        // than picked.
        let (out_slot, buf_slot, count_slot) = bos_slots(ctx);
        let _ = (out_slot, count_slot);
        let buf_len = match ctx.get_field(this, buf_slot) {
            Value::Object(Some(b)) => ctx.array_length(b) as i32,
            _ => i32::MAX,
        };
        return Err(if len_i >= buf_len {
            MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::ioobe(
                cratonvm_types::error::out_of_bounds_message::check_from_index_size(
                    i64::from(off_i),
                    i64::from(len_i),
                    i64::from(src_len),
                ),
            )))
        } else {
            MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::aioobe_index_only(
                if off_i < 0 {
                    off_i
                } else {
                    off_i.saturating_add(len_i)
                },
            )))
        });
    }
    let off = off_i as usize;
    let len = len_i as usize;
    // Mirror the real BufferedOutputStream.implWrite(byte[], off, len)
    // (JDK: `if (len >= maxBufSize) { flushBuffer(); out.write(b, off, len); }`)
    // instead of looping per byte through write(int). The per-byte loop broke
    // the chunk boundaries the inner stream observes: with buffer size N, a
    // bulk write of exactly N bytes should arrive at the inner stream as ONE
    // N-byte write (after flushing any pending bytes), but the loop delivered
    // the flush-triggering byte as its own 1-byte `write(int)` (see
    // native_bos_write_locked). OutputStreamPublisherTests.chunkSize() (chunk
    // size 3, writes of "foo"/"bar"/"baz") got "foo","b","arb","a","z".
    let (out_slot, buf_slot, count_slot) = bos_slots(ctx);
    if !bos_has_buffer_slots(ctx, this, buf_slot, count_slot) {
        return bos_side_write_bulk(ctx, this, out_slot, src, off, len);
    }
    let buf_len = match ctx.get_field(this, buf_slot) {
        Value::Object(Some(b)) => ctx.array_length(b),
        _ => return Ok(None),
    };
    if len >= buf_len {
        // At least as large as the buffer: flush pending bytes, then hand the
        // caller's array to the inner stream as a single bulk write.
        //
        // GC SAFETY: `this` must be pinned across `native_bos_flush`, which
        // invokes arbitrary overridable `OutputStream.write()` bytecode that
        // can trigger a GC and move `this` (see native_bos_flush_locked for
        // the full writeup of this hazard class).
        // `src` needs the same treatment as `this`: it is the byte array the
        // delegated `write` receives, and it crosses the same flush.
        let this_pin = ctx.pin_native_root(this);
        let src_pin = ctx.pin_native_root(src);
        let flush_result = native_bos_flush(ctx, args);
        let this = ctx.read_native_pin(this_pin, this);
        let src = ctx.read_native_pin(src_pin, src);
        ctx.unpin_native_roots(this_pin);
        flush_result?;
        let inner = match ctx.get_field(this, out_slot) {
            Value::Object(Some(o)) => o,
            _ => return Ok(None),
        };
        ctx.invoke_virtual(
            inner,
            "write",
            "([BII)V",
            &[
                Value::Object(Some(src)),
                Value::Int(off as i32),
                Value::Int(len as i32),
            ],
        )?;
        return Ok(None);
    }
    let mut count = match ctx.get_field(this, count_slot) {
        Value::Int(v) => v.max(0) as usize,
        _ => 0,
    };
    let mut src = src;
    if len > buf_len.saturating_sub(count) {
        // Not enough room: flush first (resets count to 0), then buffer.
        //
        // GC SAFETY: pin `this` across `native_bos_flush` (see
        // native_bos_flush_locked for the full writeup of this hazard
        // class); the refreshed `this` must persist past this block since
        // `buf`/`count_slot` below are re-derived from it.
        //
        // gc-common w20-f: `src` too. The copy below read the caller's array
        // at its address from before the flush (arbitrary `write` bytecode),
        // which the large-write branch above already pinned for.
        let this_pin = ctx.pin_native_root(this);
        let src_pin = ctx.pin_native_root(src);
        let flush_result = native_bos_flush(ctx, args);
        this = ctx.read_native_pin(this_pin, this);
        src = ctx.read_native_pin(src_pin, src);
        ctx.unpin_native_roots(this_pin);
        flush_result?;
        count = 0;
    }
    // Re-read `buf` after any flush rather than holding the array oop across
    // its invoke_virtual (same stale-native-local discipline as
    // native_bos_flush_locked).
    let buf: ObjectRef = match ctx.get_field(this, buf_slot) {
        Value::Object(Some(b)) => b,
        _ => return Ok(None),
    };
    for i in 0..len {
        let b = ctx.get_array_element(src, off + i);
        ctx.set_array_element(buf, count + i, b);
    }
    ctx.set_field(this, count_slot, Value::Int((count + len) as i32));
    Ok(None)
}

pub(crate) fn native_bos_flush(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    with_bos_write_lock(|| native_bos_flush_locked(ctx, args))
}

pub(crate) fn native_bos_flush_locked(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let (out_slot, buf_slot, count_slot) = bos_slots(ctx);
    if !bos_has_buffer_slots(ctx, this, buf_slot, count_slot) {
        return bos_side_flush(ctx, this, out_slot, true);
    }
    let count = match ctx.get_field(this, count_slot) {
        Value::Int(v) => v,
        _ => 0,
    };
    if count > 0 {
        let buf = match ctx.get_field(this, buf_slot) {
            Value::Object(Some(b)) => b,
            _ => return Ok(None),
        };
        let inner = match ctx.get_field(this, out_slot) {
            Value::Object(Some(o)) => o,
            _ => return Ok(None),
        };
        // CORRECTNESS FIX (2026-07-11, WildFly WFLYHC0053 investigation): this
        // used to reset `count` to 0 BEFORE the `out.write(buf, 0, count)`
        // invoke below, reasoning that it avoided holding a native-local oop
        // across the call. That reordering is a real behavioral deviation
        // from the JDK (`BufferedOutputStream.flushBuffer()` resets
        // `count = 0` AFTER `out.write(...)` returns, never before) and
        // opens a correctness window: as soon as `count` reads back as 0,
        // this object's buffer is signaled "empty and available", so any
        // write(int)/write(byte[]) that reaches this same BufferedOutputStream
        // while the invoke below is still in flight would start overwriting
        // `buf[0..]` — the SAME array object still passed as a live argument
        // to the in-flight `write` call — before its bytes are consumed by
        // the write inside that call. `buf` and `inner` are passed AS ARGS
        // to the invoke (rooted for its duration regardless of when `count`
        // is reset), so moving the reset after the call does not reintroduce
        // the stale-native-local hazard the original comment was guarding
        // against for `buf`/`inner`. This is a real, independently-justified
        // fix (verified via 2 full WildFly domain-boot runs: no regression,
        // same subsequent behavior otherwise) — NOTE it was found while
        // investigating `wildfly-domain-heap-corrupt-value-timeout-RESOLVED.md`'s
        // WFLYHC0053 blocker, but is NOT that bug's root cause: the observed
        // byte value (152) that looked like corruption on first read is
        // actually the real WildFly wire protocol's own `CHUNK_START` marker
        // byte (`ConnectionImpl$MessageOutputStream`/`ConnectionImpl$2`'s
        // `lookupswitch` on 152/153), not a corrupted opcode — see that
        // doc's own write-up for the corrected mechanism and what's still
        // open.
        //
        // GC SAFETY (2026-07-20, DoHead sporadic transport-flake
        // investigation): the claim above that "nothing is read back from
        // `this`" after the invoke was wrong — `this` IS read back, right
        // below, via `set_field`. `this` is held as a raw, unpinned native
        // local across `inner.write()`, which is arbitrary overridable Java
        // bytecode (any `OutputStream` subclass, e.g. Tomcat's socket
        // stream) that can allocate and trigger a GC. A GC landing during
        // that call moves `this`; the stale address then makes the
        // `set_field` below silently corrupt whatever object now occupies
        // it (a guarded OOB drop — observed landing on a fresh zero-field
        // `java/lang/Object`) instead of resetting the real buffer's
        // `count`, permanently desyncing this stream's byte accounting.
        // Same class of bug as the `native_map_put`/`native_map_remove_pinned`
        // GC hazard: pin `this` across the call and re-derive it from the
        // pin afterward.
        let this_pin = ctx.pin_native_root(this);
        let write_result = ctx.invoke_virtual(
            inner,
            "write",
            "([BII)V",
            &[Value::Object(Some(buf)), Value::Int(0), Value::Int(count)],
        );
        this = ctx.read_native_pin(this_pin, this);
        ctx.unpin_native_roots(this_pin);
        write_result?;
        ctx.set_field(this, count_slot, Value::Int(0));
    }
    // `BufferedOutputStream.implFlush()` is `flushBuffer(); out.flush();` — the
    // inner flush is UNCONDITIONAL, and it was missing here. Emptying our own
    // buffer into `out` is not a flush of `out`: if the inner stream buffers
    // too, its bytes stayed where they were, and any failure it would have
    // raised was never raised at all.
    //
    // The tell was inside this same function: [`bos_side_flush`], the arm taken
    // when the receiver has no real buffer slots, has always done
    // `invoke_virtual(inner, "flush")` under its `flush_inner` flag. The two
    // halves of one flush disagreed, and the half with the shorter path was
    // the one that ran for a real `BufferedOutputStream`.
    //
    // MEASURED — `probes/CloseFlushSwallowProbe.java`'s
    // `filterOutFlushFailureWins`: a sink whose `flush()` throws must let that
    // failure out of `close()`. It answered `none`, because `flush()` was never
    // called on it. `W7-57` swept 51 delegated-failure sites and this one
    // survived the sweep, since the failure is not swallowed here — it is never
    // produced. Same pin discipline as the write above: `flush()` is arbitrary
    // overridable bytecode and can move `this`.
    if let Some(inner) = bos_inner(ctx, this, out_slot) {
        let this_pin = ctx.pin_native_root(this);
        let flush_result = ctx.invoke_virtual(inner, "flush", "()V", &[]);
        this = ctx.read_native_pin(this_pin, this);
        ctx.unpin_native_roots(this_pin);
        let _ = this;
        flush_result?;
    }
    Ok(None)
}

/// Real `BufferedOutputStream.close`: flush buffered bytes to the inner
/// stream, then close the inner stream so its underlying writer (BufWriter
/// around File in fd_table) actually pushes to disk.
///
/// Previously `close` was registered as `native_bos_flush`, which left the
/// inner FileOutputStream (and its `fd_table` BufWriter) open. For small
/// writes (< BufWriter capacity, default 8 KB) the bytes never reached disk
/// — observed as every BufferedOutputStream-wrapped writer producing a
/// 0-byte file. DaCapo's `Benchmark.extractFileResource` uses exactly this
/// pattern to copy embedded jars from dacapo.jar to scratch/jar/, so every
/// benchmark jar ended up 0 bytes and `DacapoClassLoader` couldn't find the
/// benchmark class — `TestHarness.runBenchmark` then surfaced as a cryptic
/// `InvocationTargetException` whose root cause was
/// `ClassNotFoundException: org.dacapo.<bench>.<Main>`.
pub(crate) fn native_bos_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // 1) Flush the buffered bytes. The flush invokes arbitrary wrapped
    // OutputStream bytecode; preserve the receiver across it and across the
    // subsequent close before touching its side-table identity.
    let this_pin = ctx.pin_native_root(this);
    let flush_result = native_bos_flush(ctx, args);
    let mut this = ctx.read_native_pin(this_pin, this);
    // 2) Close the inner stream — from the equivalent of the JDK's `finally`,
    //    so it happens EVEN WHEN THE FLUSH FAILED.
    //
    //    `FilterOutputStream.close()` is
    //
    //        try { flush(); } catch (Throwable e) { flushException = e; throw e; }
    //        finally {
    //            if (flushException == null) { out.close(); }
    //            else { try { out.close(); } catch (Throwable ce) { … throw ce; } }
    //        }
    //
    //    so a failing flush does not cost the caller its file descriptor. This
    //    code used to `return Err(error)` on the flush failure and never reach
    //    the close at all, which leaks the inner stream on exactly the path
    //    where the caller most needs it released. It was invisible while
    //    `native_bos_flush` could not produce a failure in the first place;
    //    fixing that flush is what made this reachable.
    //    MEASURED — `CloseFlushSwallowProbe`'s
    //    `filterOutCloseAttemptedAfterFailedFlush`.
    let (out_slot, _, _) = bos_slots(ctx);
    let close_result = if let Some(inner) = bos_inner(ctx, this, out_slot) {
        let result =
            ctx.invoke_virtual_declared("java/io/OutputStream", inner, "close", "()V", &[]);
        this = ctx.read_native_pin(this_pin, this);
        result
    } else {
        Ok(None)
    };
    // The key before the lock: it used to be computed inside the lock
    // expression, so `identity_hash_code` ran under the table lock. `this`
    // was re-read after the last Java call, so its address is current.
    let side_key = io_side_row_key(ctx, this);
    bos_side_buffers().lock().remove(&side_key);
    ctx.unpin_native_roots(this_pin);
    // Precedence is the JDK's: a close failure wins over a flush failure
    // (the JDK suppresses the flush one INTO it), and a flush failure wins
    // when the close succeeded.
    //
    // NOT REPRODUCED: `closeException.addSuppressed(flushException)` when both
    // fail. The caller sees the close failure with the right identity and
    // without the suppressed flush one attached.
    match (flush_result, close_result) {
        (_, Err(close_error)) => Err(close_error),
        (Err(flush_error), Ok(_)) => Err(flush_error),
        (Ok(_), Ok(_)) => Ok(None),
    }
}

// PipedInputStream/OutputStream simplified as BAIS/BAOS
pub(crate) fn native_pis_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_bis_init(ctx, args)
}
fn native_pis_init_connected(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_bis_init(ctx, args) // connection is simulated
}
fn native_pos_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_bos_init(ctx, args)
}
pub(crate) fn native_pos_init_connected(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_bos_init(ctx, args)
}

/// `PipedOutputStream.close()`: the flush it has always been, then the side
/// buffer's row goes (gc-common w14-e). A real `PipedOutputStream` declares
/// only `sink`, so every pipe takes the side-buffer path, and `close()` was
/// registered as the bare flush: no pipe's row was ever removed. A write after
/// `close()` finds no row and starts one exactly like the one removed here
/// (empty, [`BOS_SIDE_DEFAULT_CAPACITY`], which is what `native_pos_init`
/// installs), so nothing observable changes but the growth.
pub(crate) fn native_pos_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // The flush runs Java and may move the stream: its row is found by the
    // address it has AFTER the flush (gc-common w29-c; the row key names the
    // owner's current address, and the epilogue re-files a moved row).
    let side_key = bos_side_key(ctx, this);
    let this_pin = ctx.pin_native_root(this);
    let flushed = native_bos_flush(ctx, args);
    let owner = bos_side_owner(ctx.read_native_pin(this_pin, this));
    ctx.unpin_native_roots(this_pin);
    bos_side_buffers()
        .lock()
        .remove(&io_row_key(side_key, owner));
    flushed
}

// ===========================================================================
// Phase 42: CharBuffer + Typed NIO Buffers
// ===========================================================================
// All typed buffers share the BB_FIELD_* layout (array=0, pos=1, limit=2, capacity=3, mark=4).
// Position/limit/capacity/mark/flip/clear/rewind/hasRemaining/remaining all reuse
// the ByteBuffer implementations since they only touch fields 1-4.

/// Which storage encoding a typed-buffer receiver uses — the per-receiver
/// question F26-1 §9.1 said had to be asked before any of this family could
/// alias, and the reason "land one and call the family fixed" is the failure
/// mode here.
///
/// **Three encodings live behind one `java/nio/IntBuffer` stamp**, and only
/// two of them can be aliased by this crate:
///
/// | receiver | storage | this crate |
/// |---|---|---|
/// | `alloc_typed_buffer`-minted (`IntBuffer.allocate`/`wrap`) | `hb` + `offset` in ELEMENTS | [`Self::Heap`] |
/// | a DIRECT view (`allocateDirect(n).asIntBuffer()`) | `address`, bytes | [`Self::Direct`] |
/// | an s2 heap view (`allocate(n).asIntBuffer()`) | `native-builtins`' `BB_SEGMENT_SLOT` array + a byte start encoded as `-(bs+1)` in the MARK slot | [`Self::Refuse`] |
///
/// The third is the one that must refuse. Its array is the source
/// `ByteBuffer`'s `byte[]`, so `bb_state` reports `elem == Byte` for what is
/// stamped an `IntBuffer`, and its window start is not in `offset` at all —
/// aliasing it through `hb` + `offset` would produce a view reading from
/// element 0 of a byte array with int-shaped accessors. Refusing keeps the
/// existing COPY, which is what that receiver got before this change and is
/// data-correct for it.
///
/// **Refusing rather than guessing is the whole design.** F26-1 §9.1 asked for
/// "a `tb_derive_view` that refuses (falls back to the copy) rather than
/// guessing when the receiver's encoding is not one it knows", and the reason
/// is that a wrong guess here reads the WRONG BYTES silently, where a copy
/// merely loses aliasing — which is the defect that already existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TbAlias {
    /// `hb` resolves BY NAME to an array of the family's own element type, so
    /// `offset` is an element index into it and means what this crate thinks.
    Heap { arr: ObjectRef },
    /// No array; a plausible native block whose element width the receiver's
    /// class name agrees with.
    Direct { addr: i64 },
    /// Some other encoding. Copy.
    Refuse,
}

/// [`TbAlias`] for `this`, given the view [`bb_state`] already resolved.
///
/// Three screens, each of which a wrong receiver fails:
///
/// 1. **`hb` BY NAME, not `bb_resolve_heap_array`.** That function's second arm
///    reads `BB_SEGMENT_SLOT`, which is exactly the s2-view encoding this must
///    refuse, and its third reads slot 0. Asking for the field by name is what
///    distinguishes "a real-layout typed buffer whose `offset` field is the
///    window" from "something else that also has an array somewhere".
/// 2. **The array's element type equals the family's.** A `java/nio/IntBuffer`
///    holding a `byte[]` is an s2 view that reached `hb` some other way; a
///    mismatch here is precisely the case where `offset`-as-elements is wrong.
///    This is also what makes `alloc_typed_buffer_over`'s later
///    `set_array_element` type-correct.
/// 3. **The resolved storage agrees.** If `bb_state` classified the receiver
///    DIRECT, the `hb` arm cannot apply and vice versa — the two must not be
///    read from different places, or the derived buffer's window would be
///    computed against one storage and installed over the other.
pub(crate) fn tb_alias_source(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    view: TbView,
    elem: ArrayElementType,
) -> TbAlias {
    match view.storage {
        BbStorage::Heap { arr: resolved, .. } => {
            let Value::Object(Some(named)) = ctx.get_field_by_name(this, "hb") else {
                return TbAlias::Refuse;
            };
            if named != resolved
                || ctx.heap_kind_of(named) != ObjectKind::Array
                || ctx.heap_element_type_of(named) != elem
            {
                return TbAlias::Refuse;
            }
            TbAlias::Heap { arr: named }
        }
        BbStorage::Direct { addr } => {
            if view.elem != elem {
                return TbAlias::Refuse;
            }
            TbAlias::Direct { addr }
        }
    }
}

/// Mint an ALIASING typed heap buffer over `arr` at `window` — real
/// `HeapIntBuffer`'s protected constructor, argument for argument.
///
/// `None` when the layout cannot carry `window.offset`
/// ([`buf_try_set_heap_offset`]'s round-trip probe: in synthetic-JDK mode
/// there is no `offset` field, the write is a no-op, and
/// [`bb_resolve_heap_offset`] would answer 0 for a view that needs a non-zero
/// base — reading the wrong ELEMENTS). Callers fall back to copying.
///
/// `arr` is PINNED across the allocation for the reason
/// [`alloc_byte_buffer_over`]'s doc gives at length: `alloc_typed_object`
/// allocates, a collection can RELOCATE the array the caller resolved before
/// the call, and storing the stale `ObjectRef` installs a dangling backing
/// array.
///
/// The write ORDER is [`alloc_byte_buffer_over`]'s and is load-bearing:
/// `address` goes AFTER `buf_write_metadata`, because [`buf_set_mark`] saves
/// and restores whatever `address` it finds and an earlier write would survive
/// only as the SAVED value.
pub(crate) fn alloc_typed_buffer_over(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    arr: ObjectRef,
    window: BufferWindow,
    elem_width: usize,
) -> Option<ObjectRef> {
    let arr_pin = ctx.pin_native_root(arr);
    let obj = alloc_typed_object(ctx, class_name);
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(arr_pin);
    if !buf_try_set_heap_offset(ctx, obj, window.offset) {
        // Nothing has been installed on `obj` yet, so the abandoned object is
        // inert and unreferenced. Same shape as `alloc_byte_buffer_over`.
        return None;
    }
    ctx.set_field(obj, BB_FIELD_ARRAY, Value::Object(Some(arr)));
    ctx.set_field_by_name(obj, "hb", Value::Object(Some(arr)));
    buf_write_metadata(ctx, obj, window.pos, window.lim, window.cap, window.mark);
    ctx.set_field_by_name(
        obj,
        "address",
        Value::Long(typed_buffer_address(window.offset, elem_width)),
    );
    Some(obj)
}

/// Mint an ALIASING typed DIRECT view over the native block at `addr`,
/// advanced to the window's element 0.
///
/// No array is installed, deliberately: with `hb` null, `BB_SEGMENT_SLOT`
/// null and slot 0 carrying `buf_write_metadata`'s `mark`,
/// [`bb_resolve_heap_array`] finds nothing and [`bb_resolve_direct_address`]
/// answers — so the derived buffer resolves DIRECT, which is what it is.
/// Infallible: a direct view's window lives entirely in its `address`, so
/// there is no layout question to fail.
pub(crate) fn alloc_typed_direct_view(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    addr: i64,
    rel_start: usize,
    window: BufferWindow,
    elem_width: usize,
) -> ObjectRef {
    let obj = alloc_typed_object(ctx, class_name);
    buf_write_metadata(ctx, obj, window.pos, window.lim, window.cap, window.mark);
    ctx.set_field_by_name(
        obj,
        "address",
        Value::Long(typed_direct_view_address(addr, rel_start, elem_width)),
    );
    obj
}

/// The typed families' [`bb_derive_view`]: mint the buffer a view-producing
/// native must return for `window` over `this`'s storage, ALIASING where the
/// encoding is one this crate can express and answering `None` where it is
/// not.
///
/// `window.offset` is in ELEMENTS here, not bytes — that is the whole
/// difference from `bb_derive_view`, and it is why [`slice_window`] /
/// [`slice_range_window`] / [`duplicate_window`] are reused UNCHANGED: their
/// arithmetic is `index + offset` in whatever unit the caller supplies, and
/// real `HeapIntBuffer.slice()` is `new HeapIntBuffer(hb, -1, 0, rem, rem,
/// pos + offset, segment)` — the same five arguments as `HeapByteBuffer`'s,
/// with `offset` counted in elements. Only [`typed_buffer_address`] needs the
/// width.
///
/// `rel_start` is the window's start in the SOURCE's own index space
/// (`pos` for a slice, `index` for `slice(int,int)`, `0` for a duplicate),
/// which is what the direct arm advances by and what the copy fallback indexes
/// with. Deriving it as `window.offset - src_offset` rather than passing it
/// separately keeps the two in step: getting it wrong double-counts the
/// source's own offset, which is the silent wrong-bytes read this whole family
/// is about.
pub(crate) fn tb_derive_view(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    view: TbView,
    window: BufferWindow,
    class_name: &str,
    elem: ArrayElementType,
) -> Option<ObjectRef> {
    let src_offset = match view.storage {
        BbStorage::Heap { offset, .. } => offset,
        BbStorage::Direct { .. } => 0,
    };
    let rel_start = window.offset.saturating_sub(src_offset);
    let width = tb_elem_width(elem);
    // The encoding question is asked BEFORE anything allocates, so `this` is
    // not dereferenced after a collection could have moved it.
    match tb_alias_source(ctx, this, view, elem) {
        TbAlias::Heap { arr } => alloc_typed_buffer_over(ctx, class_name, arr, window, width),
        TbAlias::Direct { addr } => Some(alloc_typed_direct_view(
            ctx, class_name, addr, rel_start, window, width,
        )),
        TbAlias::Refuse => None,
    }
}

/// Allocate the COPY a refused derivation falls back to, with the source's
/// backing array pinned across the allocation, and hand back a view rebuilt
/// from the post-GC reference.
///
/// **The pin is a fix, not a precaution.** `alloc_typed_buffer` allocates
/// TWICE (the object and its array) and the copy loop then reads through a
/// `TbView` captured *before* those allocations — so a collection landing in
/// between left `BbStorage::Heap { arr }` pointing at a relocated array and
/// every subsequent `tb_read_elem` read through a stale reference. The
/// `ByteBuffer` half of this family already pins here (`bb_derive_view`'s
/// fallback); the typed half never did.
pub(crate) fn tb_alloc_copy_target(
    ctx: &mut dyn NativeContext,
    src: TbView,
    class_name: &str,
    elem: ArrayElementType,
    capacity: usize,
) -> (ObjectRef, TbView) {
    match src.storage {
        BbStorage::Heap { arr, offset } => {
            let pin = ctx.pin_native_root(arr);
            let copy = alloc_typed_buffer(ctx, class_name, elem, capacity);
            let arr = ctx.read_native_pin(pin, arr);
            ctx.unpin_native_roots(pin);
            (
                copy,
                TbView {
                    storage: BbStorage::Heap { arr, offset },
                    ..src
                },
            )
        }
        // A native block is not moved by any collector, so the direct arm has
        // nothing to pin and its view survives the allocation unchanged.
        BbStorage::Direct { .. } => (alloc_typed_buffer(ctx, class_name, elem, capacity), src),
    }
}

/// Mint a bare typed-buffer instance with NO storage installed — the object
/// half of [`alloc_typed_buffer`].
///
/// Extracted so the ALIASING constructors ([`alloc_typed_buffer_over`],
/// [`alloc_typed_direct_view`]) cannot drift from the allocating one in the
/// width decision, which is the part that is easy to get wrong: "the larger of
/// `BB_NUM_FIELDS` and the real class's declared fields", so a real-JDK
/// `java.nio.IntBuffer` receiver has room for `hb`/`offset`/`isReadOnly` and a
/// synthetic one still has its 5 slots. `alloc_object` clamps UP anyway
/// (`vm/src/vm/vm_exec.rs`, "Layout-mismatch guard"); the `max` is what makes
/// the SYNTHETIC direction right.
///
/// Same relationship as [`alloc_bb_object`] to [`alloc_byte_buffer`].
pub(crate) fn alloc_typed_object(ctx: &mut dyn NativeContext, class_name: &str) -> ObjectRef {
    match ctx.ensure_class_initialized(class_name) {
        Ok(cid) => {
            let n = BB_NUM_FIELDS.max(ctx.class_num_total_fields(cid));
            ctx.alloc_object(cid, n)
        }
        Err(_) => ctx.alloc_object(cratonvm_types::ClassId::new(0), BB_NUM_FIELDS),
    }
}

pub(crate) fn alloc_typed_buffer(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    elem_type: ArrayElementType,
    capacity: usize,
) -> ObjectRef {
    // Pick the larger of BB_NUM_FIELDS and the real class's declared fields
    // so real-JDK-loaded types have room for their full layout.
    // (The width decision moved to `alloc_typed_object`, which the aliasing
    // constructors share; the dead `let _ = n;` that used to sit here went
    // with it.)
    let obj = alloc_typed_object(ctx, class_name);
    // GC: nothing in Java refers to this object yet, so this Rust local is
    // its ONLY reference — and the allocation below can collect. Under the
    // moving collector that leaves the local stale; under the Generational
    // collector's NON-MOVING young sweep the object is unreachable and gets
    // ZEROED in place. Pin across the allocation and read the live address
    // back (`unpinned-native-locals` family; `native-io` was outside the
    // 2026-08-25 audit's scope, which covered `native-builtins/src`).
    let __pin = ctx.pin_native_root(obj);
    let array = ctx.new_array(elem_type, capacity);
    let obj = ctx.read_native_pin(__pin, obj);
    // Synthetic slot
    ctx.set_field(obj, BB_FIELD_ARRAY, Value::Object(Some(array)));
    // Real JDK Heap*Buffer backing array is named `hb`
    ctx.set_field_by_name(obj, "hb", Value::Object(Some(array)));
    buf_write_metadata(ctx, obj, 0, capacity as i32, capacity as i32, -1);
    // A real `Heap*Buffer` sets `address = ARRAY_<T>_BASE_OFFSET + offset *
    // scale`, and every primitive array's base offset is 16 here (matching
    // `unsafe_array_read_bytes`'s `ABASE`). This buffer is freshly allocated
    // with `offset = 0`, so 16 is the whole of it. `alloc_byte_buffer` already
    // did this; the typed families (Short/Int/Long/Float/Double/Char) never
    // did, which left `address` reading as the mark (-1) and made every bulk
    // `put(<same-kind>Buffer)` throw AIOOBE out of `Unsafe.copyMemory`.
    let obj = ctx.read_native_pin(__pin, obj);
    ctx.set_field_by_name(obj, "address", Value::Long(16));
    ctx.unpin_native_roots(__pin);
    obj
}

/// Fetch a `public static final` singleton field (e.g. `ByteOrder.BIG_ENDIAN`)
/// off an already-initialized real-JDK class.
pub(crate) fn tb_static_object(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    field_name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let class_id = ctx.ensure_class_initialized(class_name)?;
    let field_idx = ctx
        .static_field_index_by_name(class_id, field_name)
        .ok_or_else(|| {
            MethodCallFailed::InternalError(VmError::Internal {
                message: format!("missing static field {class_name}.{field_name}"),
            })
        })?;
    match ctx.get_static_field(class_id, field_idx) {
        Value::Object(Some(obj)) => Ok(obj),
        _ => Err(MethodCallFailed::InternalError(VmError::Internal {
            message: format!("static field {class_name}.{field_name} is not an object"),
        })),
    }
}

tb_abstract_view_fns!(
    native_cb_slice,
    native_cb_slice2,
    native_cb_duplicate,
    native_cb_as_read_only,
    native_cb_order,
    "java/nio/CharBuffer",
    ArrayElementType::Char,
    "CharBuffer"
);
tb_abstract_view_fns!(
    native_ib_slice,
    native_ib_slice2,
    native_ib_duplicate,
    native_ib_as_read_only,
    native_ib_order,
    "java/nio/IntBuffer",
    ArrayElementType::Int,
    "IntBuffer"
);
tb_abstract_view_fns!(
    native_lb_slice,
    native_lb_slice2,
    native_lb_duplicate,
    native_lb_as_read_only,
    native_lb_order,
    "java/nio/LongBuffer",
    ArrayElementType::Long,
    "LongBuffer"
);
tb_abstract_view_fns!(
    native_fb_slice,
    native_fb_slice2,
    native_fb_duplicate,
    native_fb_as_read_only,
    native_fb_order,
    "java/nio/FloatBuffer",
    ArrayElementType::Float,
    "FloatBuffer"
);
tb_abstract_view_fns!(
    native_db_slice,
    native_db_slice2,
    native_db_duplicate,
    native_db_as_read_only,
    native_db_order,
    "java/nio/DoubleBuffer",
    ArrayElementType::Double,
    "DoubleBuffer"
);
tb_abstract_view_fns!(
    native_sb_slice,
    native_sb_slice2,
    native_sb_duplicate,
    native_sb_as_read_only,
    native_sb_order,
    "java/nio/ShortBuffer",
    ArrayElementType::Short,
    "ShortBuffer"
);

// --- CharBuffer ---
pub(crate) fn native_cb_allocate(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let cap = match args.first() {
        Some(Value::Int(v)) => (*v).max(0) as usize,
        _ => 0,
    };
    let cb = alloc_typed_buffer(ctx, "java/nio/CharBuffer", ArrayElementType::Char, cap);
    Ok(Some(Value::Object(Some(cb))))
}

pub(crate) fn native_cb_wrap(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let src = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Object(None))),
    };
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let src_pin = ctx.pin_native_root(src);
    let len = ctx.array_length(src);
    let cb = alloc_typed_buffer(ctx, "java/nio/CharBuffer", ArrayElementType::Char, len);
    let view = bb_state(ctx, cb)?;
    for i in 0..len {
        let src = ctx.read_native_pin(src_pin, src);
        let v = ctx.get_array_element(src, i);
        tb_write_elem(ctx, view, i, v)?;
    }
    buf_set_limit(ctx, cb, len as i32);
    Ok(Some(Value::Object(Some(cb))))
}

pub(crate) fn native_cb_wrap_charseq(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s = match args.first() {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    let chars: Vec<u16> = s.encode_utf16().collect();
    let len = chars.len();
    let cb = alloc_typed_buffer(ctx, "java/nio/CharBuffer", ArrayElementType::Char, len);
    let view = bb_state(ctx, cb)?;
    for (i, &ch) in chars.iter().enumerate() {
        tb_write_elem(ctx, view, i, Value::Int(ch as i32))?;
    }
    buf_set_limit(ctx, cb, len as i32);
    Ok(Some(Value::Object(Some(cb))))
}

pub(crate) fn native_cb_wrap_charseq_range(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s = match args.first() {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    let start = match args.get(1) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let end = match args.get(2) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let chars: Vec<u16> = s.encode_utf16().collect();
    let len = chars.len();
    let cb = alloc_typed_buffer(ctx, "java/nio/CharBuffer", ArrayElementType::Char, len);
    let view = bb_state(ctx, cb)?;
    for (i, &ch) in chars.iter().enumerate() {
        tb_write_elem(ctx, view, i, Value::Int(ch as i32))?;
    }
    buf_set_position(ctx, cb, start as i32);
    buf_set_limit(ctx, cb, end.min(len) as i32);
    Ok(Some(Value::Object(Some(cb))))
}

pub(crate) fn native_cb_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let view = bb_state(ctx, this)?;
    let (pos, lim) = (view.pos, view.lim);
    if pos >= lim {
        return Ok(Some(Value::Int(0)));
    }
    let val = tb_read_elem(ctx, view, pos as usize)?;
    buf_set_position(ctx, this, pos + 1);
    Ok(Some(val))
}

pub(crate) fn native_cb_get_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let view = bb_state(ctx, this)?;
    let cap = view.cap;
    if !tb_index_in_bounds(idx, cap) {
        return Err(buffer_index_out_of_bounds());
    }
    Ok(Some(tb_read_elem(ctx, view, idx as usize)?))
}

pub(crate) fn native_cb_put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    buffer_check_writable(ctx, this)?; // F37-1 §3
    let ch = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let view = bb_state(ctx, this)?;
    let (pos, lim) = (view.pos, view.lim);
    if pos < lim {
        tb_write_elem(ctx, view, pos as usize, Value::Int(ch))?;
        buf_set_position(ctx, this, pos + 1);
    }
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_cb_put_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    buffer_check_writable(ctx, this)?; // F37-1 §3
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let ch = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let view = bb_state(ctx, this)?;
    let cap = view.cap;
    if !tb_index_in_bounds(idx, cap) {
        return Err(buffer_index_out_of_bounds());
    }
    tb_write_elem(ctx, view, idx as usize, Value::Int(ch))?;
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_cb_put_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    buffer_check_writable(ctx, this)?; // F37-1 §3
    let s = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    let view = bb_state(ctx, this)?;
    let (mut pos, lim) = (view.pos, view.lim);
    for ch in s.encode_utf16() {
        if pos >= lim {
            break;
        }
        tb_write_elem(ctx, view, pos as usize, Value::Int(ch as i32))?;
        pos += 1;
    }
    buf_set_position(ctx, this, pos);
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_cb_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let view = bb_state(ctx, this)?;
    let (pos, lim) = (view.pos, view.lim);
    let mut chars = Vec::new();
    for i in pos..lim {
        if let Value::Int(v) = tb_read_elem(ctx, view, i as usize)? {
            chars.push(v as u16);
        }
    }
    let s = String::from_utf16_lossy(&chars);
    Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
}

pub(crate) fn native_cb_char_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let view = bb_state(ctx, this)?;
    let pos = view.pos;
    Ok(Some(tb_read_elem(ctx, view, (pos + idx) as usize)?))
}

pub(crate) fn native_cb_compact(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    buffer_check_writable(ctx, this)?; // F37-1 §3
    let view = bb_state(ctx, this)?;
    let (pos, lim, cap) = (view.pos, view.lim, view.cap);
    let remaining = lim - pos;
    for i in 0..remaining {
        let v = tb_read_elem(ctx, view, (pos + i) as usize)?;
        tb_write_elem(ctx, view, i as usize, v)?;
    }
    buf_set_position(ctx, this, remaining);
    buf_set_limit(ctx, this, cap);
    buf_set_mark(ctx, this, -1);
    Ok(Some(Value::Object(Some(this))))
}

// --- Typed buffer shared helpers ---

/// `CharBuffer.array()` / `IntBuffer.array()` / … — registered for all six
/// typed families.
///
/// This read raw slot 0 and returned whatever was there. Same species as
/// `bb_state`'s old fall-through, but reachable from Java rather than only via
/// an internal error: on a real-layout receiver slot 0 is
/// `java.nio.Buffer.mark`, so `array()` handed back `Int(-1)` where its
/// descriptor promises `[C`/`[I`/…; on a DIRECT view it invented a backing
/// array for a buffer that has none. HotSpot throws
/// `UnsupportedOperationException` for that case (`Buffer.array()`:
/// "@throws UnsupportedOperationException If this buffer is not backed by an
/// accessible array").
///
/// The storage-less case USED TO keep a "historic benign null", excused as a
/// behaviour change with no vector to measure. There is now a vector, and the
/// null was not benign: a `null` returned through a `()[I` descriptor reaches
/// the caller's `arraylength` as a `NullPointerException` at some unrelated
/// site, where HotSpot raises `UnsupportedOperationException` at the call
/// itself. MEASURED, jdk-25.0.3+9: `ByteBuffer.allocate(8).asIntBuffer()` —
/// the exact "neither array nor address" shape `native-builtins`' typed views
/// arrive in — answers `hasArray() == false`, `array()` →
/// `UnsupportedOperationException` (`getMessage()` null), `arrayOffset()` →
/// `UnsupportedOperationException`. Direct and storage-less are ONE arm here
/// because HotSpot gives them one answer; the split that matters is the
/// read-only one, which this had none of. See [`buffer_array_access`].
pub(crate) fn native_tb_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let arr = bb_resolve_heap_array(ctx, this);
    match buffer_array_access(arr.is_some(), buffer_is_read_only(ctx, this)) {
        BufferArrayAccess::Absent => Err(buffer_no_backing_array()),
        BufferArrayAccess::ReadOnly => Err(RuntimeError::ReadOnlyBufferException.into()),
        BufferArrayAccess::Accessible => Ok(Some(Value::Object(arr))),
    }
}

pub(crate) fn native_tb_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (pos, lim, cap) = buf_metadata(ctx, this);
    let s = format!("Buffer[pos={} lim={} cap={}]", pos, lim, cap);
    Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
}

pub(crate) fn native_tb_compact(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    buffer_check_writable(ctx, this)?; // F37-1 §3
    let view = bb_state(ctx, this)?;
    let (pos, lim, cap) = (view.pos, view.lim, view.cap);
    let remaining = lim - pos;
    for i in 0..remaining {
        let v = tb_read_elem(ctx, view, (pos + i) as usize)?;
        tb_write_elem(ctx, view, i as usize, v)?;
    }
    buf_set_position(ctx, this, remaining);
    buf_set_limit(ctx, this, cap);
    buf_set_mark(ctx, this, -1);
    Ok(Some(Value::Object(Some(this))))
}

// --- IntBuffer ---
pub(crate) fn native_ib_allocate(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let cap = match args.first() {
        Some(Value::Int(v)) => (*v).max(0) as usize,
        _ => 0,
    };
    let buf = alloc_typed_buffer(ctx, "java/nio/IntBuffer", ArrayElementType::Int, cap);
    Ok(Some(Value::Object(Some(buf))))
}

pub(crate) fn native_ib_wrap(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let src = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Object(None))),
    };
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let src_pin = ctx.pin_native_root(src);
    let len = ctx.array_length(src);
    let buf = alloc_typed_buffer(ctx, "java/nio/IntBuffer", ArrayElementType::Int, len);
    let view = bb_state(ctx, buf)?;
    for i in 0..len {
        let src = ctx.read_native_pin(src_pin, src);
        let v = ctx.get_array_element(src, i);
        tb_write_elem(ctx, view, i, v)?;
    }
    buf_set_limit(ctx, buf, len as i32);
    Ok(Some(Value::Object(Some(buf))))
}

pub(crate) fn native_tb_get_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let view = bb_state(ctx, this)?;
    let (pos, lim) = (view.pos, view.lim);
    if pos >= lim {
        return Ok(Some(Value::Int(0)));
    }
    let val = tb_read_elem(ctx, view, pos as usize)?;
    buf_set_position(ctx, this, pos + 1);
    Ok(Some(val))
}

pub(crate) fn native_tb_get_int_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let view = bb_state(ctx, this)?;
    let cap = view.cap;
    if !tb_index_in_bounds(idx, cap) {
        return Err(buffer_index_out_of_bounds());
    }
    Ok(Some(tb_read_elem(ctx, view, idx as usize)?))
}

pub(crate) fn native_tb_put_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    buffer_check_writable(ctx, this)?; // F37-1 §3
    let val = args.get(1).cloned().unwrap_or(Value::Int(0));
    let view = bb_state(ctx, this)?;
    let (pos, lim) = (view.pos, view.lim);
    if pos < lim {
        tb_write_elem(ctx, view, pos as usize, val)?;
        buf_set_position(ctx, this, pos + 1);
    }
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_tb_put_int_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    buffer_check_writable(ctx, this)?; // F37-1 §3
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let val = args.get(2).cloned().unwrap_or(Value::Int(0));
    let view = bb_state(ctx, this)?;
    let cap = view.cap;
    if !tb_index_in_bounds(idx, cap) {
        return Err(buffer_index_out_of_bounds());
    }
    tb_write_elem(ctx, view, idx as usize, val)?;
    Ok(Some(Value::Object(Some(this))))
}

// --- LongBuffer ---
pub(crate) fn native_lb_allocate(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let cap = match args.first() {
        Some(Value::Int(v)) => (*v).max(0) as usize,
        _ => 0,
    };
    let buf = alloc_typed_buffer(ctx, "java/nio/LongBuffer", ArrayElementType::Long, cap);
    Ok(Some(Value::Object(Some(buf))))
}

pub(crate) fn native_lb_wrap(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let src = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Object(None))),
    };
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let src_pin = ctx.pin_native_root(src);
    let len = ctx.array_length(src);
    let buf = alloc_typed_buffer(ctx, "java/nio/LongBuffer", ArrayElementType::Long, len);
    let view = bb_state(ctx, buf)?;
    for i in 0..len {
        let src = ctx.read_native_pin(src_pin, src);
        let v = ctx.get_array_element(src, i);
        tb_write_elem(ctx, view, i, v)?;
    }
    buf_set_limit(ctx, buf, len as i32);
    Ok(Some(Value::Object(Some(buf))))
}

pub(crate) fn native_tb_get_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let view = bb_state(ctx, this)?;
    let (pos, lim) = (view.pos, view.lim);
    if pos >= lim {
        return Ok(Some(Value::Long(0)));
    }
    let val = tb_read_elem(ctx, view, pos as usize)?;
    buf_set_position(ctx, this, pos + 1);
    Ok(Some(val))
}

pub(crate) fn native_tb_get_long_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let view = bb_state(ctx, this)?;
    let cap = view.cap;
    if !tb_index_in_bounds(idx, cap) {
        return Err(buffer_index_out_of_bounds());
    }
    Ok(Some(tb_read_elem(ctx, view, idx as usize)?))
}

pub(crate) fn native_tb_put_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    buffer_check_writable(ctx, this)?; // F37-1 §3
    let val = args.get(1).cloned().unwrap_or(Value::Long(0));
    let view = bb_state(ctx, this)?;
    let (pos, lim) = (view.pos, view.lim);
    if pos < lim {
        tb_write_elem(ctx, view, pos as usize, val)?;
        buf_set_position(ctx, this, pos + 1);
    }
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_tb_put_long_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    buffer_check_writable(ctx, this)?; // F37-1 §3
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let val = args.get(2).cloned().unwrap_or(Value::Long(0));
    let view = bb_state(ctx, this)?;
    let cap = view.cap;
    if !tb_index_in_bounds(idx, cap) {
        return Err(buffer_index_out_of_bounds());
    }
    tb_write_elem(ctx, view, idx as usize, val)?;
    Ok(Some(Value::Object(Some(this))))
}

// --- FloatBuffer ---
pub(crate) fn native_fb_allocate(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let cap = match args.first() {
        Some(Value::Int(v)) => (*v).max(0) as usize,
        _ => 0,
    };
    let buf = alloc_typed_buffer(ctx, "java/nio/FloatBuffer", ArrayElementType::Float, cap);
    Ok(Some(Value::Object(Some(buf))))
}

pub(crate) fn native_fb_wrap(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let src = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Object(None))),
    };
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let src_pin = ctx.pin_native_root(src);
    let len = ctx.array_length(src);
    let buf = alloc_typed_buffer(ctx, "java/nio/FloatBuffer", ArrayElementType::Float, len);
    let view = bb_state(ctx, buf)?;
    for i in 0..len {
        let src = ctx.read_native_pin(src_pin, src);
        let v = ctx.get_array_element(src, i);
        tb_write_elem(ctx, view, i, v)?;
    }
    buf_set_limit(ctx, buf, len as i32);
    Ok(Some(Value::Object(Some(buf))))
}

pub(crate) fn native_tb_get_float(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Float(0.0))),
    };
    let view = bb_state(ctx, this)?;
    let (pos, lim) = (view.pos, view.lim);
    if pos >= lim {
        return Ok(Some(Value::Float(0.0)));
    }
    let val = tb_read_elem(ctx, view, pos as usize)?;
    buf_set_position(ctx, this, pos + 1);
    Ok(Some(val))
}

pub(crate) fn native_tb_get_float_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Float(0.0))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let view = bb_state(ctx, this)?;
    let cap = view.cap;
    if !tb_index_in_bounds(idx, cap) {
        return Err(buffer_index_out_of_bounds());
    }
    Ok(Some(tb_read_elem(ctx, view, idx as usize)?))
}

pub(crate) fn native_tb_put_float(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    buffer_check_writable(ctx, this)?; // F37-1 §3
    let val = args.get(1).cloned().unwrap_or(Value::Float(0.0));
    let view = bb_state(ctx, this)?;
    let (pos, lim) = (view.pos, view.lim);
    if pos < lim {
        tb_write_elem(ctx, view, pos as usize, val)?;
        buf_set_position(ctx, this, pos + 1);
    }
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_tb_put_float_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    buffer_check_writable(ctx, this)?; // F37-1 §3
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let val = args.get(2).cloned().unwrap_or(Value::Float(0.0));
    let view = bb_state(ctx, this)?;
    let cap = view.cap;
    if !tb_index_in_bounds(idx, cap) {
        return Err(buffer_index_out_of_bounds());
    }
    tb_write_elem(ctx, view, idx as usize, val)?;
    Ok(Some(Value::Object(Some(this))))
}

// --- DoubleBuffer ---
pub(crate) fn native_db_allocate(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let cap = match args.first() {
        Some(Value::Int(v)) => (*v).max(0) as usize,
        _ => 0,
    };
    let buf = alloc_typed_buffer(ctx, "java/nio/DoubleBuffer", ArrayElementType::Double, cap);
    Ok(Some(Value::Object(Some(buf))))
}

pub(crate) fn native_db_wrap(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let src = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Object(None))),
    };
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let src_pin = ctx.pin_native_root(src);
    let len = ctx.array_length(src);
    let buf = alloc_typed_buffer(ctx, "java/nio/DoubleBuffer", ArrayElementType::Double, len);
    let view = bb_state(ctx, buf)?;
    for i in 0..len {
        let src = ctx.read_native_pin(src_pin, src);
        let v = ctx.get_array_element(src, i);
        tb_write_elem(ctx, view, i, v)?;
    }
    buf_set_limit(ctx, buf, len as i32);
    Ok(Some(Value::Object(Some(buf))))
}

pub(crate) fn native_tb_get_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Double(0.0))),
    };
    let view = bb_state(ctx, this)?;
    let (pos, lim) = (view.pos, view.lim);
    if pos >= lim {
        return Ok(Some(Value::Double(0.0)));
    }
    let val = tb_read_elem(ctx, view, pos as usize)?;
    buf_set_position(ctx, this, pos + 1);
    Ok(Some(val))
}

pub(crate) fn native_tb_get_double_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Double(0.0))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let view = bb_state(ctx, this)?;
    let cap = view.cap;
    if !tb_index_in_bounds(idx, cap) {
        return Err(buffer_index_out_of_bounds());
    }
    Ok(Some(tb_read_elem(ctx, view, idx as usize)?))
}

pub(crate) fn native_tb_put_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    buffer_check_writable(ctx, this)?; // F37-1 §3
    let val = args.get(1).cloned().unwrap_or(Value::Double(0.0));
    let view = bb_state(ctx, this)?;
    let (pos, lim) = (view.pos, view.lim);
    if pos < lim {
        tb_write_elem(ctx, view, pos as usize, val)?;
        buf_set_position(ctx, this, pos + 1);
    }
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_tb_put_double_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    buffer_check_writable(ctx, this)?; // F37-1 §3
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let val = args.get(2).cloned().unwrap_or(Value::Double(0.0));
    let view = bb_state(ctx, this)?;
    let cap = view.cap;
    if !tb_index_in_bounds(idx, cap) {
        return Err(buffer_index_out_of_bounds());
    }
    tb_write_elem(ctx, view, idx as usize, val)?;
    Ok(Some(Value::Object(Some(this))))
}

// --- ShortBuffer ---
pub(crate) fn native_sb_allocate(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let cap = match args.first() {
        Some(Value::Int(v)) => (*v).max(0) as usize,
        _ => 0,
    };
    let buf = alloc_typed_buffer(ctx, "java/nio/ShortBuffer", ArrayElementType::Short, cap);
    Ok(Some(Value::Object(Some(buf))))
}

pub(crate) fn native_sb_wrap(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let src = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Object(None))),
    };
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let src_pin = ctx.pin_native_root(src);
    let len = ctx.array_length(src);
    let buf = alloc_typed_buffer(ctx, "java/nio/ShortBuffer", ArrayElementType::Short, len);
    let view = bb_state(ctx, buf)?;
    for i in 0..len {
        let src = ctx.read_native_pin(src_pin, src);
        let v = ctx.get_array_element(src, i);
        tb_write_elem(ctx, view, i, v)?;
    }
    buf_set_limit(ctx, buf, len as i32);
    Ok(Some(Value::Object(Some(buf))))
}

pub(crate) fn native_tb_get_short(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let view = bb_state(ctx, this)?;
    let (pos, lim) = (view.pos, view.lim);
    if pos >= lim {
        return Ok(Some(Value::Int(0)));
    }
    let val = tb_read_elem(ctx, view, pos as usize)?;
    buf_set_position(ctx, this, pos + 1);
    Ok(Some(val))
}

pub(crate) fn native_tb_get_short_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let view = bb_state(ctx, this)?;
    let cap = view.cap;
    if !tb_index_in_bounds(idx, cap) {
        return Err(buffer_index_out_of_bounds());
    }
    Ok(Some(tb_read_elem(ctx, view, idx as usize)?))
}

pub(crate) fn native_tb_put_short(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    buffer_check_writable(ctx, this)?; // F37-1 §3
    let val = args.get(1).cloned().unwrap_or(Value::Int(0));
    let view = bb_state(ctx, this)?;
    let (pos, lim) = (view.pos, view.lim);
    if pos < lim {
        tb_write_elem(ctx, view, pos as usize, val)?;
        buf_set_position(ctx, this, pos + 1);
    }
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn native_tb_put_short_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    buffer_check_writable(ctx, this)?; // F37-1 §3
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let val = args.get(2).cloned().unwrap_or(Value::Int(0));
    let view = bb_state(ctx, this)?;
    let cap = view.cap;
    if !tb_index_in_bounds(idx, cap) {
        return Err(buffer_index_out_of_bounds());
    }
    tb_write_elem(ctx, view, idx as usize, val)?;
    Ok(Some(Value::Object(Some(this))))
}

// ===========================================================================
// Phase 45: NIO Channel Extras
// FileLock, MappedByteBuffer, FileChannel additions, Files.walk/list
// ===========================================================================

// --- FileLock layout: 4 real fields + 2 appended private slots ---
//
// MEASURED, so the next census does not re-derive it and does not read the
// 6-vs-4 `direction=over` row `CRATONVM_DBG_LAYOUT_ALIAS` prints here as a
// defect. `javap -p java.nio.channels.FileLock` on JDK 25.0.3.9 declares
// exactly four instance fields, in this order, on a class whose superclass is
// `java.lang.Object`:
//
//     private final java.nio.channels.Channel channel;   // 0
//     private final long position;                       // 1
//     private final long size;                           // 2
//     private final boolean shared;                      // 3
//
// Slots 0..3 below therefore ALIAS the real fields exactly, by index and by
// type, which is what makes the real `final` accessors `channel()`,
// `position()`, `size()` and `isShared()` return our values when they run.
// `FL_FIELD_VALID` and `FL_FIELD_TOKEN` are CratonVM's own state and start at
// 4, above everything the class declares — i.e. this is already the
// appended-slot idiom W7-49 §8 named (`try_alloc_with_appended_slots`),
// written out by hand. The over-allocation is deliberate and safe, and it is
// the reason the `over` direction is not on its own a defect predicate: the
// landed remedy for an over-allocation IS an over-allocation.
//
// Left as literal indices rather than a computed base on purpose: these
// natives also run on receivers they did not allocate (in real-JDK mode
// `<init>` lands on a real `sun.nio.ch.FileLockImpl`, five fields wide), and
// W7-49 §8 records why a per-receiver base cannot be recovered from a foreign
// object's width. W7-66-live-over-allocations.md.
pub(crate) const FL_FIELD_CHANNEL: usize = 0; // Object: owning FileChannel
pub(crate) const FL_FIELD_POSITION: usize = 1; // Long: lock start position
pub(crate) const FL_FIELD_SIZE: usize = 2; // Long: lock region size
pub(crate) const FL_FIELD_SHARED: usize = 3; // Int: 1=shared, 0=exclusive
pub(crate) const FL_FIELD_VALID: usize = 4; // Int: 1=valid, 0=released
                                 // Slot 5: Long — non-zero in-process registry id (from `next_lock_token`).
                                 // Used by `release` to find the matching entry in `file_locks()` and drop
                                 // it. 0 means the FileLock is not registered (e.g. construction failed).
pub(crate) const FL_FIELD_TOKEN: usize = 5;
pub(crate) const FL_NUM_FIELDS: usize = 6;

// ---------------------------------------------------------------------------
// FileLock registry — process-local + cross-process via OS primitives.
//
// Two layers cooperate:
//
//   1. Process-local `FILE_LOCKS` map (introduced in round-8) keyed by FdId
//      that tracks every region currently held by *this* JVM. It rejects
//      conflicting overlaps from sibling threads / channels in the same
//      process *before* hitting the kernel — POSIX `fcntl(F_SETLK)` is
//      famously per-process (two open fds on the same file in one process
//      do NOT see each other's locks), and `LockFileEx` is per-handle, so
//      the in-process table is what gives us the JVM-correct semantic of
//      "two FileChannels on the same fd cannot hold overlapping exclusive
//      locks". Without it, two threads opening the same path inside one
//      VM would both `LockFileEx` and *both* succeed.
//
//   2. OS-level lock (round-9): once the in-process registry accepts the
//      region, we additionally hold a kernel-managed advisory lock via
//      `fcntl(F_SETLK, &flock)` on Unix or `LockFileEx` on Windows. This
//      is what gives cross-process coordination — another OS process that
//      tries to lock the same byte range will see EAGAIN/EWOULDBLOCK and
//      `tryLock` returns null, matching the JDK contract.
//
// The OS lock is associated with a cloned `std::fs::File` (so the handle
// outlives the original fd close) stored in `OS_LOCK_HANDLES` keyed by the
// in-process token. `release` looks up the handle, issues F_UNLCK /
// `UnlockFileEx`, then drops both the OS entry and the registry entry.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub(crate) struct LockRegion {
    /// Unique per-process token; matches `FL_FIELD_TOKEN` on the Java side.
    pub(crate) token: i64,
    pub(crate) position: i64,
    /// Lock length, or `i64::MAX` for "rest of file" / whole-file locks.
    pub(crate) size: i64,
    /// `true` for shared (read) locks, `false` for exclusive (write).
    pub(crate) shared: bool,
}

/// Per-fd list of currently-held lock regions. Vec is fine — the JDK
/// itself permits "many" locks per channel but in practice a handful
/// is the max; iterating to check conflicts is cheaper than a tree
/// for n<32.
pub(crate) static FILE_LOCKS: OnceLock<Mutex<HashMap<i64, Vec<LockRegion>>>> = OnceLock::new();

pub(crate) fn file_locks() -> &'static Mutex<HashMap<i64, Vec<LockRegion>>> {
    FILE_LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn next_lock_token() -> i64 {
    static N: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);
    N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// OS-level lock-handle table (round-9 cross-process locking).
//
// When we successfully `fcntl(F_SETLK)` / `LockFileEx` a region we stash
// the cloned `std::fs::File` here keyed by the in-process lock token so
// that (a) the kernel handle stays open until `release()` runs, and
// (b) we can find the right handle on unlock without re-cloning from the
// original fd (which may have already been closed). Tuple stores the
// file, the position, and the length we passed to the OS — needed by
// `UnlockFileEx` / `F_UNLCK` to undo the exact same range.
// ---------------------------------------------------------------------------

pub(crate) struct OsLockHandle {
    /// Cloned file that keeps the OS-level lock alive. Drop releases the
    /// OS lock implicitly on Unix (close → unlock), but we still issue an
    /// explicit unlock in `release_os_lock` to be deterministic on
    /// Windows where the order matters.
    pub(crate) file: std::fs::File,
    pub(crate) position: i64,
    /// Size as passed to the OS — `i64::MAX`/`0` means "to EOF" which we
    /// translate platform-specifically.
    pub(crate) size: i64,
}

pub(crate) static OS_LOCK_HANDLES: OnceLock<Mutex<HashMap<i64, OsLockHandle>>> = OnceLock::new();

pub(crate) fn os_lock_handles() -> &'static Mutex<HashMap<i64, OsLockHandle>> {
    OS_LOCK_HANDLES.get_or_init(|| Mutex::new(HashMap::new()))
}

