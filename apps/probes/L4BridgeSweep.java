import java.nio.*;
import java.nio.file.*;
import java.nio.file.attribute.*;
import java.util.*;

/** L4 — every covariant bridge descriptor in `java.nio`, driven the only way
 *  that reaches it.
 *
 *  `L4TailSweep2` found ONE defect of this shape: `ByteBuffer.reset()` through
 *  a `Buffer`-typed reference raised `IllegalStateException` where the real
 *  bytecode raises `InvalidMarkException`. The registry says that was not the
 *  only one of its kind — after that probe ran, these slots were STILL at zero
 *  invocations:
 *
 *      java/nio/ByteBuffer    flip   ()Ljava/nio/Buffer;   inv=0
 *      java/nio/ByteBuffer    mark   ()Ljava/nio/Buffer;   inv=0
 *      java/nio/ByteBuffer    rewind ()Ljava/nio/Buffer;   inv=0
 *      java/nio/DoubleBuffer  clear  ()Ljava/nio/Buffer;   inv=0
 *      java/nio/DoubleBuffer  flip   ()Ljava/nio/Buffer;   inv=0
 *      java/nio/FloatBuffer   clear  ()Ljava/nio/Buffer;   inv=0
 *      java/nio/FloatBuffer   flip   ()Ljava/nio/Buffer;   inv=0
 *      java/nio/IntBuffer     flip   ()Ljava/nio/Buffer;   inv=0
 *      java/nio/LongBuffer    clear  ()Ljava/nio/Buffer;   inv=0
 *      java/nio/LongBuffer    flip   ()Ljava/nio/Buffer;   inv=0
 *      java/nio/ShortBuffer   clear  ()Ljava/nio/Buffer;   inv=0
 *      java/nio/ShortBuffer   flip   ()Ljava/nio/Buffer;   inv=0
 *
 *  `L4TypedBufferSweep` is 501 rows over exactly these classes and reaches none
 *  of them, because every reference in it is typed as the concrete buffer. The
 *  descriptor, not the method, is what goes unasked.
 *
 *  `bridges()` therefore takes a `Buffer`. That single declaration is the whole
 *  instrument: javac emits `()Ljava/nio/Buffer;` only from here.
 *
 *  Second population, same idea one level up: methods whose ONLY registered
 *  spelling returns a base type, so ordinary code reaches them and nothing in
 *  this lane ever has — `BasicFileAttributes.fileKey()`, `FileStore
 *  .getAttribute(String)`, `FileAttribute.value()`, all `-> Object`, all at
 *  inv=0.
 *
 *  Deliberately NOT here: the channel and selector families
 *  (`SocketChannel.bind`, `configureBlocking`, `SelectionKey.attach`, the
 *  `getOption` fleet). They carry the same descriptor shape and the same zero,
 *  but they are network-shaped and unclaimed by this lane -- see the record's
 *  §P2.5. Adding them here would be this lane answering another one's rows.
 */
public class L4BridgeSweep {
    static int rows = 0;

    interface ThrowingRun { void run() throws Throwable; }

    static void p(String tag, Object v) {
        rows++;
        System.out.println(tag + " |" + v + "|");
    }

    interface ThrowingGet { Object get() throws Throwable; }

    /** A VALUE row that survives a refusal.
     *
     *  `p(tag, expr)` evaluates its argument before the call, so an unexpected
     *  throw inside `expr` ends the whole run and every row below it silently
     *  becomes "not measured". That is exactly what happened on this probe's
     *  first pass: `FileStore.getAttribute("totalSpace")` raised
     *  `UnsupportedOperationException` on this VM, and the last 14 rows -- a
     *  different family entirely -- vanished with it. The line count beside the
     *  diff is what caught it (477 vs 491).
     *
     *  Use this wherever the ANSWER is the interesting part but a refusal is a
     *  possible outcome; `t()` remains for rows where the refusal IS the point.
     */
    static void pt(String tag, ThrowingGet g) {
        rows++;
        try { System.out.println(tag + " |" + g.get() + "|"); }
        catch (Throwable e) { System.out.println(tag + " |THREW " + e.getClass().getName() + "|"); }
    }

    static void t(String tag, ThrowingRun r) {
        rows++;
        try { r.run(); System.out.println(tag + " |no-throw|"); }
        catch (Throwable e) { System.out.println(tag + " |THREW " + e.getClass().getName() + "|"); }
    }

    /** The state of a buffer as one comparable string. */
    static String st(Buffer b) {
        return "pos=" + b.position() + " lim=" + b.limit() + " cap=" + b.capacity()
             + " rem=" + b.remaining() + " hasRem=" + b.hasRemaining();
    }

    // ---------------------------------------------------------------- bridges
    //
    // EVERY call below goes through the `Buffer`-typed parameter. Retyping this
    // parameter to the concrete buffer class silently converts the whole method
    // into a test of a different set of registrations -- which is exactly what
    // every earlier probe in this lane was.
    static void bridges(String k, Buffer b) {
        // Identity: each of these is specified to return `this`, and a bridge
        // that allocates a fresh view instead would still print the right
        // numbers on every row below.
        p(k + " clear returns this",    b.clear()   == b);
        p(k + " after clear",           st(b));
        p(k + " position(1) this",      b.position(1) == b);
        p(k + " after position(1)",     st(b));
        p(k + " limit(cap-1) this",     b.limit(b.capacity() - 1) == b);
        p(k + " after limit(cap-1)",    st(b));
        p(k + " mark returns this",     b.mark()    == b);
        p(k + " position(2) this",      b.position(2) == b);
        p(k + " reset returns this",    b.reset()   == b);
        p(k + " after reset",           st(b));
        p(k + " flip returns this",     b.flip()    == b);
        p(k + " after flip",            st(b));
        p(k + " rewind returns this",   b.rewind()  == b);
        p(k + " after rewind",          st(b));

        // The mark is DISCARDED by clear/flip/rewind, so reset after any of
        // them is the InvalidMarkException row -- the one that found the
        // original defect. Asked once per buffer class because the bridge is
        // registered once per buffer class.
        b.clear();
        t(k + " reset after clear",  () -> b.reset());
        b.mark(); b.flip();
        t(k + " reset after flip",   () -> b.reset());
        b.clear(); b.mark(); b.rewind();
        t(k + " reset after rewind", () -> b.reset());

        // limit() below the mark drops it too.
        b.clear(); b.position(3); b.mark(); b.limit(2);
        t(k + " reset after limit under mark", () -> b.reset());

        // Argument validation, through the same bridge descriptors.
        b.clear();
        t(k + " position(-1)",        () -> b.position(-1));
        t(k + " position(cap+1)",     () -> b.position(b.capacity() + 1));
        t(k + " limit(-1)",           () -> b.limit(-1));
        t(k + " limit(cap+1)",        () -> b.limit(b.capacity() + 1));
        p(k + " state after refusals", st(b));

        // `Buffer`'s own non-covariant accessors, for a control: if these
        // disagree too, the bridge is not the story.
        p(k + " isDirect",   b.isDirect());
        p(k + " isReadOnly", b.isReadOnly());
        p(k + " hasArray",   b.hasArray());
    }

    static void allBuffers() {
        // Heap, direct, and a VIEW over a direct buffer: the three backends
        // this VM stores differently. A bridge that is correct on the heap
        // arm and wrong on the direct one reads as fixed if only one is asked.
        ByteBuffer heap = ByteBuffer.allocate(8);
        ByteBuffer direct = ByteBuffer.allocateDirect(8);
        bridges("ByteBuffer heap", heap);
        bridges("ByteBuffer direct", direct);
        bridges("ByteBuffer ro", ByteBuffer.allocate(8).asReadOnlyBuffer());

        bridges("CharBuffer heap", CharBuffer.allocate(8));
        bridges("ShortBuffer heap", ShortBuffer.allocate(8));
        bridges("IntBuffer heap", IntBuffer.allocate(8));
        bridges("LongBuffer heap", LongBuffer.allocate(8));
        bridges("FloatBuffer heap", FloatBuffer.allocate(8));
        bridges("DoubleBuffer heap", DoubleBuffer.allocate(8));

        // The view buffers reach a different registrar again.
        ByteBuffer v = ByteBuffer.allocateDirect(64);
        bridges("ShortBuffer view", v.asShortBuffer());
        bridges("IntBuffer view", v.asIntBuffer());
        bridges("LongBuffer view", v.asLongBuffer());
        bridges("FloatBuffer view", v.asFloatBuffer());
        bridges("DoubleBuffer view", v.asDoubleBuffer());
        bridges("CharBuffer view", v.asCharBuffer());

        // A wrapped array and a slice: same classes, different construction
        // path, and `hasArray`/`isDirect` should say so.
        bridges("ByteBuffer wrap", ByteBuffer.wrap(new byte[8]));
        ByteBuffer sl = ByteBuffer.allocate(16);
        sl.position(4); sl.limit(12);
        bridges("ByteBuffer slice", sl.slice());
        bridges("CharBuffer wrap CS", CharBuffer.wrap("abcdefgh"));
    }

    // --------------------------------------------------- the `-> Object` rows
    static void objectReturns() throws Exception {
        Path dir = Files.createTempDirectory("l4bridge");
        Path f = dir.resolve("a.txt");
        Files.writeString(f, "hello");
        Path g = dir.resolve("b.txt");
        Files.writeString(g, "hello");

        // fileKey(): the VALUE is an implementation object whose class name and
        // toString differ between any two VMs, so neither is a differential
        // row. Its CONTRACT is: equal for two reads of one file, unequal for
        // two different files, null when the provider cannot supply one. That
        // is comparable, and it is what a caller actually relies on -- fileKey
        // is how you detect that two Paths name the same file.
        BasicFileAttributes a1 = Files.readAttributes(f, BasicFileAttributes.class);
        BasicFileAttributes a2 = Files.readAttributes(f, BasicFileAttributes.class);
        BasicFileAttributes b1 = Files.readAttributes(g, BasicFileAttributes.class);
        p("fileKey non-null", a1.fileKey() != null);
        p("fileKey stable for one file", Objects.equals(a1.fileKey(), a2.fileKey()));
        p("fileKey differs across files", Objects.equals(a1.fileKey(), b1.fileKey()));
        p("fileKey hashCode stable",
          a1.fileKey() == null ? "null" : (a1.fileKey().hashCode() == a2.fileKey().hashCode()));
        BasicFileAttributes d1 = Files.readAttributes(dir, BasicFileAttributes.class);
        p("dir fileKey non-null", d1.fileKey() != null);
        p("dir fileKey differs from file", Objects.equals(d1.fileKey(), a1.fileKey()));

        // The posix view answers the same method from a different declaring
        // class -- a separate registration in this VM.
        try {
            PosixFileAttributes pa = Files.readAttributes(f, PosixFileAttributes.class);
            p("posix fileKey matches basic", Objects.equals(pa.fileKey(), a1.fileKey()));
            p("posix permissions", PosixFilePermissions.toString(pa.permissions()).length());
        } catch (UnsupportedOperationException e) {
            p("posix fileKey matches basic", "UOE");
            p("posix permissions", "UOE");
        }

        // FileStore.getAttribute -> Object. The VALUES are machine state, so
        // only their shape and the refusal are comparable.
        FileStore fs = Files.getFileStore(f);
        t("getFileStore name non-null", () -> { if (fs.name() == null) throw new AssertionError(); });
        pt("getAttribute totalSpace is Long", () -> fs.getAttribute("totalSpace") instanceof Long);
        pt("getAttribute unallocated is Long", () -> fs.getAttribute("unallocatedSpace") instanceof Long);
        pt("getAttribute usable is Long", () -> fs.getAttribute("usableSpace") instanceof Long);
        // The three above are the documented `basic` FileStore attributes and
        // are what `FileStore.getTotalSpace()` and friends are specified to
        // delegate to. Ask the typed accessors as a control: if those work and
        // `getAttribute` refuses, the defect is the string-keyed door, not the
        // measurement underneath it.
        pt("getTotalSpace > 0", () -> fs.getTotalSpace() > 0);
        pt("getUnallocatedSpace >= 0", () -> fs.getUnallocatedSpace() >= 0);
        pt("getUsableSpace >= 0", () -> fs.getUsableSpace() >= 0);
        pt("getBlockSize > 0", () -> fs.getBlockSize() > 0);
        pt("getAttribute matches getTotalSpace",
           () -> fs.getAttribute("totalSpace").equals(fs.getTotalSpace()));
        t("getAttribute bogus", () -> fs.getAttribute("nosuchattribute"));
        t("getAttribute null", () -> fs.getAttribute(null));
        pt("isReadOnly", () -> fs.isReadOnly());
        pt("supportsFileAttributeView basic", () -> fs.supportsFileAttributeView("basic"));
        pt("supportsFileAttributeView posix", () -> fs.supportsFileAttributeView("posix"));
        pt("supportsFileAttributeView bogus", () -> fs.supportsFileAttributeView("nosuchview"));
        pt("supportsFileAttributeView(Class) basic",
           () -> fs.supportsFileAttributeView(BasicFileAttributeView.class));
        pt("fs.type non-null", () -> fs.type() != null);

        // FileAttribute.value() -> Object.
        try {
            Set<PosixFilePermission> perms = PosixFilePermissions.fromString("rw-r-----");
            FileAttribute<Set<PosixFilePermission>> fa = PosixFilePermissions.asFileAttribute(perms);
            p("FileAttribute name", fa.name());
            p("FileAttribute value", PosixFilePermissions.toString(fa.value()));
            p("FileAttribute value is a Set", fa.value() instanceof Set);
            // The attribute must not alias the caller's set -- a shared one lets
            // a later mutation change a file's requested mode after the fact.
            p("FileAttribute value not the same object", fa.value() == perms);
        } catch (UnsupportedOperationException e) {
            p("FileAttribute name", "UOE");
        }

        Files.deleteIfExists(f);
        Files.deleteIfExists(g);
        Files.deleteIfExists(dir);
    }

    public static void main(String[] args) throws Exception {
        allBuffers();
        objectReturns();
        System.out.println("rows " + rows);
        System.out.println("DONE L4BridgeSweep");
    }
}
