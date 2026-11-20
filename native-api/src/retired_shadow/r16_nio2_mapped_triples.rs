#[allow(unused_imports)]
use super::*;

/// Round 16 wave 16 (lane nio2), 2026-10-02 -- `MappedByteBuffer.force()` and
/// `load()`, the two `MappedByteBuffer` natives `isLoaded()` left behind when
/// `RETIRED_SHADOW_L4_BUFFERS2_TRIPLES` retired it.
///
/// # The door that reaches them for a real receiver
///
/// Every mapped buffer the real image builds is a `java.nio.DirectByteBuffer`
/// (or `DirectByteBufferR`). Neither declares `force()` or `load()`: both are
/// `public final` with `Code` on `MappedByteBuffer`. So step 1 finds nothing on
/// the receiver's class, and the superclass walk (`invoke_or_native`'s
/// `ParentShadow` arm, the stackless step-1 walk, the populate walk) stops at
/// `MappedByteBuffer`, finds the live `Bridge` there, and runs it: §1.4's
/// shadow, with the dial off. The natives read a private appended slot that
/// only the VM's own `FileChannel.map` fills (`MBB_PRIVATE_MAPPED_ADDR`). On a
/// real buffer that slot is `DirectByteBuffer`'s own first field, so `force()`
/// never reached `msync` / `FlushViewOfFile` and `load()` never reached
/// `madvise` or touched a page. Both returned `this`, so no probe output moved.
///
/// # The screen, read on this tree
///
/// * Real bytecode to yield to: both rows are declared with `Code` on JDK 17,
///   21 and 25. `force()` is `fd == null ? this : force(0, capacity())` (when
///   `isSync` or a non-empty mapping), and `load()` goes through
///   `ScopedMemoryAccess.load` to `MappedMemoryUtils.load`, whose natives
///   (`isLoaded0`, `load0`, `force0`, `registerNatives`) are wave 15's real
///   bodies (`native-io/src/file_channel.rs` `register_mapped_memory_utils`,
///   Bridges in every mode). `force(int,int)` was never registered, so that
///   half of the path is already what runs under `--real-jdk` today.
/// * Unconstructed carrier: the only producer of a buffer these natives can
///   serve is `native_fc_map` on the ABSTRACT `FileChannel.map`, reached only
///   for a VM-minted `FileChannel` (the legacy fallback in
///   `phases_late/nio_file.rs`). Its sibling `isLoaded()` was retired on the
///   same argument in wave 17 of lane 4, so the family is now retired whole.
/// * VM invariant: none. Nothing in `vm/` names `MappedByteBuffer`.
/// * Registration: `register_nio_channel_extras` (`native-io/src/lib.rs`), one
///   explicit `Bridge` scope, each triple once, every arm.
///
/// `--compatible` is unchanged by this table (a `SyntheticStub` registers and
/// dispatches there as the `Bridge` did); its real receivers are screened at
/// the native instead (`CRATONVM_MAPPED_BUFFER_RECEIVER_SCREEN`, NIO-4).
///
/// Control and kill switch (one rule, two rows):
/// `CRATONVM_UNRETIRE_NATIVE_SHADOW=java/nio/MappedByteBuffer.force,java/nio/MappedByteBuffer.load`.
/// Probe: `C:\craton\jitr16-probes\src\R16Nio2Census.java` (`mbbforce`,
/// `mbbload`). Written from reading, not from a trial binary.
pub(in crate::retired_shadow) static RETIRED_SHADOW_R16_NIO2_MAPPED_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/nio/MappedByteBuffer",
        "force",
        "()Ljava/nio/MappedByteBuffer;",
    ),
    (
        "java/nio/MappedByteBuffer",
        "load",
        "()Ljava/nio/MappedByteBuffer;",
    ),
];

/// JIT round 19 (nio19, 2026-10-07): the VM's own `java.nio.channels.Pipe`.
///
/// `native-io/src/pipe.rs` registered a whole fabricated pipe as `Bridge`s over
/// real JDK classes: the static `Pipe.open()`, `PipeImpl.source()` / `sink()`,
/// and the read / write / close / `configureBlocking` surface of
/// `SourceChannelImpl` / `SinkChannelImpl`. Every one of them won over the
/// JDK's bytecode for every real receiver (census of `PipeCls` under
/// `--real-jdk`: `Pipe.open` 1, `PipeImpl.source` 6, `SourceChannelImpl.read`
/// 1, ...), and the stand-in is wrong where HotSpot is not:
///
/// * `configureBlocking(false)` records a flag the read path never consults,
///   so `source().read(buf)` on an empty non-blocking pipe parks forever where
///   HotSpot answers `0` (`NioConf19` row `pipe`, both modes: the VM hung);
/// * the carrier has no `fd` / `fdVal`, so it cannot be registered with a real
///   `Selector`.
///
/// # The screen, read on this tree
///
/// * Real bytecode to yield to: every row is declared with `Code` on JDK 17,
///   21 and 25 (or inherited from `AbstractSelectableChannel` /
///   `AbstractInterruptibleChannel`, where the walk finds it), and the JDK's
///   `PipeImpl` is built on `IOUtil.makePipe` (`native-io/src/net_nio_real.rs`,
///   `pipe(2)`), `IOUtil.configureBlocking`, `FileDispatcherImpl.read0` /
///   `write0` and the real selector -- natives that already serve
///   `SocketChannel` in both modes.
/// * Unconstructed carrier: nothing else mints a `sun/nio/ch/PipeImpl`; the
///   fabricated one came only from `Pipe.open` here.
/// * VM invariant: none (nothing in `vm/` names a pipe class).
/// * Registration: `native-io/src/pipe.rs` `register_pipe_real`, one explicit
///   `Bridge` scope, each triple once.
///
/// `--compatible` adopts the family through the `JdkPipe` HotSpot-alignment
/// row (`COMPAT_FAMILIES`), so the stand-in is gone in both modes over a real
/// JDK. Kill switch: `CRATONVM_UNRETIRE_NATIVE_SHADOW=<class.method,...>` for
/// `--real-jdk`, `CRATONVM_COMPAT_ALIGN=-jdk-pipe` for `--compatible`.
/// Probes `NioConf19` (`pipe`, `transfer-pipe`), `PipeHang`, `PipeCls`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_R19_NIO_PIPE_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/nio/channels/Pipe",
        "open",
        "()Ljava/nio/channels/Pipe;",
    ),
    (
        "java/nio/channels/Pipe",
        "sink",
        "()Ljava/nio/channels/Pipe$SinkChannel;",
    ),
    (
        "java/nio/channels/Pipe",
        "source",
        "()Ljava/nio/channels/Pipe$SourceChannel;",
    ),
    (
        "sun/nio/ch/PipeImpl",
        "sink",
        "()Ljava/nio/channels/Pipe$SinkChannel;",
    ),
    (
        "sun/nio/ch/PipeImpl",
        "source",
        "()Ljava/nio/channels/Pipe$SourceChannel;",
    ),
    ("sun/nio/ch/SinkChannelImpl", "close", "()V"),
    (
        "sun/nio/ch/SinkChannelImpl",
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
    ),
    ("sun/nio/ch/SinkChannelImpl", "isOpen", "()Z"),
    (
        "sun/nio/ch/SinkChannelImpl",
        "write",
        "(Ljava/nio/ByteBuffer;)I",
    ),
    (
        "sun/nio/ch/SinkChannelImpl",
        "write",
        "([Ljava/nio/ByteBuffer;)J",
    ),
    (
        "sun/nio/ch/SinkChannelImpl",
        "write",
        "([Ljava/nio/ByteBuffer;II)J",
    ),
    ("sun/nio/ch/SourceChannelImpl", "close", "()V"),
    (
        "sun/nio/ch/SourceChannelImpl",
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
    ),
    ("sun/nio/ch/SourceChannelImpl", "isOpen", "()Z"),
    (
        "sun/nio/ch/SourceChannelImpl",
        "read",
        "(Ljava/nio/ByteBuffer;)I",
    ),
    (
        "sun/nio/ch/SourceChannelImpl",
        "read",
        "([Ljava/nio/ByteBuffer;)J",
    ),
    (
        "sun/nio/ch/SourceChannelImpl",
        "read",
        "([Ljava/nio/ByteBuffer;II)J",
    ),
];

/// JIT round 19 (nio19, 2026-10-07): the buffer rows on ABSTRACT declarations.
///
/// `Buffer` and the eight `X`-Buffer classes declare their accessors, views
/// and mutators `abstract` (`get`, `put`, `getInt`, `asIntBuffer`, `slice`,
/// `duplicate`, `compact`, `order`, `isDirect`, `isReadOnly`, ...; `javap` on
/// JDK 25: 100 rows abstract on the registering class, 10 more inherited
/// abstract -- `isReadOnly` on the eight families, `isDirect` / `isReadOnly`
/// on `MappedByteBuffer`). The `Bridge`s registered on them (`servlet.rs`
/// `register_s2_bytebuffer` / typed loops, `charset_buffers.rs`, the
/// `CharBuffer` rows in `register_charset_natives`) serve only a VM-minted
/// carrier whose class IS the abstract one: every concrete JDK buffer declares
/// each of these methods, so a real receiver never runs them (NIO-1 census:
/// zero invocations on every buffer row).
///
/// They were not free. A registered native on the constant-pool triple makes
/// the inline resolver refuse every call site typed by the abstract class
/// (`inline-resolve REFUSED java/nio/CharBuffer.get()C: native-shadow`, the
/// `heapcb` row of `R17NioRows`), so `ByteBuffer.getInt()`,
/// `CharBuffer.get()`, `IntBuffer.put(int)` ... stayed calls in compiled code.
/// That is NIO2-3 / NIO2-4 of the round-16 nio2 book, closed here.
///
/// # The screen, read on this tree
///
/// * Real bytecode to yield to: none needed -- the methods are abstract; a real
///   receiver dispatches to its own class's body in every mode.
/// * Unconstructed carrier: under `--real-jdk` no native mints an instance of
///   an abstract buffer class (the uninstantiable-receiver census of every
///   `NioConf19` / R16-R18 buffer probe run names none); the minting natives
///   (`ByteBuffer.allocate` / `wrap`, the `s2_*` views, the synthetic
///   `FileChannel`) are `SyntheticStub`s or retired rows there.
///   `--compatible` keeps them registered unless `JdkNioBuffers` drops the
///   minting family with them (`COMPAT_FAMILIES`).
/// * VM invariant: none.
///
/// Kill switch: `CRATONVM_UNRETIRE_NATIVE_SHADOW=<class.method,...>`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_R19_NIO_ABSTRACT_BUFFER_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/nio/Buffer", "isDirect", "()Z"),
    ("java/nio/Buffer", "isReadOnly", "()Z"),
    (
        "java/nio/ByteBuffer",
        "asCharBuffer",
        "()Ljava/nio/CharBuffer;",
    ),
    (
        "java/nio/ByteBuffer",
        "asDoubleBuffer",
        "()Ljava/nio/DoubleBuffer;",
    ),
    (
        "java/nio/ByteBuffer",
        "asFloatBuffer",
        "()Ljava/nio/FloatBuffer;",
    ),
    (
        "java/nio/ByteBuffer",
        "asIntBuffer",
        "()Ljava/nio/IntBuffer;",
    ),
    (
        "java/nio/ByteBuffer",
        "asLongBuffer",
        "()Ljava/nio/LongBuffer;",
    ),
    (
        "java/nio/ByteBuffer",
        "asReadOnlyBuffer",
        "()Ljava/nio/ByteBuffer;",
    ),
    (
        "java/nio/ByteBuffer",
        "asShortBuffer",
        "()Ljava/nio/ShortBuffer;",
    ),
    ("java/nio/ByteBuffer", "compact", "()Ljava/nio/ByteBuffer;"),
    (
        "java/nio/ByteBuffer",
        "duplicate",
        "()Ljava/nio/ByteBuffer;",
    ),
    ("java/nio/ByteBuffer", "get", "()B"),
    ("java/nio/ByteBuffer", "get", "(I)B"),
    ("java/nio/ByteBuffer", "getChar", "()C"),
    ("java/nio/ByteBuffer", "getChar", "(I)C"),
    ("java/nio/ByteBuffer", "getDouble", "()D"),
    ("java/nio/ByteBuffer", "getFloat", "()F"),
    ("java/nio/ByteBuffer", "getFloat", "(I)F"),
    ("java/nio/ByteBuffer", "getInt", "()I"),
    ("java/nio/ByteBuffer", "getInt", "(I)I"),
    ("java/nio/ByteBuffer", "getLong", "()J"),
    ("java/nio/ByteBuffer", "getLong", "(I)J"),
    ("java/nio/ByteBuffer", "getShort", "()S"),
    ("java/nio/ByteBuffer", "getShort", "(I)S"),
    ("java/nio/ByteBuffer", "isDirect", "()Z"),
    ("java/nio/ByteBuffer", "isReadOnly", "()Z"),
    ("java/nio/ByteBuffer", "put", "(B)Ljava/nio/ByteBuffer;"),
    ("java/nio/ByteBuffer", "put", "(IB)Ljava/nio/ByteBuffer;"),
    ("java/nio/ByteBuffer", "putChar", "(C)Ljava/nio/ByteBuffer;"),
    (
        "java/nio/ByteBuffer",
        "putChar",
        "(IC)Ljava/nio/ByteBuffer;",
    ),
    (
        "java/nio/ByteBuffer",
        "putDouble",
        "(D)Ljava/nio/ByteBuffer;",
    ),
    (
        "java/nio/ByteBuffer",
        "putFloat",
        "(F)Ljava/nio/ByteBuffer;",
    ),
    ("java/nio/ByteBuffer", "putInt", "(I)Ljava/nio/ByteBuffer;"),
    ("java/nio/ByteBuffer", "putInt", "(II)Ljava/nio/ByteBuffer;"),
    (
        "java/nio/ByteBuffer",
        "putLong",
        "(IJ)Ljava/nio/ByteBuffer;",
    ),
    ("java/nio/ByteBuffer", "putLong", "(J)Ljava/nio/ByteBuffer;"),
    (
        "java/nio/ByteBuffer",
        "putShort",
        "(IS)Ljava/nio/ByteBuffer;",
    ),
    (
        "java/nio/ByteBuffer",
        "putShort",
        "(S)Ljava/nio/ByteBuffer;",
    ),
    ("java/nio/ByteBuffer", "slice", "()Ljava/nio/ByteBuffer;"),
    ("java/nio/ByteBuffer", "slice", "(II)Ljava/nio/ByteBuffer;"),
    ("java/nio/CharBuffer", "get", "()C"),
    ("java/nio/CharBuffer", "get", "(I)C"),
    ("java/nio/CharBuffer", "isDirect", "()Z"),
    ("java/nio/CharBuffer", "isReadOnly", "()Z"),
    ("java/nio/CharBuffer", "order", "()Ljava/nio/ByteOrder;"),
    ("java/nio/CharBuffer", "put", "(C)Ljava/nio/CharBuffer;"),
    (
        "java/nio/CharBuffer",
        "subSequence",
        "(II)Ljava/nio/CharBuffer;",
    ),
    ("java/nio/CharBuffer", "toString", "(II)Ljava/lang/String;"),
    (
        "java/nio/DoubleBuffer",
        "asReadOnlyBuffer",
        "()Ljava/nio/DoubleBuffer;",
    ),
    (
        "java/nio/DoubleBuffer",
        "compact",
        "()Ljava/nio/DoubleBuffer;",
    ),
    (
        "java/nio/DoubleBuffer",
        "duplicate",
        "()Ljava/nio/DoubleBuffer;",
    ),
    ("java/nio/DoubleBuffer", "get", "()D"),
    ("java/nio/DoubleBuffer", "get", "(I)D"),
    ("java/nio/DoubleBuffer", "isDirect", "()Z"),
    ("java/nio/DoubleBuffer", "isReadOnly", "()Z"),
    ("java/nio/DoubleBuffer", "order", "()Ljava/nio/ByteOrder;"),
    ("java/nio/DoubleBuffer", "put", "(D)Ljava/nio/DoubleBuffer;"),
    (
        "java/nio/DoubleBuffer",
        "put",
        "(ID)Ljava/nio/DoubleBuffer;",
    ),
    (
        "java/nio/DoubleBuffer",
        "slice",
        "()Ljava/nio/DoubleBuffer;",
    ),
    (
        "java/nio/DoubleBuffer",
        "slice",
        "(II)Ljava/nio/DoubleBuffer;",
    ),
    (
        "java/nio/FloatBuffer",
        "asReadOnlyBuffer",
        "()Ljava/nio/FloatBuffer;",
    ),
    (
        "java/nio/FloatBuffer",
        "compact",
        "()Ljava/nio/FloatBuffer;",
    ),
    (
        "java/nio/FloatBuffer",
        "duplicate",
        "()Ljava/nio/FloatBuffer;",
    ),
    ("java/nio/FloatBuffer", "get", "()F"),
    ("java/nio/FloatBuffer", "get", "(I)F"),
    ("java/nio/FloatBuffer", "isDirect", "()Z"),
    ("java/nio/FloatBuffer", "isReadOnly", "()Z"),
    ("java/nio/FloatBuffer", "order", "()Ljava/nio/ByteOrder;"),
    ("java/nio/FloatBuffer", "put", "(F)Ljava/nio/FloatBuffer;"),
    ("java/nio/FloatBuffer", "put", "(IF)Ljava/nio/FloatBuffer;"),
    ("java/nio/FloatBuffer", "slice", "()Ljava/nio/FloatBuffer;"),
    (
        "java/nio/FloatBuffer",
        "slice",
        "(II)Ljava/nio/FloatBuffer;",
    ),
    (
        "java/nio/IntBuffer",
        "asReadOnlyBuffer",
        "()Ljava/nio/IntBuffer;",
    ),
    ("java/nio/IntBuffer", "compact", "()Ljava/nio/IntBuffer;"),
    ("java/nio/IntBuffer", "duplicate", "()Ljava/nio/IntBuffer;"),
    ("java/nio/IntBuffer", "get", "()I"),
    ("java/nio/IntBuffer", "get", "(I)I"),
    ("java/nio/IntBuffer", "isDirect", "()Z"),
    ("java/nio/IntBuffer", "isReadOnly", "()Z"),
    ("java/nio/IntBuffer", "order", "()Ljava/nio/ByteOrder;"),
    ("java/nio/IntBuffer", "put", "(I)Ljava/nio/IntBuffer;"),
    ("java/nio/IntBuffer", "put", "(II)Ljava/nio/IntBuffer;"),
    ("java/nio/IntBuffer", "slice", "()Ljava/nio/IntBuffer;"),
    ("java/nio/IntBuffer", "slice", "(II)Ljava/nio/IntBuffer;"),
    (
        "java/nio/LongBuffer",
        "asReadOnlyBuffer",
        "()Ljava/nio/LongBuffer;",
    ),
    ("java/nio/LongBuffer", "compact", "()Ljava/nio/LongBuffer;"),
    (
        "java/nio/LongBuffer",
        "duplicate",
        "()Ljava/nio/LongBuffer;",
    ),
    ("java/nio/LongBuffer", "get", "()J"),
    ("java/nio/LongBuffer", "get", "(I)J"),
    ("java/nio/LongBuffer", "isDirect", "()Z"),
    ("java/nio/LongBuffer", "isReadOnly", "()Z"),
    ("java/nio/LongBuffer", "order", "()Ljava/nio/ByteOrder;"),
    ("java/nio/LongBuffer", "put", "(IJ)Ljava/nio/LongBuffer;"),
    ("java/nio/LongBuffer", "put", "(J)Ljava/nio/LongBuffer;"),
    ("java/nio/LongBuffer", "slice", "()Ljava/nio/LongBuffer;"),
    ("java/nio/LongBuffer", "slice", "(II)Ljava/nio/LongBuffer;"),
    ("java/nio/MappedByteBuffer", "isDirect", "()Z"),
    ("java/nio/MappedByteBuffer", "isReadOnly", "()Z"),
    (
        "java/nio/ShortBuffer",
        "asReadOnlyBuffer",
        "()Ljava/nio/ShortBuffer;",
    ),
    (
        "java/nio/ShortBuffer",
        "compact",
        "()Ljava/nio/ShortBuffer;",
    ),
    (
        "java/nio/ShortBuffer",
        "duplicate",
        "()Ljava/nio/ShortBuffer;",
    ),
    ("java/nio/ShortBuffer", "get", "()S"),
    ("java/nio/ShortBuffer", "get", "(I)S"),
    ("java/nio/ShortBuffer", "isDirect", "()Z"),
    ("java/nio/ShortBuffer", "isReadOnly", "()Z"),
    ("java/nio/ShortBuffer", "order", "()Ljava/nio/ByteOrder;"),
    ("java/nio/ShortBuffer", "put", "(IS)Ljava/nio/ShortBuffer;"),
    ("java/nio/ShortBuffer", "put", "(S)Ljava/nio/ShortBuffer;"),
    ("java/nio/ShortBuffer", "slice", "()Ljava/nio/ShortBuffer;"),
    (
        "java/nio/ShortBuffer",
        "slice",
        "(II)Ljava/nio/ShortBuffer;",
    ),
];

/// SBR round 3 lane NIOBUF (2026-10-04): the direct-buffer accounting, the
/// buffer session pair and `ModuleReferenceImpl.open`. Record:
/// `docs/feature-designs/native-dispatch-architecture.md`; held row:
/// `docs/known-issues/jdk-only/SBR3-NIOBUF-*-HELD-20261004.md`
/// ([`RETIRED_SHADOW_SBR3_NIOBUF_HELD_TRIPLES`], unlisted).
///
/// The `Bits` rows retire together with the removal of the fabricated
/// `sun.nio.PageAlignDirectMemory=true` startup property
/// (`system_bootstrap.rs`): the real `DirectByteBuffer` constructor reserves
/// `capacity + pageSize` under it, which is the `+4096` the round-2 pages saw
/// on both arms. The real `Bits` counters are then HotSpot's.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR3_NIOBUF_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/nio/Bits", "reserveMemory", "(JJ)V"),
    ("java/nio/Bits", "unreserveMemory", "(JJ)V"),
    ("java/nio/Bits$1", "getCount", "()J"),
    ("java/nio/Bits$1", "getMemoryUsed", "()J"),
    ("java/nio/Bits$1", "getTotalCapacity", "()J"),
    ("java/nio/Buffer$2", "hasSession", "(Ljava/nio/Buffer;)Z"),
    (
        "java/nio/DirectByteBuffer",
        "session",
        "()Ljdk/internal/foreign/MemorySessionImpl;",
    ),
    ("java/nio/HeapByteBuffer", "checkSession", "()V"),
    (
        "java/nio/HeapByteBuffer",
        "session",
        "()Ljdk/internal/foreign/MemorySessionImpl;",
    ),
    // The real `ModuleFinder.of` now runs for a module-path module's reference, so
    // `ModuleInfo.read` reaches these two predicates; the descriptor one's native copy
    // accepted a preview minor (65535) for any major >= 56 where the JDK's body does
    // not; the class-file one was already the JDK's rule, retired with its neighbour
    // (it first showed as a new native-won triple in the `RJdkModule` census).
    (
        "jdk/internal/misc/VM",
        "isSupportedClassFileVersion",
        "(II)Z",
    ),
    (
        "jdk/internal/misc/VM",
        "isSupportedModuleDescriptorVersion",
        "(II)Z",
    ),
    (
        "jdk/internal/module/ModuleReferenceImpl",
        "open",
        "()Ljava/lang/module/ModuleReader;",
    ),
];

/// SBR round 3 lane NIOBUF: HELD, deliberately NOT in `RETIRED_SHADOW_TABLES`.
/// `Unsafe.compareAndExchangeByte`: the JDK body masks `offset & ~3` over what
/// it takes for a byte offset, this VM's `objectFieldOffset` answers a slot
/// index. Retired, `cas b1` answers `false` for `true` and `getAndSetByte`
/// never returns (`Sbr3NioBufUnsafe`, rc=124).
///
/// SBR3 wave 2, lane UNSAFECAS: still unretirable (the offsets are slot indices), and
/// no longer a `Bridge` to measure: `compareAndExchangeByte`, `compareAndExchangeShort`
/// and `compareAndSetShort` are reviewed `Intrinsic`s whose bodies are HotSpot-exact on
/// object fields, statics, raw memory and every array byte lane (`Sbr3UnsafecasCae`,
/// identical to HotSpot in both modes), which is the only way the row leaves the census.
#[allow(dead_code)]
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR3_NIOBUF_HELD_TRIPLES: &[(&str, &str, &str)] = &[(
    "jdk/internal/misc/Unsafe",
    "compareAndExchangeByte",
    "(Ljava/lang/Object;JBB)B",
)];

/// JIT round 17 wave 2, lane nio2 (2026-10-05): the rest of the buffer
/// `session()` / `checkSession()` family, seventeen rows over ten classes.
/// Page: `docs/feature-designs/native-dispatch-architecture.md`.
///
/// JDK 25 declares both methods ONCE, `final`, with `Code`, on `java/nio/Buffer`
/// (`segment != null ? segment.sessionImpl() : null`, and `session();
/// if (s != null) s.checkValidState()`). Every row below therefore stands over
/// real bytecode, and the superclass walk reached them for receivers SBR3 did
/// not cover: a `HeapIntBuffer` / `ByteBufferAsIntBufferB` / `DirectIntBufferS`
/// (and every typed sibling) found `IntBuffer.session` live and passed no
/// retired row, so `retired_row_masks_ancestor_bridge` had nothing to mask;
/// `DirectByteBuffer.checkSession` stayed live for every direct buffer.
///
/// # Why the JOTP refusal no longer holds
///
/// The JOTP wave measured `NoSuchMethodError` on `ArenaImpl.checkValidStateRaw`
/// when a segment-backed buffer's real `session()` ran over the VM's own arena
/// carrier. SBR3 lane NIOBUF re-measured exactly that on the class netty
/// reaches (`DirectByteBuffer.session`, `Sbr3NioBuf` segment / arena / channel
/// rows, `Sbr3NioBufNetty`): the arena carrier under `--real-jdk` is the JDK's
/// own `MemorySessionImpl`, and the real body is correct. A typed view of a
/// segment-backed buffer carries the SAME `segment` reference
/// (`ByteBufferAs*Buffer*` / `Direct*Buffer*` constructors pass it through), so
/// its `session()` is the same call on the same carrier. A buffer with no
/// segment (every `allocate` / `wrap`) answers `null` from the `getfield`, which
/// is what the shim answered.
///
/// The JIT is unaffected: compiled `session()` sites already read `segment`
/// themselves (`jit_buffer_session_direct`) and decline to the generic
/// dispatcher, which now reaches the bytecode, for a non-null segment.
///
/// `--compatible` is unchanged by this table (a `SyntheticStub` registers and
/// dispatches there as the `Bridge` did): the VM's synthetic FFM carriers live
/// in that mode, and for a JDK-built receiver the shim's answer equals the body
/// (null segment: `null`; real segment: its `scope` field, which is what
/// `AbstractMemorySegmentImpl.sessionImpl()` returns).
///
/// Control (one rule per class, or the whole family):
/// `CRATONVM_UNRETIRE_NATIVE_SHADOW=java/nio/IntBuffer.session,...`. Probe:
/// `C:\craton\jitr17-probes\src\R17Nio2Typed.java`. Written from reading, not
/// from a trial binary.
pub(in crate::retired_shadow) static RETIRED_SHADOW_R17_NIO2_SESSION_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/nio/Buffer", "checkSession", "()V"),
    (
        "java/nio/Buffer",
        "session",
        "()Ljdk/internal/foreign/MemorySessionImpl;",
    ),
    ("java/nio/ByteBuffer", "checkSession", "()V"),
    (
        "java/nio/ByteBuffer",
        "session",
        "()Ljdk/internal/foreign/MemorySessionImpl;",
    ),
    ("java/nio/DirectByteBuffer", "checkSession", "()V"),
    ("java/nio/DoubleBuffer", "checkSession", "()V"),
    (
        "java/nio/DoubleBuffer",
        "session",
        "()Ljdk/internal/foreign/MemorySessionImpl;",
    ),
    ("java/nio/FloatBuffer", "checkSession", "()V"),
    (
        "java/nio/FloatBuffer",
        "session",
        "()Ljdk/internal/foreign/MemorySessionImpl;",
    ),
    ("java/nio/IntBuffer", "checkSession", "()V"),
    (
        "java/nio/IntBuffer",
        "session",
        "()Ljdk/internal/foreign/MemorySessionImpl;",
    ),
    ("java/nio/LongBuffer", "checkSession", "()V"),
    (
        "java/nio/LongBuffer",
        "session",
        "()Ljdk/internal/foreign/MemorySessionImpl;",
    ),
    ("java/nio/MappedByteBuffer", "checkSession", "()V"),
    (
        "java/nio/MappedByteBuffer",
        "session",
        "()Ljdk/internal/foreign/MemorySessionImpl;",
    ),
    ("java/nio/ShortBuffer", "checkSession", "()V"),
    (
        "java/nio/ShortBuffer",
        "session",
        "()Ljdk/internal/foreign/MemorySessionImpl;",
    ),
];

/// SBR round 3, lane `ref` (2026-10-04): the `java.lang.ref` constructors and
/// `refersTo`, the four `StackFrameInfo` accessors, `Field.getRoot` and
/// `RecordComponent.getGenericType`.
/// The rows the lane TRIED and HELD are in [`RETIRED_SHADOW_SBR3_REF_HELD_TRIPLES`]
/// (unlisted); the record is
/// `docs/feature-designs/native-dispatch-architecture.md`,
/// the held rows' blockers are in
/// `docs/feature-designs/native-dispatch-architecture.md`.
///
/// The `java/lang/ref` constructors can retire because the reference
/// processor now hears of a `Reference` at its ALLOCATION under `--real-jdk`
/// (`vm/src/memory/ref_discovery.rs`, `RefDiscoveryState::from_flags(true)`),
/// not from the constructor bridge, and binds it from its own `referent` /
/// `queue` slots at the next collection. `Reference.<init>` (both overloads)
/// is on the table because the JDK's subclass constructors now reach it: its
/// native only stores the two slots the real body stores.
///
/// Measured (Linux host, `--real-jdk`, interleaved A/B on one binary,
/// `CRATONVM_UNRETIRE_NATIVE_SHADOW` for the control; ns/op, median of 5):
/// `new WeakReference` 785 vs 980, `new SoftReference` 874 vs 1592,
/// `new PhantomReference` 747 vs 981, `WeakReference.refersTo` 287 vs 208
/// (the one slower row: `refersTo0` stays native), `RecordComponent.getGenericType`
/// 0.3 us vs 2.0 us,
/// `Field.getRoot` 0.99 vs 0.98 us, `StackFrameInfo` accessors neutral
/// (`walk` native on both arms 12.9 vs 12.9 us).
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR3_REF_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/lang/StackFrameInfo",
        "getClassName",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/StackFrameInfo",
        "getDeclaringClass",
        "()Ljava/lang/Class;",
    ),
    (
        "java/lang/StackFrameInfo",
        "getMethodName",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/StackFrameInfo",
        "toStackTraceElement",
        "()Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/ref/PhantomReference",
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/ref/ReferenceQueue;)V",
    ),
    ("java/lang/ref/Reference", "<init>", "(Ljava/lang/Object;)V"),
    (
        "java/lang/ref/Reference",
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/ref/ReferenceQueue;)V",
    ),
    (
        "java/lang/ref/SoftReference",
        "<init>",
        "(Ljava/lang/Object;)V",
    ),
    (
        "java/lang/ref/SoftReference",
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/ref/ReferenceQueue;)V",
    ),
    (
        "java/lang/ref/WeakReference",
        "<init>",
        "(Ljava/lang/Object;)V",
    ),
    (
        "java/lang/ref/WeakReference",
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/ref/ReferenceQueue;)V",
    ),
    (
        "java/lang/ref/WeakReference",
        "refersTo",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/lang/reflect/Field",
        "getRoot",
        "()Ljava/lang/reflect/Field;",
    ),
    (
        "java/lang/reflect/RecordComponent",
        "getGenericType",
        "()Ljava/lang/reflect/Type;",
    ),
];

/// SBR round 3, lane `ref`: the rows TRIED and HELD, each with its measured
/// blocker (`docs/feature-designs/native-dispatch-architecture.md`).
/// **Not in `RETIRED_SHADOW_TABLES`**: they stay live `Bridge`s. Sorted so the
/// table can be spliced into the const unchanged; `#[allow(dead_code)]`
/// because nothing reads it by default.
///
/// SBR3 wave 2 (lane `refmisc`) RE-EARNED `Class.getPackage`,
/// `ClassLoader.loadClass` x2 and `StackTraceElement.computeFormat` out of this
/// list ([`RETIRED_SHADOW_SBR3_REFMISC_TRIPLES`]); their bullets below are the
/// round-3 record of why they were held. The rows that remain have a
/// wave-2 measurement in
/// `docs/known-issues/jdk-only/SBR3-REFMISC-*-HELD-20261005.md`.
///
/// * `Class.getPackage`: the real body fails `RLangPackages`
///   (`java.lang` `isSealed()` is `false`: the VM leaves `Class.classLoader`
///   null for built-in loaders, so `BootLoader.definePackage` builds the
///   package on an unnamed module) and answers `null` for a lambda class.
/// * `Class.getTypeName`: correct, array form 4.9 us against 0.9 us (5.6x).
/// * `ClassLoader.loadClass` x2 (held in `RETIRED_SHADOW_SBR2_MODLOAD_HELD_TRIPLES`,
///   not repeated here): re-measured, the real body is 4.4 us against 19 us and
///   matches HotSpot on `Sbr3RefClass`, but a class-path jar whose manifest says
///   `Sealed: true` dies on its first class (`sealing violation`), the whole of
///   `Sbr2W9ModloadPkg` with it.
/// * `StackTraceElement.computeFormat`: the Linux `java.base` carries no
///   `ModuleHashes` attribute (a jlinked image keeps them in the generated
///   `SystemModules`), so the real body prints `java.base@25.0.4/...`; and
///   `getStackTrace` is 1.4x slower.
/// * `StackWalker.walk`: correct, 59 us against 12.9 us (4.6x) for a five-frame
///   walk.
/// * `System.exit`: correct (stdout, shutdown hook and status equal HotSpot's,
///   `Sbr3RefSys exit`), but the real body is `Runtime.getRuntime().exit(..)`,
///   whose `Runtime.exit` native (held, `the_sbr_fjp2_wave...`) then shows up
///   native-won in the census: one row for another on the same path. It would
///   also drop the Surefire `ForkedBooter.exit(1)` soft-return that
///   `native_system_exit` carries.
/// * `System.clearProperty` / `getProperties` / `setProperty`: no probe or
///   vector fails retired (`SystemRuntimeObjectSweep`, `Sbr3RefSys*`), but the
///   VM's property store stays a second copy: a Rust reader of a property
///   never sees a later write, and `getProperties` is 59 ns retired against
///   325 us native (the native harvests the whole map each call).
/// * `SoftReference.get` / `WeakReference.get` (and `Reference.get`) were held
///   here for the SATB keep-alive a bytecode `getfield referent` lacked; SBR4
///   lane `satb` carried the keep-alive into the bytecode read and retired them
///   ([`RETIRED_SHADOW_SBR4_SATB_TRIPLES`]).
#[allow(dead_code)]
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR3_REF_HELD_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/Class", "getTypeName", "()Ljava/lang/String;"),
    (
        "java/lang/StackWalker",
        "walk",
        "(Ljava/util/function/Function;)Ljava/lang/Object;",
    ),
];

/// SBR round 3 wave 2, lane `refmisc` (2026-10-05): what lane REF held and a
/// VM fix has since made safe. Sorted (binary-searched). The record is
/// `docs/feature-designs/native-dispatch-architecture.md`, the rows still
/// held are in `docs/known-issues/jdk-only/SBR3-REFMISC-*-HELD-20261005.md`
/// and [`RETIRED_SHADOW_SBR3_REF_HELD_TRIPLES`].
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR3_REFMISC_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/Class", "getPackage", "()Ljava/lang/Package;"),
    (
        "java/lang/ClassLoader",
        "loadClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
    ),
    (
        "java/lang/ClassLoader",
        "loadClass",
        "(Ljava/lang/String;Z)Ljava/lang/Class;",
    ),
    (
        "java/lang/StackFrameInfo",
        "declaringClass",
        "()Ljava/lang/Class;",
    ),
    ("java/lang/StackTraceElement", "computeFormat", "()V"),
];

/// SBR round 4, lane `lang` (2026-10-05): see
/// `docs/feature-designs/native-dispatch-architecture.md`.
/// Sorted (binary-searched). EMPTY: the six rows SBR3 held
/// ([`RETIRED_SHADOW_SBR3_REF_HELD_TRIPLES`]) were each retired in a trial build and
/// A/B-measured against `CRATONVM_UNRETIRE_NATIVE_SHADOW` on one binary, and every one
/// stays native on a NEWLY MEASURED blocker, recorded in
/// `docs/feature-designs/native-dispatch-architecture.md`:
///
/// * `System.setProperty`/`clearProperty`/`getProperties`: retired is FASTER (1.0 vs 5.1 us,
///   3.1 vs 9.4 us, 0.15 vs 357 us) but a Rust reader of the property store misses the write:
///   `Sbr4LangPropStore` row `InetAddress.getLoopbackAddress` after a run-time
///   `java.net.preferIPv4Stack=true` answers the IPv6 loopback retired, HotSpot's IPv4 native.
/// * `StackWalker.walk`: retired alone now throws `NullPointerException` out of
///   `StackFrameInfo.declaringClass` (`memberName` null), because lane REFMISC retired that
///   row; the two are coupled. Cost as before (`Constructor.newInstance` 2.3 us).
/// * `Class.getTypeName`: array form 5.5 vs 0.9 us (6.3x), plain 1.13 vs 0.97 us.
/// * `System.exit`: needs `Runtime.exit` first (held by SBR W5 FJP2).
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR4_LANG_TRIPLES: &[(&str, &str, &str)] = &[];

/// SBR round 4, lane `satb` (2026-10-05): `Reference.get` and the two
/// subclass rows lane REF held for the SATB keep-alive. Sorted (binary-searched).
///
/// `Reference.get()` is a HotSpot intrinsic that loads the referent through an
/// SATB keep-alive. Real bytecode is `aload_0; getfield referent; areturn`, and
/// the VM's concurrent marker (G1, concurrent ZGC, the generational cycle) hides
/// the referent slot, so the keep-alive now lives where the bytecode reads the
/// field: `op_getfield` applies `heap.satb_barrier` to a `Reference.referent`
/// load (`memory::ref_discovery::is_referent_field`), and the doors that would
/// answer the read WITHOUT it decline it (the quickened `getfield` site fill,
/// the cached trivial-getter door, the JIT's field-site resolvers, which refuse
/// a `getfield` of the field so compiled code never loads it unguarded).
/// `PhantomReference.get` (returns `null`) stays a registered native: the
/// bytecode is `return null`, so there is nothing to keep alive and no row to
/// win. Record: `docs/feature-designs/jit-barrier-lowering.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR4_SATB_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/ref/Reference", "get", "()Ljava/lang/Object;"),
    ("java/lang/ref/SoftReference", "get", "()Ljava/lang/Object;"),
    ("java/lang/ref/WeakReference", "get", "()Ljava/lang/Object;"),
];

/// SBR5 lane jfr: the JFR boundary. Every row below was a native standing in
/// front of the JDK's own `jdk.jfr` bytecode (the Rust recorder fed
/// `Event.commit`, `Recording.start/stop/dump` and the `RecordingStream`
/// consumers directly; `JDKEvents.initialize` was a no-op). The real VM half
/// now exists (`native-builtins/src/jfr_backend.rs`: event writer, chunk writer,
/// retransformation behind the instrumentation upcall), so the Java classes run
/// as written, in both modes, and the registrations are gone. Listed so the
/// strict-mode census and `triple_is_retired_shadow` keep naming them.
/// `AnnotationElement.checkType` stood in the same way.
///
/// `jdk/jfr/internal/Type.getKnownType` and `util/Utils.getValidType` retired
/// in SBR3 (`RETIRED_SHADOW_SBR3_AWT_TRIPLES`); they lost their `--compatible`
/// registrations here.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR5_JFR_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "jdk/jfr/AnnotationElement",
        "checkType",
        "(Ljava/lang/Class;)V",
    ),
    ("jdk/jfr/Event", "begin", "()V"),
    ("jdk/jfr/Event", "commit", "()V"),
    ("jdk/jfr/Event", "end", "()V"),
    ("jdk/jfr/Event", "isEnabled", "()Z"),
    ("jdk/jfr/Event", "shouldCommit", "()Z"),
    ("jdk/jfr/Recording", "dump", "(Ljava/nio/file/Path;)V"),
    ("jdk/jfr/Recording", "start", "()V"),
    ("jdk/jfr/Recording", "stop", "()Z"),
    ("jdk/jfr/consumer/RecordingStream", "close", "()V"),
    (
        "jdk/jfr/consumer/RecordingStream",
        "onEvent",
        "(Ljava/lang/String;Ljava/util/function/Consumer;)V",
    ),
    (
        "jdk/jfr/consumer/RecordingStream",
        "onEvent",
        "(Ljava/util/function/Consumer;)V",
    ),
    ("jdk/jfr/consumer/RecordingStream", "start", "()V"),
    ("jdk/jfr/consumer/RecordingStream", "startAsync", "()V"),
    ("jdk/jfr/internal/JDKEvents", "initialize", "()V"),
    ("jdk/jfr/internal/instrument/JDKEvents", "initialize", "()V"),
];

/// SBR round 5, lane `sysprops` (2026-10-06): `System.setProperty` / `clearProperty` /
/// `getProperties` run the JDK's own bytecode (they write the real `System.props`).
/// Sorted (binary-searched). SBR4 lane `lang` measured them retired as FASTER (1.0 vs 5.1 us,
/// 3.1 vs 9.4 us, 0.15 us vs 357 us) and held them on one correctness defect: the ~130 Rust
/// readers of the VM's property store never saw a run-time write. Every reader now goes
/// through `NativeContext::get_system_property` / `list_system_properties`, which under
/// `--real-jdk` read the Java map through `native-builtins/src/sysprops_merge.rs` (merged with
/// the store, latest writer wins). Record:
/// `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR5_SYSPROPS_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/Runtime", "exit", "(I)V"),
    ("java/lang/Shutdown", "runHooks", "()V"),
    (
        "java/lang/System",
        "clearProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
    ),
    ("java/lang/System", "exit", "(I)V"),
    (
        "java/lang/System",
        "getProperties",
        "()Ljava/util/Properties;",
    ),
    (
        "java/lang/System",
        "setProperty",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
    ),
];

/// SBR round 5, lane `walker` (2026-10-06): the `ClassFrameInfo` / `StackFrameInfo` frame
/// accessors. Sorted (binary-searched). Record:
/// `docs/feature-designs/native-dispatch-architecture.md`.
///
/// `StackWalker.walk` / `forEach` / `getCallerClass` are NOT here: retired they run the JDK's
/// `StackStreamFactory` pipeline at 3.5x / 5x / 15x the native's CPU time (measured, see
/// `docs/feature-designs/native-dispatch-architecture.md`). `Class.getTypeName`
/// is a reviewed `Intrinsic` (`native_class_get_type_name_exact`), not a retired row.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR5_WALKER_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/lang/ClassFrameInfo",
        "declaringClass",
        "()Ljava/lang/Class;",
    ),
    (
        "java/lang/ClassFrameInfo",
        "ensureRetainClassRefEnabled",
        "()V",
    ),
    (
        "java/lang/ClassFrameInfo",
        "getClassName",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/ClassFrameInfo",
        "getDeclaringClass",
        "()Ljava/lang/Class;",
    ),
    (
        "java/lang/ClassFrameInfo",
        "getMethodName",
        "()Ljava/lang/String;",
    ),
    ("java/lang/StackFrameInfo", "getByteCodeIndex", "()I"),
    (
        "java/lang/StackFrameInfo",
        "getDescriptor",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/StackFrameInfo",
        "getFileName",
        "()Ljava/lang/String;",
    ),
    ("java/lang/StackFrameInfo", "getLineNumber", "()I"),
    (
        "java/lang/StackFrameInfo",
        "getMethodType",
        "()Ljava/lang/invoke/MethodType;",
    ),
    ("java/lang/StackFrameInfo", "isNativeMethod", "()Z"),
];

/// 2026-10-06, SBR round 5, lane AVH: `MemoryLayout.arrayElementVarHandle(PathElement...)`
/// (the 26 receiver classes the `Bridge` `p67_memory_layout_array_element_var_handle` was
/// registered on), sorted, LISTED in `RETIRED_SHADOW_TABLES`. The JDK's body is
/// `MethodHandles.collectCoordinates(varHandle(elements), 1, scaleHandle())`, an
/// `IndirectVarHandle` over the layout's `SegmentVarHandle`; SBR4 held the rows because
/// that road cost 11.5x (44.9 us vs 3.85 us per get+set pair: `vh_indirect_dispatch`
/// went through the JDK's `invokeWithArguments`, 100+ us). Lane AVH serves a real composed
/// mode handle at its own type from its spun entry (no `asType`, no `Object[]`) and gives
/// `invokeWithArguments` on a real handle its own door; the measurement is in
/// `docs/feature-designs/native-dispatch-architecture.md`.
/// Behaviour the bridge got wrong and the real body gets right (`WrongMethodTypeException`
/// on a wrong coordinate, the alignment message, `toMethodHandle`) is listed in the page.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR5_AVH_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/lang/foreign/AddressLayout",
        "arrayElementVarHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
    ),
    (
        "java/lang/foreign/GroupLayout",
        "arrayElementVarHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
    ),
    (
        "java/lang/foreign/MemoryLayout",
        "arrayElementVarHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
    ),
    (
        "java/lang/foreign/SequenceLayout",
        "arrayElementVarHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
    ),
    (
        "java/lang/foreign/StructLayout",
        "arrayElementVarHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
    ),
    (
        "java/lang/foreign/UnionLayout",
        "arrayElementVarHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
    ),
    (
        "java/lang/foreign/ValueLayout$OfBoolean",
        "arrayElementVarHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
    ),
    (
        "java/lang/foreign/ValueLayout$OfByte",
        "arrayElementVarHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
    ),
    (
        "java/lang/foreign/ValueLayout$OfChar",
        "arrayElementVarHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
    ),
    (
        "java/lang/foreign/ValueLayout$OfDouble",
        "arrayElementVarHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
    ),
    (
        "java/lang/foreign/ValueLayout$OfFloat",
        "arrayElementVarHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
    ),
    (
        "java/lang/foreign/ValueLayout$OfInt",
        "arrayElementVarHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
    ),
    (
        "java/lang/foreign/ValueLayout$OfLong",
        "arrayElementVarHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
    ),
    (
        "java/lang/foreign/ValueLayout$OfShort",
        "arrayElementVarHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
    ),
    (
        "jdk/internal/foreign/layout/SequenceLayoutImpl",
        "arrayElementVarHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
    ),
    (
        "jdk/internal/foreign/layout/StructLayoutImpl",
        "arrayElementVarHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
    ),
    (
        "jdk/internal/foreign/layout/UnionLayoutImpl",
        "arrayElementVarHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "arrayElementVarHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "arrayElementVarHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "arrayElementVarHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "arrayElementVarHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "arrayElementVarHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "arrayElementVarHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "arrayElementVarHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "arrayElementVarHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "arrayElementVarHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
    ),
];

/// Quarkus suite (2026-10-09): the Agroal JDBC pool shim (`native-builtins/src/agroal_pool.rs`).
/// A native `AgroalDataSourceConfigurationSupplier.get()` answered an instance of the INTERFACE
/// `AgroalDataSourceConfiguration` (`alloc_object_for` on the interface's class id, its six
/// slots holding the shim's own url/driver/user/password/min/max), so the first
/// `config.connectionPoolConfiguration()` the real `io.quarkus.agroal.runtime.DataSources` made
/// was an `invokeinterface` on an object of the interface itself: `AbstractMethodError:
/// ... connectionPoolConfiguration() has no Code attribute`, in all 23 agroal extension tests
/// that create a datasource. The shim's registrar already documented the collision and its
/// escape (`CRATONVM_REAL_AGROAL`, `real_agroal()`); with the escape on, the same tests pass over
/// the real `io.agroal.pool.*` bytecode and the real `org.h2.Driver`. Rows of the registrar
/// (`register_agroal_natives`, explicitly `Bridge`, which is what hid them from the shadow
/// census). `--compatible` keeps the shim. Sorted (binary-searched). Record:
/// `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_QUARKUS_AGROAL_TRIPLES: &[(&str, &str, &str)] = &[
    ("io/agroal/api/AgroalDataSource", "<init>", "(Lio/agroal/api/configuration/AgroalDataSourceConfiguration;)V"),
    ("io/agroal/api/AgroalDataSource", "close", "()V"),
    ("io/agroal/api/AgroalDataSource", "from", "(Lio/agroal/api/configuration/AgroalDataSourceConfiguration;)Lio/agroal/api/AgroalDataSource;"),
    ("io/agroal/api/AgroalDataSource", "getConnection", "()Ljava/sql/Connection;"),
    ("io/agroal/api/AgroalPropertiesReader", "readProperties", "(Ljava/lang/String;)Lio/agroal/api/configuration/AgroalDataSourceConfiguration;"),
    ("io/agroal/api/configuration/AgroalDataSourceConfigurationSupplier$Builder", "build", "()Lio/agroal/api/configuration/AgroalDataSourceConfiguration;"),
    ("io/agroal/api/configuration/supplier/AgroalDataSourceConfigurationSupplier", "get", "()Lio/agroal/api/configuration/AgroalDataSourceConfiguration;"),
    ("io/agroal/pool/ConnectionPool", "<init>", "(Lio/agroal/api/configuration/AgroalDataSourceConfiguration;)V"),
    ("io/agroal/pool/PoolHandler", "getConnection", "()Ljava/sql/Connection;"),
];

/// Quarkus suite (2026-10-10, wave 1): the `LocaleConverter.convert` native
/// (`essential_quarkus_locale_convert`, `native-builtins/src/lib.rs`). It was written when
/// `Locale.forLanguageTag` did not populate the language in a synthetic JDK, and answered a
/// fresh `new Locale(lang, country, variant)` split on `-`/`_`: `"all"` came back as a NEW empty
/// locale instead of `Locale.ROOT` (`assertSame(Locale.ROOT, ..)` fails) and an ill-formed tag
/// such as `c.u-US` as `c.u_US` where the real converter validates through
/// `Locale.forLanguageTag` and answers `Locale.ROOT`. With the real `forLanguageTag` the bytecode
/// is correct (HotSpot passes `LocaleConverterTest`). `--compatible` keeps the native.
/// Sorted (binary-searched).
pub(in crate::retired_shadow) static RETIRED_SHADOW_QUARKUS_CONFIG_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "io/quarkus/runtime/configuration/LocaleConverter",
        "convert",
        "(Ljava/lang/String;)Ljava/lang/Object;",
    ),
    (
        "io/quarkus/runtime/configuration/LocaleConverter",
        "convert",
        "(Ljava/lang/String;)Ljava/util/Locale;",
    ),
];

/// Quarkus suite (2026-10-09): the jboss-logging backends and the jboss-logmanager
/// classes that `register_logmanager_natives` (`native-builtins/src/logmanager.rs`) and the
/// `ExtHandler` / `QuarkusDelayedHandler` shims (`native-builtins/src/lib.rs`) stood in front
/// of. They were written for a synthetic JDK in which `java.util.logging.LogManager` was
/// itself fabricated; under `--real-jdk` the JUL natives are refused and the real
/// `LogManager` instantiates the real `org.jboss.logmanager.LogManager`, so these natives then
/// ran over a REAL manager with a fake `LogContext` singleton (one field, `rootLogger` null),
/// and `jboss-logging`'s `LoggerProviders.tryJBossLogManager` failed with a
/// `NullPointerException` in `LogContext.getAttachment`, fell through to the JDK provider, and
/// `JDKLogger.doLog`'s native then printed `LEVEL [name] message` to stderr instead of calling
/// the JUL handlers. Every log-record assertion in the Quarkus suite
/// (`ArcContainerTest`, `AeshSshNoAuthWarningTest`, ...) saw zero records.
/// `--compatible` keeps the natives (a `SyntheticStub` registers and dispatches there).
/// Sorted (binary-searched). Record:
/// `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_QUARKUS_LOG_TRIPLES: &[(&str, &str, &str)] = &[
    ("io/quarkus/bootstrap/logging/QuarkusDelayedHandler", "addHandler", "(Ljava/util/logging/Handler;)V"),
    ("io/quarkus/bootstrap/logging/QuarkusDelayedHandler", "close", "()V"),
    ("io/quarkus/bootstrap/logging/QuarkusDelayedHandler", "flush", "()V"),
    ("io/quarkus/bootstrap/logging/QuarkusDelayedHandler", "getFormatter", "()Ljava/util/logging/Formatter;"),
    ("io/quarkus/bootstrap/logging/QuarkusDelayedHandler", "getLevel", "()Ljava/util/logging/Level;"),
    ("io/quarkus/bootstrap/logging/QuarkusDelayedHandler", "isLoggable", "(Ljava/util/logging/LogRecord;)Z"),
    ("io/quarkus/bootstrap/logging/QuarkusDelayedHandler", "publish", "(Ljava/util/logging/LogRecord;)V"),
    ("io/quarkus/bootstrap/logging/QuarkusDelayedHandler", "setFormatter", "(Ljava/util/logging/Formatter;)V"),
    ("io/quarkus/bootstrap/logging/QuarkusDelayedHandler", "setHandlers", "([Ljava/util/logging/Handler;)V"),
    ("io/quarkus/bootstrap/logging/QuarkusDelayedHandler", "setLevel", "(Ljava/util/logging/Level;)V"),
    ("org/jboss/logging/JBossLogManagerLogger", "doLog", "(Lorg/jboss/logging/Logger$Level;Ljava/lang/String;Ljava/lang/Object;[Ljava/lang/Object;Ljava/lang/Throwable;)V"),
    ("org/jboss/logging/JBossLogManagerLogger", "doLogf", "(Lorg/jboss/logging/Logger$Level;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/Object;Ljava/lang/Throwable;)V"),
    ("org/jboss/logging/JDKLogger", "doLog", "(Lorg/jboss/logging/Logger$Level;Ljava/lang/String;Ljava/lang/Object;[Ljava/lang/Object;Ljava/lang/Throwable;)V"),
    ("org/jboss/logging/JDKLogger", "doLogf", "(Lorg/jboss/logging/Logger$Level;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/Object;Ljava/lang/Throwable;)V"),
    ("org/jboss/logging/Log4j2Logger", "doLog", "(Lorg/jboss/logging/Logger$Level;Ljava/lang/String;Ljava/lang/Object;[Ljava/lang/Object;Ljava/lang/Throwable;)V"),
    ("org/jboss/logging/Log4j2Logger", "doLogf", "(Lorg/jboss/logging/Logger$Level;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/Object;Ljava/lang/Throwable;)V"),
    ("org/jboss/logging/Log4jLogger", "doLog", "(Lorg/jboss/logging/Logger$Level;Ljava/lang/String;Ljava/lang/Object;[Ljava/lang/Object;Ljava/lang/Throwable;)V"),
    ("org/jboss/logging/Log4jLogger", "doLogf", "(Lorg/jboss/logging/Logger$Level;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/Object;Ljava/lang/Throwable;)V"),
    ("org/jboss/logging/Logger", "getMessageLogger", "(Ljava/lang/Class;Ljava/lang/String;)Ljava/lang/Object;"),
    ("org/jboss/logging/Logger", "getMessageLogger", "(Ljava/lang/Class;Ljava/lang/String;Ljava/util/Locale;)Ljava/lang/Object;"),
    ("org/jboss/logging/Logger", "getMessageLogger", "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/Class;Ljava/lang/String;)Ljava/lang/Object;"),
    ("org/jboss/logging/Logger", "getMessageLogger", "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/Class;Ljava/lang/String;Ljava/util/Locale;)Ljava/lang/Object;"),
    ("org/jboss/logging/LoggerProviders", "findProvider", "()Lorg/jboss/logging/LoggerProvider;"),
    ("org/jboss/logging/Slf4jLogger", "doLog", "(Lorg/jboss/logging/Logger$Level;Ljava/lang/String;Ljava/lang/Object;[Ljava/lang/Object;Ljava/lang/Throwable;)V"),
    ("org/jboss/logging/Slf4jLogger", "doLogf", "(Lorg/jboss/logging/Logger$Level;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/Object;Ljava/lang/Throwable;)V"),
    ("org/jboss/logmanager/ExtHandler", "addHandler", "(Ljava/util/logging/Handler;)V"),
    ("org/jboss/logmanager/ExtHandler", "close", "()V"),
    ("org/jboss/logmanager/ExtHandler", "flush", "()V"),
    ("org/jboss/logmanager/ExtHandler", "getFormatter", "()Ljava/util/logging/Formatter;"),
    ("org/jboss/logmanager/ExtHandler", "getLevel", "()Ljava/util/logging/Level;"),
    ("org/jboss/logmanager/ExtHandler", "isLoggable", "(Ljava/util/logging/LogRecord;)Z"),
    ("org/jboss/logmanager/ExtHandler", "publish", "(Ljava/util/logging/LogRecord;)V"),
    ("org/jboss/logmanager/ExtHandler", "setFormatter", "(Ljava/util/logging/Formatter;)V"),
    ("org/jboss/logmanager/ExtHandler", "setHandlers", "([Ljava/util/logging/Handler;)[Ljava/util/logging/Handler;"),
    ("org/jboss/logmanager/ExtHandler", "setLevel", "(Ljava/util/logging/Level;)V"),
    ("org/jboss/logmanager/LogContext", "addCloseHandler", "(Ljava/lang/AutoCloseable;)V"),
    ("org/jboss/logmanager/LogContext", "checkAccess", "(Lorg/jboss/logmanager/LogContext;)V"),
    ("org/jboss/logmanager/LogContext", "checkSecurityAccess", "()V"),
    ("org/jboss/logmanager/LogContext", "close", "()V"),
    ("org/jboss/logmanager/LogContext", "getCloseHandlers", "()Ljava/util/Set;"),
    ("org/jboss/logmanager/LogContext", "getLevelForName", "(Ljava/lang/String;)Ljava/util/logging/Level;"),
    ("org/jboss/logmanager/LogContext", "getLogContext", "()Lorg/jboss/logmanager/LogContext;"),
    ("org/jboss/logmanager/LogContext", "getLogger", "(Ljava/lang/String;)Lorg/jboss/logmanager/Logger;"),
    ("org/jboss/logmanager/LogContext", "getLoggerIfExists", "(Ljava/lang/String;)Lorg/jboss/logmanager/Logger;"),
    ("org/jboss/logmanager/LogContext", "getSystemLogContext", "()Lorg/jboss/logmanager/LogContext;"),
    ("org/jboss/logmanager/LogContext", "setCloseHandlers", "(Ljava/util/Collection;)V"),
    ("org/jboss/logmanager/LogManager", "addLogger", "(Ljava/util/logging/Logger;)Z"),
    ("org/jboss/logmanager/LogManager", "checkAccess", "()V"),
    ("org/jboss/logmanager/LogManager", "getLogManager", "()Ljava/util/logging/LogManager;"),
    ("org/jboss/logmanager/LogManager", "getLogger", "(Ljava/lang/String;)Ljava/util/logging/Logger;"),
    ("org/jboss/logmanager/LogManager", "getLogger", "(Ljava/lang/String;)Lorg/jboss/logmanager/Logger;"),
    ("org/jboss/logmanager/LogManager", "getLoggerNames", "()Ljava/util/Enumeration;"),
    ("org/jboss/logmanager/LogManager", "getProperty", "(Ljava/lang/String;)Ljava/lang/String;"),
    ("org/jboss/logmanager/LogManager", "readConfiguration", "()V"),
    ("org/jboss/logmanager/LogManager", "readConfiguration", "(Ljava/io/InputStream;)V"),
    ("org/jboss/logmanager/LogManager", "reset", "()V"),
    ("org/jboss/logmanager/Logger", "addHandler", "(Ljava/util/logging/Handler;)V"),
    ("org/jboss/logmanager/Logger", "attach", "(Lorg/jboss/logmanager/Logger$AttachmentKey;Ljava/lang/Object;)Ljava/lang/Object;"),
    ("org/jboss/logmanager/Logger", "attachIfAbsent", "(Lorg/jboss/logmanager/Logger$AttachmentKey;Ljava/lang/Object;)Ljava/lang/Object;"),
    ("org/jboss/logmanager/Logger", "detach", "(Lorg/jboss/logmanager/Logger$AttachmentKey;)Ljava/lang/Object;"),
    ("org/jboss/logmanager/Logger", "fine", "(Ljava/lang/String;)V"),
    ("org/jboss/logmanager/Logger", "getAttachment", "(Lorg/jboss/logmanager/Logger$AttachmentKey;)Ljava/lang/Object;"),
    ("org/jboss/logmanager/Logger", "getEffectiveLevel", "()I"),
    ("org/jboss/logmanager/Logger", "getHandlers", "()[Ljava/util/logging/Handler;"),
    ("org/jboss/logmanager/Logger", "getLevel", "()Ljava/util/logging/Level;"),
    ("org/jboss/logmanager/Logger", "getLogContext", "()Lorg/jboss/logmanager/LogContext;"),
    ("org/jboss/logmanager/Logger", "getName", "()Ljava/lang/String;"),
    ("org/jboss/logmanager/Logger", "getParent", "()Lorg/jboss/logmanager/Logger;"),
    ("org/jboss/logmanager/Logger", "getUseParentFilters", "()Z"),
    ("org/jboss/logmanager/Logger", "getUseParentHandlers", "()Z"),
    ("org/jboss/logmanager/Logger", "info", "(Ljava/lang/String;)V"),
    ("org/jboss/logmanager/Logger", "isLoggable", "(Ljava/util/logging/Level;)Z"),
    ("org/jboss/logmanager/Logger", "log", "(Ljava/util/logging/Level;Ljava/lang/String;)V"),
    ("org/jboss/logmanager/Logger", "log", "(Ljava/util/logging/Level;Ljava/util/function/Supplier;)V"),
    ("org/jboss/logmanager/Logger", "log", "(Ljava/util/logging/LogRecord;)V"),
    ("org/jboss/logmanager/Logger", "logRaw", "(Ljava/util/logging/LogRecord;)V"),
    ("org/jboss/logmanager/Logger", "logRaw", "(Lorg/jboss/logmanager/ExtLogRecord;)V"),
    ("org/jboss/logmanager/Logger", "removeHandler", "(Ljava/util/logging/Handler;)V"),
    ("org/jboss/logmanager/Logger", "setHandlers", "([Ljava/util/logging/Handler;)V"),
    ("org/jboss/logmanager/Logger", "setLevel", "(Ljava/util/logging/Level;)V"),
    ("org/jboss/logmanager/Logger", "setUseParentFilters", "(Z)V"),
    ("org/jboss/logmanager/Logger", "setUseParentHandlers", "(Z)V"),
    ("org/jboss/logmanager/Logger", "severe", "(Ljava/lang/String;)V"),
    ("org/jboss/logmanager/Logger", "warning", "(Ljava/lang/String;)V"),
];

pub(crate) const RETIRED_SHADOW_TABLES: &[&[(&str, &str, &str)]] = &[
    RETIRED_SHADOW_TRIPLES,
    RETIRED_SHADOW_STATELESS_TRIPLES,
    RETIRED_SHADOW_PHASE2_TRIPLES,
    RETIRED_SHADOW_L2_TRIPLES,
    RETIRED_SHADOW_PHASE3_TRIPLES,
    RETIRED_SHADOW_L5_TRIPLES,
    // Added here by the 2026-09-11 merge of lane 1's wave 2, NOT by lane 1:
    // this const and lane 1's table were written on branches that never saw
    // each other, so `git merge` resolved both files without a conflict and
    // left the new 325-row table off the list. That is the exact drift
    // `the_tables_const_lists_every_table_the_predicate_consults` exists to
    // catch, and it is what caught it.
    RETIRED_SHADOW_L1_TRIPLES,
    // And again, one merge later, for exactly the same reason: lane T's table
    // and this const were also written on branches that never saw each other,
    // so the 2026-09-11 merge resolved both files and left 906 more rows off
    // the list. Two lanes in two days is the shape of the defect rather than
    // an accident of one -- a const that enumerates tables drifts from the
    // predicate that consults them every time a lane lands, and only this
    // guard closes the gap.
    RETIRED_SHADOW_LT_TRIPLES,
    // And again on 2026-09-11, one merge later, for lanes L0 and L3 -- same
    // mechanism, same silence. This const arrived on `dev` while L0's and L3's
    // tables were being written on this branch, so neither file conflicted and
    // the predicate chain above ended up consulting NINE tables against this
    // const's SEVEN. The count assertion is the only thing that noticed.
    //
    // The cost was not cosmetic: this const is what feeds
    // `real_layout_bridge_keeps_are_not_retired_shadows`, so until this line
    // L0's 19 and L3's 24 were never asked whether their `Bridge` ->
    // `SyntheticStub` re-tag disarms a `keep_real_*_bridge` arm in REAL-JDK
    // mode -- the question that took lane 5 from 100 rows to 98.
    RETIRED_SHADOW_L0_TRIPLES,
    RETIRED_SHADOW_L3_TRIPLES,
    // And here by the 2026-09-11 merge of lane 7, for exactly the reason the
    // note above gives -- same shape, second occurrence in two days. Lane 7's
    // table and this const were also written on branches that never saw each
    // other, `git merge` resolved both files, and the omission was again
    // invisible to every per-table test.
    RETIRED_SHADOW_L7_TRIPLES,
    RETIRED_SHADOW_L4_TRIPLES,
    // Lane 1's HashMap (21 triples) and jar/text (29 triples over
    // java/text/Normalizer and java/util/jar/*) waves, added by the 2026-09-11
    // L7 merge and NOT by lane 1 -- the third and fourth occurrence of this
    // exact drift in two days, after lane 1's first table and lane 7's.
    //
    // They arrived as two more `||` arms in the predicate with no entry here,
    // which under the old chain meant "consulted, but invisible to every gate
    // driven off this const". Under the loop the predicate reads this list, so
    // the same omission would have UN-RETIRED both waves instead of merely
    // under-covering them -- a louder failure, and the reason the loop is worth
    // having: the two lists cannot disagree, because there is only one.
    // Lane 6, added under the loop rather than beside a `||` arm -- which is
    // the whole difference the loop makes. This lane hit the old failure mode
    // twice while the chain still existed: once on its own table, once on
    // lane 1's waves 3 and 4, which were consulted by the chain and missing
    // from this const on `dev`. Under the loop neither omission is possible,
    // because forgetting the const row does not under-cover a table -- it
    // un-retires it, loudly, in the lane's own probe run.
    RETIRED_SHADOW_L6_TRIPLES,
    RETIRED_SHADOW_L1_HM_TRIPLES,
    RETIRED_SHADOW_L1_JT_TRIPLES,
    // Lane 5's residual wave, added here in the same 2026-09-11 merge. The
    // note above is why this line exists at all: the loop below reads THIS
    // list, so a table missing from it is a table the predicate does not
    // consult.
    RETIRED_SHADOW_L5R_TRIPLES,
    // 2026-09-11, L1 wave 5. Added HERE and nowhere else, which under the loop
    // above is the whole registration -- there is no chain arm to forget any
    // more. See `RETIRED_SHADOW_L1_ZI_TRIPLES` for why two rows and not forty,
    // and why the table is written but not yet accepted.
    RETIRED_SHADOW_L1_ZI_TRIPLES,
    // Lane 5's SECOND residual wave, 2026-09-11. Same one-line registration as
    // the line above it, and the same consequence for forgetting it: the loop
    // below reads THIS list, so a table that is not named here is a table
    // nothing consults and 33 rows that read as un-retired.
    RETIRED_SHADOW_L5S_TRIPLES,
    // Lane 5's THIRD residual wave, 2026-09-12 -- the off-heap half of
    // `jdk/internal/misc/Unsafe` plus the two `sun/misc/Unsafe` rows that
    // needed a 21 run to be dispatchable at all. Same one-line registration as
    // its two predecessors, and the same consequence for forgetting it: the
    // predicate reads THIS list, so a table absent from it is 27 rows that
    // read as un-retired everywhere except the sorted-and-unique test.
    RETIRED_SHADOW_L5T_TRIPLES,
    // 2026-09-11, L1 wave 6: `java/text/BreakIterator`, all seventeen, with
    // the BREAKITER pin in `vm_exec.rs` removed in the same commit. Neither
    // half is correct alone -- retire without lifting the pin and the pin
    // still serves the factories; lift without retiring and the census still
    // counts seventeen shadows over a family that no longer uses them.
    RETIRED_SHADOW_L1_BI_TRIPLES,
    RETIRED_SHADOW_L1_LP_TRIPLES,
    // 2026-09-12, L1 wave 7. `Hashtable` (79) and the eight `HashMap` rows
    // `LinkedHashMap` was blocking. What made them takeable is a DECODE fix
    // in `native-collections` -- `hs_backing_map` / `hs_view_elements`
    // resolving a foreign view's source map through `this$0` -- and not a
    // change of store: the store was already real in both families.
    //
    // `LinkedHashMap`'s own 102 were MEASURED AND REFUSED in the same wave, on
    // two trial binaries, and so were the eight `java/util/HashMap` rows it
    // inherits -- on a THIRD. See
    // `wave_seven_refused_the_linked_hash_map_iterator_half`.
    RETIRED_SHADOW_L1_HT_TRIPLES,
    // 2026-09-12, L1 wave 8. The whole `TreeMap`/`TreeSet` family, 156 rows.
    // Takeable for the same reason wave 7's were and one more: the state was
    // already real (`root`, `size`, `comparator` all match HotSpot on an
    // unarmed control), and this wave added the READ half of the `root`
    // mirror -- `tm_pairs_from_real_root` -- so a native that still has to
    // decode a TreeMap can do it from the node graph real bytecode maintains.
    RETIRED_SHADOW_L1_TM_TRIPLES,
    // L1 wave 9. Listed in the same commit as the tables, which is the whole
    // of what the five notes above are about: every one of them is a lane that
    // wrote a table on a branch this const did not see.
    RETIRED_SHADOW_L1_LHM_TRIPLES,
    RETIRED_SHADOW_L1_DATE_TRIPLES,
    // Lane 4 wave 2, added WITH the table rather than after a gate caught it --
    // which is the whole point of the loop this const now feeds.
    RETIRED_SHADOW_L4_FFM_TRIPLES,
    // Lane 4 wave 3, the group half of the same prefix. No new prefix:
    // `jdk/internal/foreign/layout/` was admitted by wave 2 and its note
    // there says what this line is the other half of.
    RETIRED_SHADOW_L4_FFM_GROUP_TRIPLES,
    // Lane 4 wave 4. No new prefix either: `java/nio/` was admitted by wave 1,
    // which retires this class's `java/nio/ByteBuffer` twin under it.
    RETIRED_SHADOW_L4_CHARBUFFER_TRIPLES,
    // Lane 4 wave 5. No new prefix either: `java/io/` and `java/nio/` were
    // both admitted by wave 1, and every class in this table is under one of
    // them. The table is the whole of the decision.
    RETIRED_SHADOW_L4_BACKED_OUT_TRIPLES,
    // Lane 4 wave 6, in TWO tables for one registrar. `java/io/` was admitted
    // as a prefix by wave 1, so neither needs a new one. They are separate
    // because their recorded verdicts were opposite and because
    // `CRATONVM_UNRETIRE_NATIVE_SHADOW` disarms a TABLE: one build can then be
    // scored with either half off, which is the only way to attribute a
    // corpus failure to a half without a second build.
    RETIRED_SHADOW_L4_PRINTWRITER_TRIPLES,
    RETIRED_SHADOW_L4_PRINTSTREAM_TRIPLES,
    // Lane 4 wave 7, the Path half of the Path/FileSystemProvider retirement
    // wave. No new prefix: `java/nio/` was admitted by wave 1.
    RETIRED_SHADOW_L4_PATH_TRIPLES,
    // Lane 4 wave 9, 2026-09-17. No new prefix: `java/nio/` was admitted by
    // wave 1 and already covers `java/nio/file/spi/FileSystemProvider`. Four
    // of the nine rows the carrier fix, the `Path.fs`-field fix and the
    // `FileSystem` carrier fix together unblocked; see the table's own doc
    // comment for why the other five stayed out.
    RETIRED_SHADOW_L4_FILESYSTEMPROVIDER_TRIPLES,
    // Lane 4 wave 8, 2026-09-18. Same merge-drift risk the notes above
    // describe, so landed with this const in the same commit.
    RETIRED_SHADOW_L4_DEPRECATED_STREAMS_TRIPLES,
    RETIRED_SHADOW_L4_FILEVISITOR_TRIPLES,
    RETIRED_SHADOW_L4_CHARBUFFER_VIEWS_TRIPLES,
    RETIRED_SHADOW_L4_PATHS_TRIPLES,
    // Lane 4 wave 10/13, 2026-09-18. 16 of the 19 original candidates,
    // landed after three real bugs found and fixed across three attempts;
    // getFileName/getParent/toRealPath excluded (see the table's own doc:
    // the first two do a direct '\\'-scan no field pre-population can fix
    // on this VM's '/'-stored path, and toRealPath reaches a Win32 native
    // -- GetFullPathName0 -- this VM has not implemented yet).
    RETIRED_SHADOW_L4_WINDOWSPATH_TRIPLES,
    // 2026-09-19, the `--real-jdk` throughput wave. Registered here in the same
    // commit as the table, for the reason every note above gives.
    RETIRED_SHADOW_JOTP_TRIPLES,
    // Part two of the same wave. A separate table so the two halves can be disarmed
    // independently by `CRATONVM_UNRETIRE_NATIVE_SHADOW`.
    RETIRED_SHADOW_JOTP_UNALIGNED_TRIPLES,
    // Lane 4 wave 16, 2026-09-19. The Windows `sun/nio/fs` filesystem classes;
    // landed with this const in the same commit for the merge-drift reason the
    // notes above give.
    RETIRED_SHADOW_L4_WINFS_TRIPLES,
    // Lane 4 wave 17, 2026-09-19. The eleven `WinNTFileSystem` wrappers.
    RETIRED_SHADOW_L4_WINNTFS_TRIPLES,
    // Wave 18's Linux twin of the line above, measured on linux/x86_64.
    RETIRED_SHADOW_L4_UNIXFS_TRIPLES,
    // Wave 19's 63 rows, the ledger's remainder.
    RETIRED_SHADOW_L4_W19_TRIPLES,
    RETIRED_SHADOW_L4_FILETMP_TRIPLES,
    RETIRED_SHADOW_L4_FILES2_TRIPLES,
    RETIRED_SHADOW_L4_BUFFERS2_TRIPLES,
    // 2026-09-21, closing `H14-3` N2. Landed with this const in the same
    // commit, per every note above.
    RETIRED_SHADOW_STRINGBUILDER_TRIPLES,
    // 2026-09-23, the H2 census's descriptor leak. One row; landed with this
    // const in the same commit, per every note above.
    RETIRED_SHADOW_FD_ACCESS_CLOSE_TRIPLES,
    // 2026-09-23, the same census's shared `Util$BufferCache`. Two tables, one
    // decision -- see the second one's doc comment.
    RETIRED_SHADOW_JLA_CARRIER_TL_TRIPLES,
    RETIRED_SHADOW_UTIL_TEMP_BUFFER_TRIPLES,
    // 2026-09-23, the real `jar:` / `jrt:` providers: `installedProviders` and
    // the `newFileSystem(Path, ...)` doors. Five rows, see the table.
    RETIRED_SHADOW_ARCHIVE_PROVIDER_TRIPLES,
    // 2026-09-24, `java/util/jar/JarFile` whole. Landed with this const in the
    // same commit, per every note above.
    RETIRED_SHADOW_JARFILE_TRIPLES,
    // 2026-09-24, `java/net/URLClassLoader` and `jdk/internal/loader/URLClassPath`
    // whole. 26 rows, see the table.
    RETIRED_SHADOW_URLCLASSLOADER_TRIPLES,
    // 2026-09-24, `java/net/URL`'s `(String)` constructor and seven accessors.
    RETIRED_SHADOW_URL_ACCESSOR_TRIPLES,
    // 2026-09-26, `LinkedHashMap`'s three `sequenced*` view accessors. Landed
    // with this const in the same commit, per every note above.
    RETIRED_SHADOW_LHM_SEQUENCED_VIEW_TRIPLES,
    // 2026-09-28, round 13 wave 7: the `HashSet` family, in two tables so the
    // first keeps its "declared on all three images" claim and the JDK 21
    // `SequencedSet` half is separate. Landed with this const, per every note
    // above.
    RETIRED_SHADOW_L1_HS_TRIPLES,
    RETIRED_SHADOW_LHS_SEQUENCED_TRIPLES,
    // 2026-09-28, round 13 wave 8 (lane shadow2): the `java.beans`
    // change-support family, 25 rows over three classes. Landed with this
    // const and its three class prefixes, per every note above.
    RETIRED_SHADOW_BEANS_CHANGE_SUPPORT_TRIPLES,
    // 2026-09-28, round 13 wave 12 (lane shadow4): the throwable family's
    // `printStackTrace(PrintWriter)` / `setStackTrace` pair, 123 rows lane T
    // retired and pulled back. No new prefix: lane T's class names and the
    // `java/` package prefixes already admit all 62 classes.
    RETIRED_SHADOW_THROWABLE_TRACE_PAIR_TRIPLES,
    // 2026-09-28, round 13 wave 13 (lane shadow5): `StringBuilder`'s five
    // inherited rows and `VirtualMachineError`'s message pair. Two tables so
    // `CRATONVM_UNRETIRE_NATIVE_SHADOW` can disarm either alone. No new
    // prefix: `java/lang/` admits both classes.
    RETIRED_SHADOW_SB_INHERITED_FIVE_TRIPLES,
    RETIRED_SHADOW_VME_MESSAGE_PAIR_TRIPLES,
    // 2026-09-29, round 14 wave 2 (lane shadow): `OutputStream.write([B)V`.
    // No new prefix: `java/io/` admits it.
    RETIRED_SHADOW_OUTPUT_STREAM_WRITE_ALL_TRIPLES,
    // 2026-09-29, round 14 wave 5 (lane compat5): the `java.sql` date/time
    // family, 11 rows. Landed with this const and its three class prefixes.
    RETIRED_SHADOW_SQL_DATETIME_TRIPLES,
    // 2026-09-29, round 14 wave 6 (lane compat6): `Array.newInstance`, both
    // overloads. No new prefix: `java/lang/` admits it.
    RETIRED_SHADOW_REFLECT_ARRAY_NEW_INSTANCE_TRIPLES,
    // 2026-09-30, round 15 wave 1 (lane shadow): three CA6-1 thin-wrapper
    // families, one table each so `CRATONVM_UNRETIRE_NATIVE_SHADOW` can
    // disarm any one alone. `java/lang/` and `sun/nio/ch/` admit the first
    // two; the third landed with its two class prefixes.
    RETIRED_SHADOW_SYSTEM_STREAM_SETTER_TRIPLES,
    RETIRED_SHADOW_NIO_COVARIANT_BRIDGE_TRIPLES,
    RETIRED_SHADOW_REFLECT_GENERICS_DELEGATE_TRIPLES,
    // 2026-09-28, SBR wave 1 lane LOCALE: `Locale`, `TimeZone` and
    // `ResourceBundle$Control.getCandidateLocales`. No new prefix: `java/util/`
    // admits all three. Landed with this const, per every note above.
    RETIRED_SHADOW_SBR_LOCALE_TRIPLES,
    // 2026-09-28, SBR wave 1 lane JCA: six JCA classes, 30 rows. Landed with
    // this const, per every note above.
    RETIRED_SHADOW_SBR_JCA_TRIPLES,
    // 2026-09-28, SBR wave 1 lane `uri`: `java/net/URI` whole, 30 rows. No new
    // prefix: `java/net/` was admitted by lane L6.
    RETIRED_SHADOW_SBR_URI_TRIPLES,
    // 2026-09-28, SBR wave 1 lane JMX: `javax/management/ObjectName` whole.
    // Landed with this const in the same commit, per every note above.
    RETIRED_SHADOW_SBR_JMX_TRIPLES,
    // 2026-09-28, SBR wave 1 lane `bytes`: `SimpleEntry`, the five leftover
    // `StringBuilder` rows, `Bits$1.getName` and four `Buffer$2` rows. 15
    // rows, see the table (`ByteArray` became a reviewed `Intrinsic` instead).
    RETIRED_SHADOW_SBR_BYTES_TRIPLES,
    // 2026-09-28, SBR wave 2 lane NIOCH: ONE row, `SocketDispatcher.close`;
    // the rest of the socket/channel/selector family is held. No new prefix:
    // `sun/nio/ch/` was admitted by Phase 2. Landed with this const, per every
    // note above.
    RETIRED_SHADOW_SBR_NIOCH_TRIPLES,
    // 2026-09-29, SBR wave 3 lane REFLECT: the generic carriers, three
    // `Field` rows, six `Method`/`Constructor` rows and one
    // `ReflectionFactory` row; `Field`'s value accessors (cost), the copy
    // door, annotations and `Proxy` are held. Two narrow prefixes admit the
    // non-`java/lang/` classes.
    RETIRED_SHADOW_SBR_REFLECT_TRIPLES,
    // 2026-09-29, SBR wave 2 lane JCA2: the provider side of the JCA and the
    // engines it unblocked. Landed with this const, per every note above.
    RETIRED_SHADOW_SBR_JCA2_TRIPLES,
    // 2026-09-28, SBR wave 2 lane `fs`: the Linux `sun/nio/fs` family
    // (`UnixPath`, `UnixFileAttributes`, `LinuxFileSystemProvider`, the two
    // directory streams). No new prefix: `sun/nio/fs/` was admitted by H2-1.
    RETIRED_SHADOW_SBR_FS_TRIPLES,
    // 2026-09-28, SBR wave 2 lane `netaddr`: the `InetAddress` family (39 rows)
    // and `java/net/URL` (10 rows), two tables so `CRATONVM_UNRETIRE_NATIVE_SHADOW`
    // reads as two decisions. No new prefix: `java/net/` was admitted by lane L6.
    RETIRED_SHADOW_SBR_NETADDR_TRIPLES,
    RETIRED_SHADOW_SBR_NETADDR_URL_TRIPLES,
    // 2026-09-29, SBR wave 4 lane FJP: `ForkJoinPool` and its task classes,
    // `StackWalker`, `Runtime`/`Runtime$Version`, `jdk/internal/misc/Signal`.
    RETIRED_SHADOW_SBR_FJP_TRIPLES,
    // 2026-09-29, SBR wave 4 lane LOCALE2: `ResourceBundle`, `Bundles.of` and
    // the six `LocaleResources` readers. Two narrow prefixes admit the
    // `sun/util/` classes. Landed with this const, per every note above.
    RETIRED_SHADOW_SBR_LOCALE2_TRIPLES,
    // 2026-09-29, SBR wave 4 lane SSL: `X500Principal`/`X500Name`,
    // `SSLEngineResult`, `KeyStore.getInstance(String, Provider)`; the JSSE
    // handshake half is held. Landed with this const, per every note above.
    RETIRED_SHADOW_SBR_SSL_TRIPLES,
    // 2026-09-29, SBR wave 5 lane NIOREAL: the socket/channel/selector/datagram
    // family wave 2 held, over the real `sun.nio.ch` native layer. No new
    // prefix: `java/net/`, `java/nio/` and `sun/nio/ch/` are all admitted.
    RETIRED_SHADOW_SBR_NIOREAL_TRIPLES,
    // 2026-09-29, SBR wave 5 lane CRYPTOENG: `MessageDigest`, unblocked by
    // reviewed `Intrinsic`s on the SUN `@IntrinsicCandidate` kernels
    // (`crypto_intrinsics.rs`); `Mac`/`Cipher` held for handshake setup cost.
    RETIRED_SHADOW_SBR_CRYPTOENG_TRIPLES,
    // 2026-09-29, SBR wave 5 lane FS2: the `sun/nio/fs` rows wave 2 held --
    // the Linux `WatchService` pipeline, `DefaultFileSystemProvider`, the
    // `FileStore` family. `sun/nio/fs/` was admitted by lane 4.
    RETIRED_SHADOW_SBR_FS2_TRIPLES,
    // 2026-09-29, SBR wave 5 lane JSSE: the JSSE handshake half with its
    // managers, factory SPIs and keystore SPIs, retired as one unit.
    RETIRED_SHADOW_SBR_JSSE_TRIPLES,
    // 2026-09-29, SBR wave 5 lane FJP2: the rows wave 4 FJP held.
    RETIRED_SHADOW_SBR_FJP2_TRIPLES,
    // 2026-09-29, SBR wave 5 lane HTTP: the plain-HTTP `HttpURLConnection`
    // family, `URLConnection` and the `jrt:` connection. The
    // `com.sun.net.httpserver` server and the `java.net.http` client are HELD
    // (see the table's doc).
    RETIRED_SHADOW_SBR_HTTP_TRIPLES,
    RETIRED_SHADOW_SBR_REFLMETA_TRIPLES,
    // 2026-09-29, SBR wave 6 lane HTTPSTACK: the HTTP server, client, HTTPS
    // carrier and default proxy selector wave 5 held.
    RETIRED_SHADOW_SBR_HTTPSTACK_TRIPLES,
    RETIRED_SHADOW_SBR_ASYNCNET_TRIPLES,
    // 2026-09-29, SBR wave 6 lane MHLOOKUP.
    RETIRED_SHADOW_SBR_MHLOOKUP_TRIPLES,
    RETIRED_SHADOW_SBR_CLASSREFL_TRIPLES,
    // 2026-09-29, SBR wave 6 lane THREAD: the `java/lang/Thread` accessor,
    // interrupt and lifecycle rows (`java/lang/Thread` is admitted by prefix).
    RETIRED_SHADOW_SBR_THREAD_TRIPLES,
    // 2026-09-29, SBR wave 6 lane TAIL: Proxy, Unsafe wrappers, KeyAgreement and
    // the small tail. Landed with this const, per every note above.
    RETIRED_SHADOW_SBR_TAIL_TRIPLES,
    RETIRED_SHADOW_SBR_MODULE_TRIPLES,
    // 2026-09-29, SBR wave 6 lane SYSTEM: `System`, the `JavaLangAccess` carrier
    // `System$1`, the `SharedSecrets` getters, `VM`, `DefaultLoggerFinder`, and
    // `BootLoader.loadLibrary` (the row `System.load*` cannot retire without).
    // `java/lang/` and `jdk/internal/misc/` were admitted long ago; three
    // narrow class prefixes admit the rest.
    RETIRED_SHADOW_SBR_SYSTEM_TRIPLES,
    // 2026-09-29, SBR wave 6 lane DISPATCH: `javax/crypto/Cipher`, whose native
    // answers 13 probe rows wrong. `Mac` stays held.
    RETIRED_SHADOW_SBR_DISPATCH_TRIPLES,
    // 2026-09-30, SBR round 2 wave 7 lane TAIL.
    RETIRED_SHADOW_SBR2_TAIL_TRIPLES,
    // 2026-09-30, SBR round 2 wave 8 lane SMALL.
    RETIRED_SHADOW_SBR2_SMALL_TRIPLES,
    // 2026-09-30, SBR round 2 wave 7 lane CALLER: `Field` value accessors and
    // `javax/crypto/Mac` (native answers probe rows wrong; cost named above).
    RETIRED_SHADOW_SBR2_CALLER_TRIPLES,
    // 2026-09-30, SBR round 2 wave 7 lane MH: the four carrier-surface rows
    // whose real bytecode runs correctly, gainfully or free (see the table's doc);
    // the held rows are in `RETIRED_SHADOW_SBR2_MH_HELD_TRIPLES`, unlisted.
    RETIRED_SHADOW_SBR2_MH_TRIPLES,
    // 2026-09-30, SBR round 2 wave 7 lane AWT: the concrete `sun/management`
    // impl rows. `sun/management/` is admitted by a new prefix above.
    RETIRED_SHADOW_SBR2_AWT_TRIPLES,
    // 2026-10-04, SBR round 3 lane awt.
    RETIRED_SHADOW_SBR3_AWT_TRIPLES,
    // 2026-09-30, SBR round 2 wave 7 lane THREAD: the nine `Thread.<init>`
    // overloads, `isAlive` and `threadState()`. `java/lang/` admits them.
    RETIRED_SHADOW_SBR2_THREAD_TRIPLES,
    // 2026-09-30, SBR round 2 wave 7 lane CLASSREFL.
    RETIRED_SHADOW_SBR2_CLASSREFL_TRIPLES,
    // 2026-09-30, SBR round 2 wave 7 lane MODULE.
    RETIRED_SHADOW_SBR2_MODULE_TRIPLES,
    // 2026-09-30, SBR round 2 wave 8 lane FFM.
    RETIRED_SHADOW_SBR2_FFM_TRIPLES,
    // 2026-09-30, SBR round 2 wave 8 lane SPRINGAPP (trial).
    RETIRED_SHADOW_SBR2_SPRINGAPP_TRIPLES,
    RETIRED_SHADOW_SBR2_SPRINGAPP_APP_TRIPLES,
    // 2026-09-30, SBR round 2 wave 8 lane CTORPLAN.
    RETIRED_SHADOW_SBR2_CTORPLAN_TRIPLES,
    RETIRED_SHADOW_SBR2_LOADERS_TRIPLES,
    // 2026-10-01, SBR round 2 wave 9 lane MHFIX.
    RETIRED_SHADOW_SBR2_MHFIX_TRIPLES,
    // 2026-10-04, SBR round 3 lane mh.
    RETIRED_SHADOW_SBR3_MH_TRIPLES,
    // 2026-10-05, SBR round 3 wave 2 lane mhperf: the MethodHandle family as a unit.
    RETIRED_SHADOW_SBR4_MHCONS_TRIPLES,
    // 2026-10-01, SBR round 2 wave 9 lane MODLOAD.
    RETIRED_SHADOW_SBR2_MODLOAD_TRIPLES,
    // 2026-10-01, SBR round 2 wave 9 lane SMALL2.
    RETIRED_SHADOW_SBR2_SMALL2_TRIPLES,
    // 2026-10-02, round 16 wave 15 (lane shadow): the JCEKS / DKS keystore
    // engine surfaces and `javax.crypto.KEM`, one table per family so
    // `CRATONVM_UNRETIRE_NATIVE_SHADOW` can disarm either alone. Landed with
    // their three class prefixes.
    RETIRED_SHADOW_R16_KEYSTORE_SPI_TRIPLES,
    RETIRED_SHADOW_R16_KEM_TRIPLES,
    // 2026-10-02, round 16 wave 16 (lane nio2): `MappedByteBuffer.force()` and
    // `load()`, reached for every real mapped buffer through the superclass
    // walk. `java/nio/` was admitted long ago.
    RETIRED_SHADOW_R16_NIO2_MAPPED_TRIPLES,
    // 2026-10-07, JIT round 19 (nio19): the VM's fabricated `Pipe`; `java/nio/`
    // and `sun/nio/ch/` were admitted long ago.
    RETIRED_SHADOW_R19_NIO_PIPE_TRIPLES,
    // 2026-10-07, JIT round 19 (nio19): the abstract-declaration buffer rows.
    RETIRED_SHADOW_R19_NIO_ABSTRACT_BUFFER_TRIPLES,
    // 2026-10-04, Spring Boot residual retirement: `Class.getAnnotatedInterfaces`
    // and `Class.getAnnotatedSuperclass`. `java/lang/` admits both.
    RETIRED_SHADOW_SB_CLASS_ANNOTATED_TRIPLES,
    // 2026-10-04, SBR round 3 lane NIOBUF.
    RETIRED_SHADOW_SBR3_NIOBUF_TRIPLES,
    // 2026-10-04, SBR round 3 lane REF.
    RETIRED_SHADOW_SBR3_REF_TRIPLES,
    // 2026-10-04, SBR round 3 lane FFM: the segment / arena / session /
    // splitter / layout-VarHandle / layout-factory rows round 2 held.
    RETIRED_SHADOW_SBR3_FFM_TRIPLES,
    // 2026-10-05, JIT round 17 wave 2 (lane nio2): the rest of the buffer
    // `session()` / `checkSession()` family (typed buffers, `Buffer` itself).
    RETIRED_SHADOW_R17_NIO2_SESSION_TRIPLES,
    // 2026-10-05, SBR round 3 wave 2 lane REFMISC.
    RETIRED_SHADOW_SBR3_REFMISC_TRIPLES,
    // 2026-10-05, SBR round 3 wave 2, lane UNSAFECAS: `PlatformGraphicsInfo.createToolkit`.
    RETIRED_SHADOW_SBR3_UNSAFECAS_TRIPLES,
    // 2026-10-05, SBR round 4 lane LANG.
    RETIRED_SHADOW_SBR4_LANG_TRIPLES,
    // 2026-10-05, SBR round 4, lane SATB: `Reference.get` and its two subclass rows.
    RETIRED_SHADOW_SBR4_SATB_TRIPLES,
    RETIRED_SHADOW_SBR4_SEGACC_TRIPLES,
    RETIRED_SHADOW_SBR5_JFR_TRIPLES,
    // 2026-10-06, SBR round 5 lane SYSPROPS.
    RETIRED_SHADOW_SBR5_SYSPROPS_TRIPLES,
    // 2026-10-06, SBR round 5, lane WALKER.
    RETIRED_SHADOW_SBR5_WALKER_TRIPLES,
    // 2026-10-06, SBR round 5, lane AVH: `arrayElementVarHandle` x26.
    RETIRED_SHADOW_SBR5_AVH_TRIPLES,
    // 2026-10-06, SBR round 5, lane FFMLINK.
    RETIRED_SHADOW_SBR5_FFMLINK_TRIPLES,
    // 2026-10-06, SBR round 5 lane AWTJFR: Java2D / ImageIO / fonts.
    RETIRED_SHADOW_SBR5_AWTJFR_TRIPLES,
    // 2026-10-07, SBR round 5, lane FIX3.
    RETIRED_SHADOW_SBR5_FIX3_TRIPLES,
    // 2026-10-09, Quarkus suite: the jboss-logging / jboss-logmanager shadows.
    RETIRED_SHADOW_QUARKUS_LOG_TRIPLES,
    // 2026-10-09, Quarkus suite: the Agroal pool shim.
    RETIRED_SHADOW_QUARKUS_AGROAL_TRIPLES,
    // 2026-10-10, Quarkus suite wave 1: the LocaleConverter native.
    RETIRED_SHADOW_QUARKUS_CONFIG_TRIPLES,
];

