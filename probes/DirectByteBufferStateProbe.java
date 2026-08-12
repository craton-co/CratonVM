import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.BufferOverflowException;
import java.nio.BufferUnderflowException;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.ReadOnlyBufferException;
import java.util.ArrayList;
import java.util.List;

/**
 * W7-58 -- the direct-buffer arm of `bb_state`.
 *
 * `bb_state` (native-io/src/lib.rs) resolves a ByteBuffer's backing array as
 * `hb`-by-name, then slot 5, then slot 0, and had NO direct-buffer arm. Applied
 * to a buffer with no backing array it fell through to slot 0 -- which on the
 * real `java.nio.Buffer` layout is `mark`, initialised to -1 -- and raised
 * "ByteBuffer missing backing array (field 0 returned Int(-1))". Every accessor
 * routed through it therefore failed on a DIRECT receiver even when it never
 * needed the array at all (`remaining()`, `hasRemaining()`).
 *
 * The probe runs the SAME battery over a direct buffer and a heap buffer. The
 * heap arm is the control: if it diverges too, the defect is wider than the
 * missing direct arm.
 *
 * Every expected value in here was MEASURED on HotSpot
 * (Eclipse Adoptium jdk-25.0.3.9) and is asserted exactly. Nothing asserts a
 * range (`remaining() >= 0` passes against -1-derived garbage); nothing asserts
 * "did not throw".
 *
 * W7-76 added three sections after the original two arms — `orderPropagation`,
 * `arrayWindow` and `freshOrder`, 77 further checks, 233 in total. They are
 * appended rather than interleaved so every line of the original 156-check
 * transcript stays byte-identical and the expected file diffs additively. The
 * heap arm is still the control in all three.
 *
 * W7-83 appends `segmentBuffer` on the same terms — 49 further checks, 282 in
 * total, all green on HotSpot. It is the oracle for one
 * question: `java.nio.Buffer.segment` (slot 5 on JDK 25) is a `MemorySegment`,
 * NOT a backing array, and a buffer that has one must answer
 * `hasArray() == false` and throw `UnsupportedOperationException` from
 * `array()`. Both CratonVM ByteBuffer families read slot 5 as "maybe an array"
 * and returned the segment; `array()`'s declared return type is `[B`.
 *
 * The section is written against the vacuous shape it would otherwise take.
 * Asserting that `hasArray()` returns *a boolean* passes against either answer,
 * so every row states the exact value on BOTH arms — a native-segment buffer
 * (false) and a heap-segment buffer (true) — and `array()` is driven through
 * real `invokevirtual java/nio/ByteBuffer.array:()[B` rather than reflection,
 * so a VM that answers the native must answer it here.
 *
 * Run: java DirectByteBufferStateProbe
 * Exit 0 = every check matched; exit 1 = at least one FAIL line above.
 */
public final class DirectByteBufferStateProbe {

    private static final List<String> failures = new ArrayList<>();
    private static int checks = 0;

    public static void main(String[] args) {
        arm("direct", ByteBuffer.allocateDirect(16), true);
        arm("heap", ByteBuffer.allocate(16), false);

        // W7-76. Appended AFTER the two original arms so every line above is
        // byte-identical to the 156-check transcript W7-58 measured; the
        // expected file diffs purely additively.
        orderPropagation("direct", ByteBuffer.allocateDirect(16), true);
        orderPropagation("heap", ByteBuffer.allocate(16), false);
        arrayWindow("direct", ByteBuffer.allocateDirect(16), true);
        arrayWindow("heap", ByteBuffer.allocate(16), false);
        freshOrder();

        // W7-83. Appended last, same rule: every line above stays identical.
        segmentBuffer();

        System.out.println("---");
        System.out.println("checks=" + checks + " failures=" + failures.size());
        for (String f : failures) {
            System.out.println("FAIL " + f);
        }
        if (!failures.isEmpty()) {
            System.exit(1);
        }
        System.out.println("PROBE PASS");
    }

    /**
     * `direct` selects the two contract points that genuinely differ between
     * the storage kinds; everything else is identical by specification and is
     * asserted to the same literal on both arms.
     */
    private static void arm(String tag, ByteBuffer b, boolean direct) {
        // --- fresh state -------------------------------------------------
        eq(tag + ".isDirect", b.isDirect(), direct);
        eq(tag + ".isReadOnly", b.isReadOnly(), false);
        eq(tag + ".capacity", b.capacity(), 16);
        eq(tag + ".limit", b.limit(), 16);
        eq(tag + ".position", b.position(), 0);
        eq(tag + ".remaining", b.remaining(), 16);
        eq(tag + ".hasRemaining", b.hasRemaining(), true);
        eq(tag + ".order", b.order().toString(), "BIG_ENDIAN");

        // `hasArray()`/`array()`/`arrayOffset()` are the storage-kind fork.
        // A direct buffer MUST report false and MUST throw
        // UnsupportedOperationException -- not return a fabricated array and
        // not return garbage derived from a misread slot.
        eq(tag + ".hasArray", b.hasArray(), !direct);
        if (direct) {
            eq(tag + ".array.throws", throwName(() -> b.array()),
                    "java.lang.UnsupportedOperationException");
            eq(tag + ".arrayOffset.throws", throwName(() -> b.arrayOffset()),
                    "java.lang.UnsupportedOperationException");
        } else {
            eq(tag + ".array.length", b.array().length, 16);
            eq(tag + ".arrayOffset", b.arrayOffset(), 0);
        }

        // --- relative put/get -------------------------------------------
        for (int i = 0; i < 16; i++) {
            b.put((byte) (i * 7));
        }
        eq(tag + ".afterFill.position", b.position(), 16);
        eq(tag + ".afterFill.remaining", b.remaining(), 0);
        eq(tag + ".afterFill.hasRemaining", b.hasRemaining(), false);
        eq(tag + ".overflow.throws", throwName(() -> b.put((byte) 1)),
                "java.nio.BufferOverflowException");

        b.flip();
        eq(tag + ".afterFlip.position", b.position(), 0);
        eq(tag + ".afterFlip.limit", b.limit(), 16);
        eq(tag + ".afterFlip.remaining", b.remaining(), 16);
        eq(tag + ".get0", b.get(), (byte) 0);
        eq(tag + ".get1", b.get(), (byte) 7);
        eq(tag + ".afterTwoGets.position", b.position(), 2);
        eq(tag + ".afterTwoGets.remaining", b.remaining(), 14);

        // --- absolute get/put -------------------------------------------
        eq(tag + ".getAbs3", b.get(3), (byte) 21);
        eq(tag + ".getAbs15", b.get(15), (byte) 105);
        eq(tag + ".getAbs.oob.throws", throwName(() -> b.get(16)),
                "java.lang.IndexOutOfBoundsException");
        eq(tag + ".getAbs.negative.throws", throwName(() -> b.get(-1)),
                "java.lang.IndexOutOfBoundsException");
        b.put(3, (byte) -50);
        eq(tag + ".putAbs.readback", b.get(3), (byte) -50);
        eq(tag + ".putAbs.leavesPosition", b.position(), 2);
        b.put(3, (byte) 21);

        // --- position/limit contract ------------------------------------
        b.position(4);
        eq(tag + ".setPosition.remaining", b.remaining(), 12);
        eq(tag + ".setPosition.position", b.position(), 4);
        eq(tag + ".position.oob.throws", throwName(() -> b.position(17)),
                "java.lang.IllegalArgumentException");
        eq(tag + ".limit.oob.throws", throwName(() -> b.limit(17)),
                "java.lang.IllegalArgumentException");

        // --- typed accessors, both byte orders ---------------------------
        // Bytes 4..7 are 28,35,42,49 = 0x1C232A31.
        b.order(ByteOrder.BIG_ENDIAN);
        eq(tag + ".order.afterBE", b.order().toString(), "BIG_ENDIAN");
        eq(tag + ".getIntBE", b.getInt(4), 0x1C232A31);
        b.order(ByteOrder.LITTLE_ENDIAN);
        eq(tag + ".order.afterLE", b.order().toString(), "LITTLE_ENDIAN");
        eq(tag + ".getIntLE", b.getInt(4), 0x312A231C);
        b.putInt(4, 0x01020304);
        eq(tag + ".putIntLE.byte4", b.get(4), (byte) 0x04);
        eq(tag + ".putIntLE.byte7", b.get(7), (byte) 0x01);
        b.order(ByteOrder.BIG_ENDIAN);
        eq(tag + ".getIntBE.afterPutLE", b.getInt(4), 0x04030201);
        // restore 28,35,42,49
        b.putInt(4, 0x1C232A31);
        eq(tag + ".restore.byte4", b.get(4), (byte) 28);

        // --- slice -------------------------------------------------------
        // position=4, limit=16 -> a 12-element window starting at byte 4.
        ByteBuffer s = b.slice();
        eq(tag + ".slice.capacity", s.capacity(), 12);
        eq(tag + ".slice.position", s.position(), 0);
        eq(tag + ".slice.limit", s.limit(), 12);
        eq(tag + ".slice.remaining", s.remaining(), 12);
        eq(tag + ".slice.isDirect", s.isDirect(), direct);
        eq(tag + ".slice.hasArray", s.hasArray(), !direct);
        eq(tag + ".slice.get0", s.get(0), (byte) 28);
        eq(tag + ".slice.get11", s.get(11), (byte) 105);
        eq(tag + ".slice.order", s.order().toString(), "BIG_ENDIAN");
        // slice() ALIASES: a write through the slice is visible in the parent.
        s.put(0, (byte) 99);
        eq(tag + ".slice.aliasesParent", b.get(4), (byte) 99);
        s.put(0, (byte) 28);
        eq(tag + ".slice.underflow.throws", throwName(() -> {
            ByteBuffer e = s.duplicate();
            e.position(e.limit());
            return e.get();
        }), "java.nio.BufferUnderflowException");

        // --- duplicate ---------------------------------------------------
        ByteBuffer d = b.duplicate();
        eq(tag + ".duplicate.capacity", d.capacity(), 16);
        eq(tag + ".duplicate.position", d.position(), 4);
        eq(tag + ".duplicate.limit", d.limit(), 16);
        eq(tag + ".duplicate.remaining", d.remaining(), 12);
        eq(tag + ".duplicate.isDirect", d.isDirect(), direct);
        eq(tag + ".duplicate.hasArray", d.hasArray(), !direct);
        eq(tag + ".duplicate.get0", d.get(0), (byte) 0);
        d.put(0, (byte) 77);
        eq(tag + ".duplicate.aliasesParent", b.get(0), (byte) 77);
        d.put(0, (byte) 0);
        // The duplicate's own position is independent of the parent's.
        d.position(9);
        eq(tag + ".duplicate.independentPosition", b.position(), 4);

        // --- asReadOnlyBuffer --------------------------------------------
        ByteBuffer r = b.asReadOnlyBuffer();
        eq(tag + ".readOnly.isReadOnly", r.isReadOnly(), true);
        eq(tag + ".readOnly.isDirect", r.isDirect(), direct);
        eq(tag + ".readOnly.hasArray", r.hasArray(), false);
        eq(tag + ".readOnly.capacity", r.capacity(), 16);
        eq(tag + ".readOnly.position", r.position(), 4);
        eq(tag + ".readOnly.get0", r.get(0), (byte) 0);
        eq(tag + ".readOnly.put.throws", throwName(() -> r.put(0, (byte) 1)),
                "java.nio.ReadOnlyBufferException");
        eq(tag + ".readOnly.array.throws", throwName(() -> r.array()),
                direct ? "java.lang.UnsupportedOperationException"
                       : "java.nio.ReadOnlyBufferException");
        eq(tag + ".parent.stillWritable", b.isReadOnly(), false);

        // --- clear / rewind / compact ------------------------------------
        b.clear();
        eq(tag + ".clear.position", b.position(), 0);
        eq(tag + ".clear.limit", b.limit(), 16);
        eq(tag + ".clear.remaining", b.remaining(), 16);
        b.position(6);
        b.limit(14);
        b.compact();
        eq(tag + ".compact.position", b.position(), 8);
        eq(tag + ".compact.limit", b.limit(), 16);
        eq(tag + ".compact.remaining", b.remaining(), 8);
        // compact() moved bytes 6..13 down to 0..7; byte 6 was 42.
        eq(tag + ".compact.movedByte0", b.get(0), (byte) 42);
        b.rewind();
        eq(tag + ".rewind.position", b.position(), 0);
        eq(tag + ".rewind.remaining", b.remaining(), 16);
    }

    /**
     * W7-76 — byte order: where it comes from, and what does NOT inherit it.
     *
     * `java.nio.ByteBuffer` declares `boolean bigEndian = true` as a FIELD
     * INITIALISER, so the value is written by every `ByteBuffer` constructor
     * and by nothing else. A buffer a VM fabricates with a raw object
     * allocation — no constructor run — leaves it at the Java default `false`,
     * and real `ByteBuffer.order()` (which is `public final` and reads the
     * field directly, so it cannot be intercepted per-subclass) then answers
     * LITTLE_ENDIAN. That is the whole of the defect this section exists for,
     * and `.fresh` below is its one-line statement.
     *
     * The derived-buffer rows are MEASURED and are the opposite of the
     * intuition the task carried in: `slice()`, `slice(int,int)`,
     * `duplicate()` and `asReadOnlyBuffer()` preserve CONTENT and do NOT
     * preserve ORDER. Each of them runs a constructor, so each comes back
     * BIG_ENDIAN however the source was set. Asserting "preserves order" here
     * would have pinned a behaviour HotSpot does not have.
     */
    private static void orderPropagation(String tag, ByteBuffer b, boolean direct) {
        String t = tag + ".ord";
        for (int i = 0; i < 16; i++) {
            b.put(i, (byte) (i * 7));
        }
        // bytes 0..3 are 0,7,14,21 = 0x00070E15.
        eq(t + ".fresh", b.order().toString(), "BIG_ENDIAN");
        eq(t + ".isDirect", b.isDirect(), direct);
        eq(t + ".getIntBE", b.getInt(0), 0x00070E15);
        b.order(ByteOrder.LITTLE_ENDIAN);
        eq(t + ".afterSet", b.order().toString(), "LITTLE_ENDIAN");
        eq(t + ".getIntLE", b.getInt(0), 0x150E0700);

        ByteBuffer s = b.slice();
        eq(t + ".slice.order", s.order().toString(), "BIG_ENDIAN");
        eq(t + ".slice.getIntBE", s.getInt(0), 0x00070E15);
        eq(t + ".slice.get5", s.get(5), (byte) 35);

        ByteBuffer s2 = b.slice(2, 4);
        eq(t + ".sliceRange.order", s2.order().toString(), "BIG_ENDIAN");
        eq(t + ".sliceRange.capacity", s2.capacity(), 4);
        eq(t + ".sliceRange.get0", s2.get(0), (byte) 14);

        ByteBuffer d = b.duplicate();
        eq(t + ".duplicate.order", d.order().toString(), "BIG_ENDIAN");
        eq(t + ".duplicate.get5", d.get(5), (byte) 35);

        ByteBuffer r = b.asReadOnlyBuffer();
        eq(t + ".readOnly.order", r.order().toString(), "BIG_ENDIAN");
        eq(t + ".readOnly.get5", r.get(5), (byte) 35);

        // A TYPED VIEW is the one thing that DOES carry the order across, and
        // it is not an exception to the rule above: the JDK compiles one
        // concrete view class per endianness (`ByteBufferAsIntBufferB` /
        // `...L`), so the order is frozen into the CLASS at creation from the
        // source's order at that moment. A VM that decides a view's order from
        // a field rather than from the class it stamped gets these two rows
        // the same and both wrong.
        eq(t + ".asIntBuffer.fromLE", b.asIntBuffer().order().toString(), "LITTLE_ENDIAN");
        b.order(ByteOrder.BIG_ENDIAN);
        eq(t + ".asIntBuffer.fromBE", b.asIntBuffer().order().toString(), "BIG_ENDIAN");
        eq(t + ".asIntBuffer.get0", b.asIntBuffer().get(0), 0x00070E15);
        eq(t + ".restored", b.order().toString(), "BIG_ENDIAN");
    }

    /**
     * W7-76 — `hasArray`/`array`/`arrayOffset` on a WINDOW, which is where a
     * fabricated answer stops being indistinguishable from the real one.
     *
     * On a fresh heap buffer `arrayOffset()` is 0 and `array().length` is the
     * capacity, so a native that returns a fresh copy of the right size passes
     * every check the original arm makes. A heap `slice()` separates them: the
     * array is the PARENT's, still 16 long, and the offset is 4. The identity
     * row is the one that cannot be faked.
     */
    private static void arrayWindow(String tag, ByteBuffer b, boolean direct) {
        String t = tag + ".win";
        for (int i = 0; i < 16; i++) {
            b.put(i, (byte) (i * 7));
        }
        b.position(4);
        ByteBuffer w = b.slice();
        eq(t + ".capacity", w.capacity(), 12);
        eq(t + ".position", w.position(), 0);
        eq(t + ".hasArray", w.hasArray(), !direct);
        if (direct) {
            eq(t + ".array.throws", throwName(() -> w.array()),
                    "java.lang.UnsupportedOperationException");
            eq(t + ".arrayOffset.throws", throwName(() -> w.arrayOffset()),
                    "java.lang.UnsupportedOperationException");
        } else {
            eq(t + ".array.length", w.array().length, 16);
            eq(t + ".array.identity", w.array() == b.array(), true);
            eq(t + ".arrayOffset", w.arrayOffset(), 4);
            eq(t + ".array.windowByte", w.array()[w.arrayOffset()], (byte) 28);
        }
        eq(t + ".get0", w.get(0), (byte) 28);
        eq(t + ".get11", w.get(11), (byte) 105);

        // `hasArray()` is `hb != null && !isReadOnly`, so BOTH read-only arms
        // answer false — including the heap one, whose array exists. The two
        // arms then differ in WHICH exception they raise, and the direct one is
        // not the read-only exception.
        ByteBuffer ro = w.asReadOnlyBuffer();
        eq(t + ".readOnly.hasArray", ro.hasArray(), false);
        eq(t + ".readOnly.array.throws", throwName(() -> ro.array()),
                direct ? "java.lang.UnsupportedOperationException"
                       : "java.nio.ReadOnlyBufferException");
        eq(t + ".readOnly.arrayOffset.throws", throwName(() -> ro.arrayOffset()),
                direct ? "java.lang.UnsupportedOperationException"
                       : "java.nio.ReadOnlyBufferException");
    }

    /**
     * W7-76 — the RED, stated as plainly as it can be stated: every factory
     * that mints a ByteBuffer answers BIG_ENDIAN, at every size, through every
     * entry point. `allocate(0)` is included because a zero-capacity buffer is
     * the one case where a VM might skip its allocation path entirely.
     */
    private static void freshOrder() {
        eq("fresh.allocate8.order", ByteBuffer.allocate(8).order().toString(), "BIG_ENDIAN");
        eq("fresh.allocate0.order", ByteBuffer.allocate(0).order().toString(), "BIG_ENDIAN");
        eq("fresh.allocateDirect8.order",
                ByteBuffer.allocateDirect(8).order().toString(), "BIG_ENDIAN");

        byte[] backing = new byte[16];
        ByteBuffer w = ByteBuffer.wrap(backing);
        eq("fresh.wrap.order", w.order().toString(), "BIG_ENDIAN");
        eq("fresh.wrap.arrayIdentity", w.array() == backing, true);
        eq("fresh.wrap.arrayOffset", w.arrayOffset(), 0);
        eq("fresh.wrap.capacity", w.capacity(), 16);

        // wrap(array, off, len) sets POSITION and LIMIT, not offset/capacity —
        // the buffer still spans the whole array and arrayOffset stays 0. Its
        // slice() is what carries the offset.
        ByteBuffer wr = ByteBuffer.wrap(backing, 4, 8);
        eq("fresh.wrapRange.order", wr.order().toString(), "BIG_ENDIAN");
        eq("fresh.wrapRange.position", wr.position(), 4);
        eq("fresh.wrapRange.limit", wr.limit(), 12);
        eq("fresh.wrapRange.capacity", wr.capacity(), 16);
        eq("fresh.wrapRange.arrayOffset", wr.arrayOffset(), 0);
        eq("fresh.wrapRange.remaining", wr.remaining(), 8);

        ByteBuffer wrs = wr.slice();
        eq("fresh.wrapRange.slice.arrayOffset", wrs.arrayOffset(), 4);
        eq("fresh.wrapRange.slice.capacity", wrs.capacity(), 8);
        eq("fresh.wrapRange.slice.order", wrs.order().toString(), "BIG_ENDIAN");
        eq("fresh.wrapRange.slice.arrayIdentity", wrs.array() == backing, true);
    }

    /**
     * W7-83 — `java.nio.Buffer.segment` is not a backing array.
     *
     * Slot 5 of a JDK 25 `Buffer` is `final MemorySegment segment`, and both
     * CratonVM ByteBuffer families read it as a candidate backing array (it is
     * the only Object-typed field `Buffer` declares, so `native-builtins`'
     * typed buffer views deliberately park a real array there). On a receiver
     * minted by `MemorySegment.asByteBuffer()` the field is a live
     * `jdk.internal.foreign.NativeMemorySegmentImpl` and `hb` is null, so the
     * segment was handed back from `array()` — declared `()[B`.
     *
     * The two arms are the discriminator and they are the same battery:
     *
     * - `seg.native.*` — an `Arena` segment. `hasArray()` is FALSE and both
     *   accessors throw `UnsupportedOperationException`. A VM that returns the
     *   segment fails `hasArray` (true where false is expected) and fails
     *   `array.throws` with a `NO-THROW:` line naming what it returned.
     * - `seg.heap.*` — `MemorySegment.ofArray(byte[])`. `hasArray()` is TRUE
     *   and `array()` returns THE VERY ARRAY, asserted by identity. A VM that
     *   "fixes" the first arm by refusing every slot-5 value fails here.
     *
     * `seg.heapSlice.*` is the row that cannot be faked by an accessor that
     * fabricates a right-sized copy: a sliced heap segment's buffer has
     * capacity 8, `arrayOffset() == 4`, and `array()` is the WHOLE 16-element
     * parent array by identity.
     *
     * Every value below was measured on Eclipse Adoptium jdk-25.0.3.9.
     */
    private static void segmentBuffer() {
        // --- a NATIVE segment: storage exists, a backing ARRAY does not ---
        MemorySegment ns = Arena.ofAuto().allocate(16);
        ByteBuffer nb = ns.asByteBuffer();
        eq("seg.native.isDirect", nb.isDirect(), true);
        eq("seg.native.isReadOnly", nb.isReadOnly(), false);
        eq("seg.native.capacity", nb.capacity(), 16);
        eq("seg.native.limit", nb.limit(), 16);
        eq("seg.native.position", nb.position(), 0);
        eq("seg.native.order", nb.order().toString(), "BIG_ENDIAN");
        eq("seg.native.hasArray", nb.hasArray(), false);
        eq("seg.native.array.throws", throwName(() -> nb.array()),
                "java.lang.UnsupportedOperationException");
        eq("seg.native.arrayOffset.throws", throwName(() -> nb.arrayOffset()),
                "java.lang.UnsupportedOperationException");
        // The storage is real, and it is the segment's: refusing to call it an
        // array must not degrade to "no storage at all".
        eq("seg.native.freshGet0", nb.get(0), (byte) 0);
        nb.put(0, (byte) 0x5A);
        eq("seg.native.putGet0", nb.get(0), (byte) 0x5A);
        eq("seg.native.aliasesSegment", ns.get(ValueLayout.JAVA_BYTE, 0), (byte) 0x5A);
        eq("seg.native.getIntBE", nb.getInt(0), 0x5A000000);

        // A derived view of it is still array-less.
        ByteBuffer nbs = nb.slice(4, 8);
        eq("seg.native.slice.capacity", nbs.capacity(), 8);
        eq("seg.native.slice.isDirect", nbs.isDirect(), true);
        eq("seg.native.slice.hasArray", nbs.hasArray(), false);
        eq("seg.native.slice.array.throws", throwName(() -> nbs.array()),
                "java.lang.UnsupportedOperationException");
        ByteBuffer nro = nb.asReadOnlyBuffer();
        eq("seg.native.readOnly.isReadOnly", nro.isReadOnly(), true);
        eq("seg.native.readOnly.hasArray", nro.hasArray(), false);
        // UnsupportedOperationException, NOT ReadOnlyBufferException: the
        // storage kind is decided before the mutability, and W7-58 measured the
        // same split for `allocateDirect`. Getting it backwards is a
        // one-word error that a "did it throw" check would not see.
        eq("seg.native.readOnly.array.throws", throwName(() -> nro.array()),
                "java.lang.UnsupportedOperationException");

        // A read-only NATIVE segment answers the same way.
        ByteBuffer roSeg = Arena.ofAuto().allocate(16).asReadOnly().asByteBuffer();
        eq("seg.nativeRO.isReadOnly", roSeg.isReadOnly(), true);
        eq("seg.nativeRO.hasArray", roSeg.hasArray(), false);
        eq("seg.nativeRO.array.throws", throwName(() -> roSeg.array()),
                "java.lang.UnsupportedOperationException");

        // A confined arena's segment is the same shape with a different
        // lifetime — included because it is the receiver W7-76 §8.1 measured.
        try (Arena confined = Arena.ofConfined()) {
            ByteBuffer cb = confined.allocate(16).asByteBuffer();
            eq("seg.confined.isDirect", cb.isDirect(), true);
            eq("seg.confined.hasArray", cb.hasArray(), false);
            eq("seg.confined.array.throws", throwName(() -> cb.array()),
                    "java.lang.UnsupportedOperationException");
        }

        // --- the HEAP control: a segment over a byte[] DOES have an array ---
        byte[] backing = new byte[16];
        for (int i = 0; i < 16; i++) {
            backing[i] = (byte) (i * 7);
        }
        ByteBuffer hb = MemorySegment.ofArray(backing).asByteBuffer();
        eq("seg.heap.isDirect", hb.isDirect(), false);
        eq("seg.heap.isReadOnly", hb.isReadOnly(), false);
        eq("seg.heap.capacity", hb.capacity(), 16);
        eq("seg.heap.order", hb.order().toString(), "BIG_ENDIAN");
        eq("seg.heap.hasArray", hb.hasArray(), true);
        eq("seg.heap.arrayLength", hb.array().length, 16);
        eq("seg.heap.arrayIdentity", hb.array() == backing, true);
        eq("seg.heap.arrayOffset", hb.arrayOffset(), 0);
        eq("seg.heap.get0", hb.get(0), (byte) 0);
        eq("seg.heap.get4", hb.get(4), (byte) 28);

        // The window row: capacity 8, offset 4, and the array is the PARENT's
        // 16 elements. A fabricated copy of the right size passes every other
        // check in this section and fails these three.
        ByteBuffer hs = MemorySegment.ofArray(backing).asSlice(4, 8).asByteBuffer();
        eq("seg.heapSlice.isDirect", hs.isDirect(), false);
        eq("seg.heapSlice.capacity", hs.capacity(), 8);
        eq("seg.heapSlice.hasArray", hs.hasArray(), true);
        eq("seg.heapSlice.arrayLength", hs.array().length, 16);
        eq("seg.heapSlice.arrayIdentity", hs.array() == backing, true);
        eq("seg.heapSlice.arrayOffset", hs.arrayOffset(), 4);
        eq("seg.heapSlice.get0", hs.get(0), (byte) 28);
        hs.put(0, (byte) 99);
        eq("seg.heapSlice.aliasesBacking", backing[4], (byte) 99);
        backing[4] = (byte) 28;

        // A read-only HEAP segment splits the other way — ReadOnlyBufferException,
        // not UnsupportedOperationException. Same asymmetry W7-76 §7 measured
        // for `asReadOnlyBuffer`, reached here through the segment API.
        ByteBuffer roHeap = MemorySegment.ofArray(backing).asReadOnly().asByteBuffer();
        eq("seg.heapRO.isReadOnly", roHeap.isReadOnly(), true);
        eq("seg.heapRO.hasArray", roHeap.hasArray(), false);
        eq("seg.heapRO.array.throws", throwName(() -> roHeap.array()),
                "java.nio.ReadOnlyBufferException");
        eq("seg.heapRO.arrayOffset.throws", throwName(() -> roHeap.arrayOffset()),
                "java.nio.ReadOnlyBufferException");

        // And a segment over a NON-byte array has no ByteBuffer at all: the
        // JDK refuses at `asByteBuffer()` rather than minting a buffer whose
        // `array()` would be an `int[]`. Same species of refusal as the screen
        // this section is the oracle for, one layer up.
        int[] ints = new int[4];
        eq("seg.intArray.asByteBuffer.throws",
                throwName(() -> MemorySegment.ofArray(ints).asByteBuffer()),
                "java.lang.UnsupportedOperationException");
    }

    // ---- harness ---------------------------------------------------------

    private interface Body {
        Object run();
    }

    /**
     * Fully-qualified name of what `body` threw, or the literal
     * "NO-THROW:<value>" when it returned. Naming the returned value matters:
     * a native that answers a direct `array()` with a fabricated array or with
     * a misread slot returns instead of throwing, and the failure line must
     * show what it returned rather than just "expected a throw".
     */
    private static String throwName(Body body) {
        try {
            Object v = body.run();
            return "NO-THROW:" + describe(v);
        } catch (ReadOnlyBufferException e) {
            // MUST precede UnsupportedOperationException: ReadOnlyBufferException
            // EXTENDS it, so the wider catch first is a compile error and, once
            // reordered by hand, would silently relabel every read-only refusal
            // as an unsupported-operation one.
            return "java.nio.ReadOnlyBufferException";
        } catch (UnsupportedOperationException e) {
            return "java.lang.UnsupportedOperationException";
        } catch (BufferUnderflowException e) {
            return "java.nio.BufferUnderflowException";
        } catch (BufferOverflowException e) {
            return "java.nio.BufferOverflowException";
        } catch (IndexOutOfBoundsException e) {
            // IndexOutOfBoundsException, not its subclasses: `ByteBuffer.get(int)`
            // is specified to throw the base type and HotSpot throws exactly it.
            return "java.lang.IndexOutOfBoundsException";
        } catch (IllegalArgumentException e) {
            return "java.lang.IllegalArgumentException";
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    private static String describe(Object v) {
        if (v == null) {
            return "null";
        }
        if (v instanceof byte[]) {
            return "byte[" + ((byte[]) v).length + "]";
        }
        return String.valueOf(v);
    }

    private static void eq(String name, Object actual, Object expected) {
        checks++;
        String a = describe(actual);
        String e = describe(expected);
        boolean ok = a.equals(e);
        System.out.println((ok ? "ok   " : "BAD  ") + name + " = " + a
                + (ok ? "" : "   (expected " + e + ")"));
        if (!ok) {
            failures.add(name + ": got " + a + ", expected " + e);
        }
    }
}
