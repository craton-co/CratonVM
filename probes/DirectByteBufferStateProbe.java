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
 * Run: java DirectByteBufferStateProbe
 * Exit 0 = every check matched; exit 1 = at least one FAIL line above.
 */
public final class DirectByteBufferStateProbe {

    private static final List<String> failures = new ArrayList<>();
    private static int checks = 0;

    public static void main(String[] args) {
        arm("direct", ByteBuffer.allocateDirect(16), true);
        arm("heap", ByteBuffer.allocate(16), false);

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
