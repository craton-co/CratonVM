/**
 * Unbox semantics, so the BOX_UNBOX intrinsic can be shown not to have broken
 * them. Run with and without CRATONVM_JIT_NO_BOX_UNBOX_INTRINSIC — the two must
 * agree, and both must agree with HotSpot.
 *
 * The values are chosen to catch the two mistakes an inline field load makes:
 * reading 4 bytes where 8 belong (or vice versa), and losing the sign when a
 * 32-bit value is widened into the 64-bit operand slot.
 */
public class BoxCorrectness {
    static int fails = 0;
    static void eq(String what, long got, long want) {
        if (got != want) { System.out.println("CK FAIL " + what + ": got " + got + " want " + want); fails++; }
    }
    static long viaLongValue(Long v) { return v.longValue(); }
    static int viaIntValue(Integer v) { return v.intValue(); }

    public static void main(String[] a) {
        long[] longs = { 0, 1, -1, 127, 128, -128, -129, Integer.MAX_VALUE, Integer.MIN_VALUE,
                         0x0123456789ABCDEFL, -0x0123456789ABCDEFL, Long.MAX_VALUE, Long.MIN_VALUE,
                         0xFFFFFFFFL, 0x100000000L, -0x100000000L };
        int[] ints = { 0, 1, -1, 127, 128, -128, -129, 65535, -65536,
                       Integer.MAX_VALUE, Integer.MIN_VALUE, 0x7FFFFFFF, -0x80000000 };

        // Warm enough that the loop bodies reach the compiled/OSR tiers, which
        // is where the intrinsic is emitted; an unwarmed run tests only the
        // interpreter and would pass whatever the JIT does.
        for (int rep = 0; rep < 200_000; rep++) {
            for (long v : longs) eq("longValue " + v, viaLongValue(v), v);
            for (int v : ints)   eq("intValue " + v,  viaIntValue(v),  v);
            if (fails > 20) break;
        }

        // Autoboxed arithmetic through the same two calls, the shape the blob
        // fixture uses: `count > 0` is longValue, `count--` is longValue+valueOf.
        Long count = 1_000_000L;
        long spins = 0;
        while (count > 0) { count--; spins++; }
        eq("boxed Long countdown", spins, 1_000_000);
        eq("boxed Long final", count, 0);

        Integer icount = 500_000;
        long ispins = 0;
        while (icount > 0) { icount--; ispins++; }
        eq("boxed Integer countdown", ispins, 500_000);
        eq("boxed Integer final", icount, 0);

        // Identity cache must be unaffected: valueOf(-128..127) is interned.
        eq("Long cache identity", (Long.valueOf(127) == Long.valueOf(127)) ? 1 : 0, 1);
        eq("Integer cache identity", (Integer.valueOf(-128) == Integer.valueOf(-128)) ? 1 : 0, 1);

        // Null receiver must still NPE, not read slot 0 of nothing.
        try { Long n = null; long x = n.longValue(); eq("null longValue did not throw", x, -999); }
        catch (NullPointerException e) { /* expected */ }
        try { Integer n = null; int x = n.intValue(); eq("null intValue did not throw", x, -999); }
        catch (NullPointerException e) { /* expected */ }

        System.out.println("CK BoxCorrectness fails=" + fails);
    }
}
