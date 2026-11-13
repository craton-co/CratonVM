// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// j27 wave 2 (lane TCARRAY): page
// docs/known-issues/jit/j27-typecheck-array-arm-and-array-miss-switches-are-dead-20261028.md
// `o instanceof T[]` over plain objects, primitive arrays, reference arrays and null.
// Facts are deterministic; timing lines only with -Dj27.time=true.
public class J27TcarrayMiss {
    static final Object[] RECEIVERS = {
        "s", new Object(), new int[1], new String[1], new Integer[1],
        new Object[1], new String[1][1], null, new CharSequence[1],
    };
    static final String[] NAMES = {
        "string", "object", "int-array", "string-array", "integer-array",
        "object-array", "string-array-2d", "null", "charseq-array",
    };

    static int kernel(Object o) {
        int r = 0;
        if (o instanceof String[]) r |= 1;
        if (o instanceof Object[]) r |= 2;
        if (o instanceof CharSequence[]) r |= 4;
        if (o instanceof Number[]) r |= 8;
        return r;
    }

    static long sweep() {
        long s = 0;
        for (Object o : RECEIVERS) s += kernel(o);
        return s;
    }

    public static void main(String[] args) {
        boolean time = Boolean.getBoolean("j27.time");
        long total = 0;
        for (int w = 0; w < 200_000; w++) total += sweep();
        System.out.println("fact total " + total);
        for (int i = 0; i < RECEIVERS.length; i++) {
            System.out.println("fact " + NAMES[i] + " " + kernel(RECEIVERS[i]));
        }
        if (time) {
            Object plain = RECEIVERS[1];
            int reps = 2_000_000;
            long sink = 0;
            long t0 = System.nanoTime();
            for (int i = 0; i < reps; i++) sink += kernel(plain);
            System.out.println("time plain-miss " + (System.nanoTime() - t0) * 1000 / reps + " ps/4-checks");
            if (sink == 42) System.out.println("unlikely");
        }
    }
}
