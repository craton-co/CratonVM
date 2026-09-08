// Long.longValue()J / Integer.intValue()I lowered by the OPTIMIZING tier as a
// guarded field load (null check, exact class guard, per-object compact/legacy
// layout branch, payload load) instead of refusing the whole method.
//
// The layout branch is the reason this family sat on the refusal list, so the
// probe deliberately mixes boxes from DIFFERENT allocation paths: values inside
// the Integer/Long cache (-128..127) come from a preallocated table, values
// outside it are freshly allocated per box. If the compact/legacy branch were
// wrong for either shape, one of these two groups returns garbage.
//
// Every answer is checked against a value computed WITHOUT the accessor, so the
// probe cannot pass by agreeing with itself. Run under HotSpot for the oracle.
public class UnboxIntrinsicProbe {
    static final int WARM = 200_000;

    // The accessor under test is the implicit longValue()/intValue() in the
    // unboxing conversions below.
    static long sumLong(Long[] xs) {
        long s = 0;
        for (Long x : xs) s += x;          // Long.longValue()
        return s;
    }

    static int sumInt(Integer[] xs) {
        int s = 0;
        for (Integer x : xs) s += x;       // Integer.intValue()
        return s;
    }

    static long oneLong(Long x) { return x; }
    static int  oneInt(Integer x) { return x; }

    public static void main(String[] args) {
        boolean ok = true;

        // Mixed cache-resident and freshly-allocated boxes.
        Long[] ls = new Long[] {
            0L, 1L, -1L, 127L, -128L,              // cached range
            128L, -129L, 1_000_000L, -1_000_000L,  // fresh allocations
            Long.MAX_VALUE, Long.MIN_VALUE,
            (long) Integer.MAX_VALUE + 1L, (long) Integer.MIN_VALUE - 1L,
        };
        Integer[] is = new Integer[] {
            0, 1, -1, 127, -128,
            128, -129, 1_000_000, -1_000_000,
            Integer.MAX_VALUE, Integer.MIN_VALUE,
        };

        long wantL = 0; for (Long x : ls) wantL += x.longValue();
        int  wantI = 0; for (Integer x : is) wantI += x.intValue();

        long gotL = 0; int gotI = 0;
        for (int i = 0; i < WARM; i++) { gotL = sumLong(ls); gotI = sumInt(is); }

        if (gotL != wantL) { ok = false; System.out.println("FAIL sumLong " + gotL + " != " + wantL); }
        if (gotI != wantI) { ok = false; System.out.println("FAIL sumInt " + gotI + " != " + wantI); }

        // Per-value identity: every element must round-trip exactly.
        for (Long x : ls) {
            long got = 0;
            for (int i = 0; i < 2000; i++) got = oneLong(x);
            if (got != x.longValue()) { ok = false; System.out.println("FAIL oneLong " + x); }
        }
        for (Integer x : is) {
            int got = 0;
            for (int i = 0; i < 2000; i++) got = oneInt(x);
            if (got != x.intValue()) { ok = false; System.out.println("FAIL oneInt " + x); }
        }

        // A null receiver must still raise NPE, not read offset 0 of nothing.
        try {
            Long n = null;
            for (int i = 0; i < 2000; i++) oneLong(n);
            ok = false; System.out.println("FAIL null Long did not throw");
        } catch (NullPointerException expected) { }
        try {
            Integer n = null;
            for (int i = 0; i < 2000; i++) oneInt(n);
            ok = false; System.out.println("FAIL null Integer did not throw");
        } catch (NullPointerException expected) { }

        System.out.println("SUMS " + gotL + " " + gotI);
        System.out.println("UNBOX INTRINSIC PROBE " + (ok ? "OK" : "FAILED"));
    }
}
