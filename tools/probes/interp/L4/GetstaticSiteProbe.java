// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 4, lane L4: quickened `getstatic` sites
// (JvmThread::static_field_sites, field_fast::getstatic_site_hit). A site is
// filled after its first ordinary execution when the declaring class is
// initialized; later executions skip resolution and the initialization check.
// This probe checks the quickened reads are indistinguishable from the slow
// path: values written by putstatic in between are seen, J/D statics stay
// bit-exact, a failed <clinit> keeps throwing NoClassDefFoundError (a failed
// class never fills a site), a lazily initialized class runs <clinit> exactly
// once and in order, and a <clinit> that loops over its own statics works.
//
// Run it with and without --nojit, and with CRATONVM_JIT_NO_FIELD_FAST_PATH=1
// (arm off) for the A/B; stdout must be identical in all three and to HotSpot.
// Timing goes to stderr: "getstatic loop: N ns/iter" (interpreter cost of the
// hot loop body, ~4 getstatics + arithmetic; compare arm on/off under --nojit).
//
// Expected on HotSpot 25:
//   I=7 L=fffc000012345678 D=fff8000000000001 S=hello N=null
//   Z=true B=-3 C=65 SH=-2
//   loop sum=36219860 long=fffc00000000021c
//   before Lazy
//   Lazy.<clinit>
//   Lazy.V=42 again=42
//   Table.SUM=45 Table.T[9]=109
//   Bad #0: java.lang.ExceptionInInitializerError
//   Bad #1: java.lang.NoClassDefFoundError
//   Bad #2: java.lang.NoClassDefFoundError
//   Bad #3: java.lang.NoClassDefFoundError
//   Boolean.TRUE canonical=true FALSE canonical=true
//   Iface.OBJ=iface-obj
//   done
public class GetstaticSiteProbe {
    static int I = 7;
    static long L = 0xFFFC_0000_1234_5678L;
    static double D = Double.longBitsToDouble(0xFFF8_0000_0000_0001L);
    static String S = "hello";
    static Object N = null;
    static boolean Z = true;
    static byte B = -3;
    static char C = 'A';
    static short SH = -2;

    static int counter;
    static long acc = 0xFFFC_0000_0000_0000L;

    static class Lazy {
        static int V;
        static {
            System.out.println("Lazy.<clinit>");
            V = 42;
        }
    }

    static class Table {
        static final int BASE = Integer.parseInt("100");
        static int[] T = new int[10];
        static int SUM;
        static {
            // <clinit> reading and writing its own statics in a loop: the
            // class is not initialized yet, so these sites must not fill.
            for (int i = 0; i < T.length; i++) {
                T[i] = BASE + i;
                SUM += i;
            }
        }
    }

    static class Bad {
        static int X;
        static {
            if (Integer.parseInt("1") == 1) {
                throw new IllegalStateException("boom");
            }
            X = 1;
        }
    }

    interface Iface {
        Object OBJ = new StringBuilder("iface-").append("obj").toString();
    }

    static void bump(int i) {
        counter += i;
        acc += 5;
    }

    static int readBad() {
        return Bad.X;
    }

    public static void main(String[] a) {
        System.out.println("I=" + I + " L=" + Long.toHexString(L)
                + " D=" + Long.toHexString(Double.doubleToRawLongBits(D))
                + " S=" + S + " N=" + N);
        System.out.println("Z=" + Z + " B=" + B + " C=" + (int) C + " SH=" + SH);

        long sum = 0;
        long t0 = System.nanoTime();
        int iters = 110_000;
        for (int i = 0; i < iters; i++) {
            // Four getstatic sites per iteration; `counter` and `acc` change
            // underneath them through putstatic in `bump`.
            sum += I + counter;
            if ((i & 1023) == 0) {
                bump(1);
            }
            sum += (acc & 0xFFFF) + B;
        }
        long t1 = System.nanoTime();
        System.out.println("loop sum=" + sum + " long=" + Long.toHexString(acc));
        System.err.println("getstatic loop: " + ((t1 - t0) / iters) + " ns/iter");

        System.out.println("before Lazy");
        int v = Lazy.V;
        int again = 0;
        for (int i = 0; i < 1000; i++) {
            again = Lazy.V;
        }
        System.out.println("Lazy.V=" + v + " again=" + again);

        System.out.println("Table.SUM=" + Table.SUM + " Table.T[9]=" + Table.T[9]);

        for (int i = 0; i < 4; i++) {
            try {
                readBad();
                System.out.println("Bad #" + i + ": no exception");
            } catch (Throwable t) {
                System.out.println("Bad #" + i + ": " + t.getClass().getName());
            }
        }

        boolean tCanon = Boolean.TRUE == Boolean.valueOf(true);
        boolean fCanon = Boolean.FALSE == Boolean.valueOf(false);
        for (int i = 0; i < 1000; i++) {
            tCanon &= Boolean.TRUE == Boolean.valueOf(true);
        }
        System.out.println("Boolean.TRUE canonical=" + tCanon + " FALSE canonical=" + fCanon);

        Object o = null;
        for (int i = 0; i < 100; i++) {
            o = Iface.OBJ;
        }
        System.out.println("Iface.OBJ=" + o);
        System.out.println("done");
    }
}
