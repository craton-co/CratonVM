// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 100, lane L6 -- page
// docs/internal/fixed-bugs/interpreter-i91-L6-virtual-thread-code-is-never-compiled-FIXED-20261017.md:
// back-edge OSR on a JDK virtual thread for a frame whose method cannot reach
// a yield (CRATONVM_VTHREAD_COMPILED_LEAF_OSR, default off). Each leaf below
// is called ONCE per virtual thread with a loop long enough to cross the OSR
// threshold, so only OSR (not the invocation tier-up) can compile it there.
// The output must be the same with the switch off and on, both modes, JIT and
// --nojit:
//   cratonvm L6W100VirtualThreadLeafOsr
//   CRATONVM_VTHREAD_COMPILED_LEAF_OSR=1 cratonvm L6W100VirtualThreadLeafOsr
//   CRATONVM_VTHREAD_COMPILED_LEAF_OSR=1 cratonvm --compatible L6W100VirtualThreadLeafOsr
// With CRATONVM_DBG_JITC=1 and the switch on, an OSR compile of `spin`,
// `index` and `div` should be requested from the virtual rows.
//
// HOTSPOT_EXPECTED_BEGIN
// spin same=true
// aioobe: java.lang.ArrayIndexOutOfBoundsException: Index 2000000 out of bounds for length 1024 at index
// div: java.lang.ArithmeticException: / by zero at div
// sleep-after-loop: ok
// parallel: same=true
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.atomic.AtomicReference;

public class L6W100VirtualThreadLeafOsr {
    // Leaves: no invoke, no monitor op, not synchronized, short Code.
    static long spin(int n, int seed) {
        long s = seed;
        for (int i = 0; i < n; i++) {
            s = s * 31 + (i ^ (s >>> 7));
        }
        return s;
    }

    static int index(int[] a, int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += a[i & 1023];
        }
        return s + a[n];
    }

    static int div(int n) {
        int s = 0;
        for (int i = n; i > -5; i--) {
            s += 1000 / i;
        }
        return s;
    }

    static String describe(Throwable t) {
        StackTraceElement[] st = t.getStackTrace();
        String top = st.length == 0 ? "?" : st[0].getMethodName();
        return t + " at " + top;
    }

    static <T> T onVirtual(java.util.concurrent.Callable<T> c) throws Exception {
        AtomicReference<Object> out = new AtomicReference<>();
        Thread t = Thread.ofVirtual().start(() -> {
            try {
                out.set(c.call());
            } catch (Throwable e) {
                out.set(e);
            }
        });
        t.join();
        Object o = out.get();
        if (o instanceof Exception e) {
            throw e;
        }
        if (o instanceof Error e) {
            throw e;
        }
        @SuppressWarnings("unchecked")
        T v = (T) o;
        return v;
    }

    public static void main(String[] args) throws Exception {
        long v = onVirtual(() -> spin(3_000_000, 7));
        long p = spin(3_000_000, 7);
        System.out.println("spin same=" + (v == p));

        int[] a = new int[1024];
        for (int i = 0; i < a.length; i++) {
            a[i] = i;
        }
        try {
            onVirtual(() -> index(a, 2_000_000));
            System.out.println("aioobe: none");
        } catch (ArrayIndexOutOfBoundsException e) {
            System.out.println("aioobe: " + describe(e));
        }

        try {
            onVirtual(() -> div(2_000_000));
            System.out.println("div: none");
        } catch (ArithmeticException e) {
            System.out.println("div: " + describe(e));
        }

        String slept = onVirtual(() -> {
            long s = spin(2_000_000, 3);
            Thread.sleep(1);
            long t = spin(2_000_000, 3);
            return s == t ? "ok" : "differs";
        });
        System.out.println("sleep-after-loop: " + slept);

        // Eight virtual threads in their leaf loops at once.
        long[] got = new long[8];
        Thread[] ts = new Thread[8];
        for (int k = 0; k < 8; k++) {
            final int kk = k;
            ts[k] = Thread.ofVirtual().start(() -> got[kk] = spin(1_500_000, kk));
        }
        boolean same = true;
        for (int k = 0; k < 8; k++) {
            ts[k].join();
            same &= got[k] == spin(1_500_000, k);
        }
        System.out.println("parallel: same=" + same);
        System.out.println("done");
    }
}
