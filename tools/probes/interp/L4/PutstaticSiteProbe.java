// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 5, lane L4: quickened `putstatic` sites and
// volatile `getstatic` sites (field_fast::putstatic_site_hit; one
// JvmThread::static_field_sites entry serves both opcodes). A site fills after
// its first ordinary execution once the declaring class is initialized; later
// stores skip resolution, the final-field check and the initialization check.
// This probe checks the quickened stores are indistinguishable from the slow
// path: every primitive kind round-trips (J/D bit-exact), stores from another
// class's code land in the holder, a reference static goes null -> object ->
// null, a volatile flag hands a value from one thread to another, a final
// static written in <clinit> reads back, and a store racing nothing but its own
// loop keeps the arithmetic exact.
//
// Run it with and without --nojit, and with CRATONVM_JIT_NO_FIELD_FAST_PATH=1
// (arm off) for the A/B; stdout must be identical in all three and to HotSpot.
// With CRATONVM_DBG_GETSTATIC_PROF=1 the exit line `[GS_PROF] ... | interp_site
// get_hit=N put_hit=N fill=N` (stderr) must show non-zero put_hit and get_hit
// under --nojit. Timing goes to stderr: "putstatic loop: N ns/iter".
//
// Expected on HotSpot 25:
//   I=1000000 J=fffc0000000f4240 F=1000000.0 D=fff80000000f4240
//   Z=true B=64 C=16960 S=16960
//   Other.X=1000000 Other.R=odd
//   ref: null -> probe -> null
//   volatile handoff: 12345 after 1 flag flips
//   Fin.K=77 Fin.K2=154
//   counter=3000000
//   done
public class PutstaticSiteProbe {
    static int I;
    static long J = 0xFFFC_0000_0000_0000L;
    static float F;
    static double D;
    static boolean Z;
    static byte B;
    static char C;
    static short S;
    static Object REF;
    static final String[] NAMES = {"even", "odd"};

    static volatile boolean READY;
    static volatile int PAYLOAD;
    static int counter;

    static class Other {
        static int X;
        static String R;
    }

    static class Fin {
        static final int K;
        static final int K2;
        static {
            K = Integer.parseInt("77");
            K2 = K * 2;
        }
    }

    static void storeAll(int n) {
        for (int i = 0; i < n; i++) {
            I = i + 1;
            J = 0xFFFC_0000_0000_0000L | (i + 1);
            F = (float) (i + 1);
            D = Double.longBitsToDouble(0xFFF8_0000_0000_0000L | (i + 1));
            Z = (i & 1) == 1;
            B = (byte) (i + 1);
            C = (char) (i + 1);
            S = (short) (i + 1);
            Other.X = i + 1;
            Other.R = NAMES[i & 1];
        }
    }

    static void bump(int n) {
        for (int i = 0; i < n; i++) {
            counter = counter + 1;
        }
    }

    public static void main(String[] args) throws Exception {
        int n = 1_000_000;
        storeAll(n);
        System.out.println("I=" + I + " J=" + Long.toHexString(J) + " F=" + F
                + " D=" + Long.toHexString(Double.doubleToRawLongBits(D)));
        System.out.println("Z=" + Z + " B=" + B + " C=" + (int) C + " S=" + S);
        System.out.println("Other.X=" + Other.X + " Other.R=" + Other.R);

        StringBuilder sb = new StringBuilder("ref: ");
        for (int round = 0; round < 3; round++) {
            REF = round == 1 ? "probe" : null;
            sb.append(REF);
            if (round < 2) {
                sb.append(" -> ");
            }
        }
        System.out.println(sb);

        // Volatile handoff: the writer publishes PAYLOAD then READY; the reader
        // spins on READY (a volatile getstatic site) and must see PAYLOAD.
        READY = false;
        int[] flips = new int[1];
        Thread reader = new Thread(() -> {
            while (!READY) {
                Thread.onSpinWait();
            }
            flips[0]++;
            System.out.println("volatile handoff: " + PAYLOAD + " after " + flips[0]
                    + " flag flips");
        });
        reader.start();
        for (int i = 0; i < 10_000; i++) {
            PAYLOAD = i; // warm the volatile putstatic site
        }
        PAYLOAD = 12345;
        READY = true;
        reader.join();

        System.out.println("Fin.K=" + Fin.K + " Fin.K2=" + Fin.K2);

        long t0 = System.nanoTime();
        bump(n);
        bump(n);
        bump(n);
        long t1 = System.nanoTime();
        System.out.println("counter=" + counter);
        System.err.println("putstatic loop: " + ((t1 - t0) / (3.0 * n)) + " ns/iter");
        System.out.println("done");
    }
}
