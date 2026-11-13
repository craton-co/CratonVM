import java.lang.reflect.Field;

/**
 * {@code sun.misc.Unsafe.{get,put}X(Object base, long offset, …)} with an ARRAY
 * base, at every primitive width.
 *
 * <p>An array base is an ELEMENT access. CratonVM's int, long, byte, short and
 * object natives have always screened the receiver kind and routed an array to
 * {@code get/set_array_element}; the four FLOAT and DOUBLE natives did not, and
 * passed the byte offset straight to the plain-object field accessor as if it
 * were a slot index.
 *
 * <p>That is not a type error the accessor can catch, because an array MIRRORS
 * ITS LENGTH into {@code num_slots}: {@code new byte[32]} reports thirty-two
 * "fields", so every offset up to 31 is "in bounds" while the byte offset the
 * accessor computes is {@code HEADER_SIZE + offset * 16}. For the shape below —
 * which is exactly Hazelcast's {@code UnsafeUtil.checkUnsafeInstance}, run on
 * every Spring Boot cache autoconfiguration test — {@code putFloat(buf, 16, 3f)}
 * addressed byte 272 of a 32-byte body: a silent 240-byte out-of-bounds WRITE,
 * twice per run, that no guard could see because the striden bytes happened to
 * form a valid discriminant.
 *
 * <p>Every assertion here is a HotSpot-observable round trip, so the vector
 * says what the platform says rather than what this VM happens to do. The
 * out-of-bounds write is caught by the NEIGHBOUR arrays: they are allocated
 * around the target, filled with a known pattern, and checked afterwards. A
 * write that lands past the target has to land on one of them.
 */
public class RUnsafeArrayBase {
    static int checks = 0;

    static void eq(String what, Object got, Object want) {
        checks++;
        if (!String.valueOf(want).equals(String.valueOf(got))) {
            throw new AssertionError(what + ": expected [" + want + "] got [" + got + "]");
        }
    }

    static void ck(String what, boolean ok, String detail) {
        checks++;
        if (!ok) {
            throw new AssertionError(what + ": " + detail);
        }
    }

    static sun.misc.Unsafe unsafe() throws Exception {
        Field f = sun.misc.Unsafe.class.getDeclaredField("theUnsafe");
        f.setAccessible(true);
        return (sun.misc.Unsafe) f.get(null);
    }

    /** Fill with a distinctive pattern so ANY foreign write is visible. */
    static byte[] guardArray(int len, int seed) {
        byte[] a = new byte[len];
        for (int i = 0; i < len; i++) {
            a[i] = (byte) (seed + i);
        }
        return a;
    }

    static void checkGuard(String what, byte[] a, int seed) {
        for (int i = 0; i < a.length; i++) {
            ck(what + "[" + i + "]", a[i] == (byte) (seed + i),
                    "neighbour byte " + i + " was overwritten: expected "
                            + (byte) (seed + i) + " got " + a[i]);
        }
    }

    /**
     * The `ARRAY_*` CONSTANT, if this image declares it. `null` when absent.
     *
     * <p>Read reflectively rather than referenced directly so the vector keeps
     * one check count in every mode: a mode whose `sun.misc.Unsafe` does not
     * declare these still runs -- and passes -- the same number of assertions.
     */
    static Long constant(String name) {
        try {
            Field f = sun.misc.Unsafe.class.getDeclaredField(name);
            f.setAccessible(true);
            return ((Number) f.get(null)).longValue();
        } catch (Throwable t) {
            return null;
        }
    }

    /**
     * A declared constant must equal what the native answers.
     *
     * <p>They are ONE NUMBER by the JDK's own construction -- `<clinit>` fills
     * the constant from the native -- so this holds on HotSpot and on CratonVM
     * without pinning a value that legitimately differs between them (this VM
     * uses a uniform 16-byte header and no compressed oops).
     *
     * <p>MEASURED 2026-08-30: every one of these nineteen constants was ZERO
     * on CratonVM while `arrayBaseOffset()` answered correctly, because
     * `sun.misc.Unsafe.<clinit>` computes them through natives that are not
     * registered that early in boot and an unregistered native returns its
     * return type's zero instead of throwing. Any consumer following the
     * documented `ARRAY_<T>_BASE_OFFSET + index * ARRAY_<T>_INDEX_SCALE`
     * protocol read bytes 16 short, silently. This vector called the method
     * and never the constant, so it stayed green throughout.
     */
    static void agrees(String name, long fromNative) {
        Long c = constant(name);
        ck("sun.misc.Unsafe." + name + " agrees with the native",
                c == null || c.longValue() == fromNative,
                "constant says " + c + ", native says " + fromNative
                        + " -- a zero here means <clinit> latched an unregistered"
                        + " native's zero return");
    }

    public static void main(String[] args) throws Exception {
        sun.misc.Unsafe u = unsafe();

        long base = u.arrayBaseOffset(byte[].class);
        ck("arrayBaseOffset(byte[]) is positive", base > 0, "got " + base);

        // Hazelcast's own shape, verbatim in structure: a byte[] sized
        // base + 2*8, written at every primitive width.
        byte[] before = guardArray(64, 11);
        byte[] buffer = new byte[(int) base + 16];
        byte[] after = guardArray(64, 77);

        u.putByte(buffer, base, (byte) 0x00);
        u.putBoolean(buffer, base, false);
        u.putChar(buffer, base + 2, '0');
        u.putShort(buffer, base + 2, (short) 1);
        u.putInt(buffer, base + 4, 2);
        u.putFloat(buffer, base + 4, 3f);
        u.putLong(buffer, base + 8, 4L);
        u.putDouble(buffer, base + 8, 5d);

        // NOTHING may have landed outside `buffer`.
        checkGuard("neighbour before", before, 11);
        checkGuard("neighbour after", after, 77);

        // And the target must still be its own length, readable end to end.
        eq("buffer.length", buffer.length, (int) base + 16);
        int sum = 0;
        for (byte b : buffer) {
            sum += b & 0xff;
        }
        ck("buffer is readable end to end", sum >= 0, "sum=" + sum);

        // ---- round trips at each width, on a typed array of that width -----
        //
        // A typed array is where the ELEMENT interpretation is unambiguous, so
        // these are exact-value assertions rather than "did not corrupt".
        float[] fa = new float[4];
        long fbase = u.arrayBaseOffset(float[].class);
        long fscale = u.arrayIndexScale(float[].class);
        u.putFloat(fa, fbase + 2 * fscale, 2.5f);
        eq("float[] via Unsafe round trip", fa[2], 2.5f);
        eq("float[] via Unsafe read", u.getFloat(fa, fbase + 2 * fscale), 2.5f);
        eq("float[] untouched neighbour", fa[3], 0.0f);

        double[] da = new double[4];
        long dbase = u.arrayBaseOffset(double[].class);
        long dscale = u.arrayIndexScale(double[].class);
        u.putDouble(da, dbase + 1 * dscale, 6.25d);
        eq("double[] via Unsafe round trip", da[1], 6.25d);
        eq("double[] via Unsafe read", u.getDouble(da, dbase + 1 * dscale), 6.25d);
        eq("double[] untouched neighbour", da[2], 0.0d);

        int[] ia = new int[4];
        long ibase = u.arrayBaseOffset(int[].class);
        long iscale = u.arrayIndexScale(int[].class);
        u.putInt(ia, ibase + 3 * iscale, 99);
        eq("int[] via Unsafe round trip", ia[3], 99);
        eq("int[] via Unsafe read", u.getInt(ia, ibase + 3 * iscale), 99);

        long[] la = new long[4];
        long lbase = u.arrayBaseOffset(long[].class);
        long lscale = u.arrayIndexScale(long[].class);
        u.putLong(la, lbase + 1 * lscale, 1234567890123L);
        eq("long[] via Unsafe round trip", la[1], 1234567890123L);
        eq("long[] via Unsafe read", u.getLong(la, lbase + 1 * lscale), 1234567890123L);

        Object[] oa = new Object[4];
        long obase = u.arrayBaseOffset(Object[].class);
        long oscale = u.arrayIndexScale(Object[].class);
        u.putObject(oa, obase + 2 * oscale, "v");
        eq("Object[] via Unsafe round trip", oa[2], "v");
        eq("Object[] via Unsafe read", u.getObject(oa, obase + 2 * oscale), "v");
        eq("Object[] untouched neighbour", oa[3], null);

        // ---- byte-ADDRESSED round trips over a byte[] ---------------------
        //
        // This is the half a "did not corrupt anything" assertion cannot see.
        // `Unsafe.putFloat(byte[], off, v)` writes FOUR bytes at that offset,
        // not one truncated element, and `Bits.writeIntL([BII)` — one frame
        // below Hazelcast's probe in the stack that found this defect — is
        // exactly that idiom. Routing the array base to `set_array_element`
        // alone stops the out-of-bounds write and still gives the wrong answer.
        byte[] buf = new byte[(int) base + 32];
        byte[] bufGuardBefore = guardArray(64, 55);
        byte[] bufGuardAfter = guardArray(64, 99);
        u.putFloat(buf, base, 1.5f);
        u.putDouble(buf, base + 8, 2.75d);
        u.putInt(buf, base + 16, 0x01020304);
        u.putLong(buf, base + 24, 0x0102030405060708L);
        eq("byte[] getFloat round trip", u.getFloat(buf, base), 1.5f);
        eq("byte[] getDouble round trip", u.getDouble(buf, base + 8), 2.75d);
        eq("byte[] getInt round trip", u.getInt(buf, base + 16), 0x01020304);
        eq("byte[] getLong round trip", u.getLong(buf, base + 24), 0x0102030405060708L);
        // The FOUR bytes must actually be four bytes: read them back as
        // elements and rebuild the value the way the JDK's own Bits does.
        int rebuilt = (buf[0] & 0xff) | ((buf[1] & 0xff) << 8)
                | ((buf[2] & 0xff) << 16) | ((buf[3] & 0xff) << 24);
        eq("byte[] float landed as 4 elements", Float.intBitsToFloat(rebuilt), 1.5f);
        ck("byte[] float did not land as one truncated element",
                !(buf[1] == 0 && buf[2] == 0 && buf[3] == 0),
                "bytes 1..3 are all zero — the write was one element wide");
        checkGuard("byte[] neighbour before", bufGuardBefore, 55);
        checkGuard("byte[] neighbour after", bufGuardAfter, 99);

        // ---- an OUT-OF-RANGE offset must not write anything ---------------
        //
        // Past the end of the element range there is no element to name. It
        // must be a no-op, not a stride into the next allocation — which is
        // what the float/double path did.
        float[] small = new float[2];
        byte[] sentinel = guardArray(64, 33);
        u.putFloat(small, fbase + 64 * fscale, 9f);
        u.putDouble(new double[2], dbase + 64 * dscale, 9d);
        checkGuard("sentinel after an out-of-range Unsafe write", sentinel, 33);
        eq("small[0] untouched", small[0], 0.0f);
        eq("small[1] untouched", small[1], 0.0f);

        // The CONSTANTS, not just the method. See `agrees` for what this
        // caught and why this vector missed it for as long as it existed.
        agrees("ARRAY_BOOLEAN_BASE_OFFSET", u.arrayBaseOffset(boolean[].class));
        agrees("ARRAY_BYTE_BASE_OFFSET", u.arrayBaseOffset(byte[].class));
        agrees("ARRAY_SHORT_BASE_OFFSET", u.arrayBaseOffset(short[].class));
        agrees("ARRAY_CHAR_BASE_OFFSET", u.arrayBaseOffset(char[].class));
        agrees("ARRAY_INT_BASE_OFFSET", u.arrayBaseOffset(int[].class));
        agrees("ARRAY_LONG_BASE_OFFSET", u.arrayBaseOffset(long[].class));
        agrees("ARRAY_FLOAT_BASE_OFFSET", u.arrayBaseOffset(float[].class));
        agrees("ARRAY_DOUBLE_BASE_OFFSET", u.arrayBaseOffset(double[].class));
        agrees("ARRAY_OBJECT_BASE_OFFSET", u.arrayBaseOffset(Object[].class));
        agrees("ARRAY_BOOLEAN_INDEX_SCALE", u.arrayIndexScale(boolean[].class));
        agrees("ARRAY_BYTE_INDEX_SCALE", u.arrayIndexScale(byte[].class));
        agrees("ARRAY_SHORT_INDEX_SCALE", u.arrayIndexScale(short[].class));
        agrees("ARRAY_CHAR_INDEX_SCALE", u.arrayIndexScale(char[].class));
        agrees("ARRAY_INT_INDEX_SCALE", u.arrayIndexScale(int[].class));
        agrees("ARRAY_LONG_INDEX_SCALE", u.arrayIndexScale(long[].class));
        agrees("ARRAY_FLOAT_INDEX_SCALE", u.arrayIndexScale(float[].class));
        agrees("ARRAY_DOUBLE_INDEX_SCALE", u.arrayIndexScale(double[].class));
        agrees("ARRAY_OBJECT_INDEX_SCALE", u.arrayIndexScale(Object[].class));
        agrees("ADDRESS_SIZE", u.addressSize());

        // And the access the zeroed constants silently broke: element 2 of a
        // byte[] addressed through the CONSTANTS rather than the method.
        Long cb = constant("ARRAY_BYTE_BASE_OFFSET");
        Long cs = constant("ARRAY_BYTE_INDEX_SCALE");
        byte[] viaConst = new byte[8];
        viaConst[2] = (byte) 0x5A;
        ck("the documented protocol reads the right byte through the constants",
                cb == null || cs == null
                        || u.getByte(viaConst, cb.longValue() + 2L * cs.longValue()) == (byte) 0x5A,
                "base=" + cb + " scale=" + cs);

        System.out.println("PASS RUnsafeArrayBase (" + checks + " checks)");
    }
}
