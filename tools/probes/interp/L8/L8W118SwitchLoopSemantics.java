// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 118, lane L8: what the two wave-118 switches
// must leave unchanged.
//
//   CRATONVM_INTERP_FAST_SWITCH_ARMS (default off): `tableswitch` and
//     `lookupswitch` on the dispatch loop's fast path (forward targets).
//   CRATONVM_INTERP_FUSED_LOOP_STEP_TEST (default off): the `iinc; goto`
//     fusion runs the loop test at its target in the same dispatch.
//
// Rows:
//   dense <k>      a tableswitch over -2..9 (cases -1..7, default outside),
//                  keys from below low to above high
//   edge <k>       a tableswitch whose range touches Integer.MIN_VALUE /
//                  MAX_VALUE
//   sparse <k>     a 40-key lookupswitch (binary-search size): every key, the
//                  gaps between keys, both ends, and keys past both ends
//   small <k>      a 3-key lookupswitch
//   string         a string switch (hashCode lookupswitch, then equals, then
//                  a tableswitch), including a hash collision ("Aa" / "BB")
//   enumSwitch     a switch over an enum (the ordinal map's tableswitch)
//   loops          counted loops for every if_icmp condition, an iconst /
//                  bipush / sipush / ldc / arraylength / local bound, steps
//                  of 1, 3 and -1, a wide (iload n) counter, `while (n != 0)`,
//                  break and continue, and nested loops
//   loopCatch      a counted loop whose body throws and catches
//
// Every line must be the same with each switch off and on (and both),
// --jdk-only and --compatible, JIT and --nojit.
//
// Run: javac -d . L8W118SwitchLoopSemantics.java
//      java -cp . L8W118SwitchLoopSemantics                              (HotSpot reference)
//      cratonvm --jdk-only --nojit -cp . L8W118SwitchLoopSemantics
//      CRATONVM_INTERP_FAST_SWITCH_ARMS=1 CRATONVM_INTERP_FUSED_LOOP_STEP_TEST=1 \
//        cratonvm --jdk-only --nojit -cp . L8W118SwitchLoopSemantics
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// dense -1 -1 100 101 102 103 104 -1 106 107 108 -1 -1
// edge 1 3 0 0 3 1 0 0
// sparse 40 80 -1 -1 0 39
// small 1 2 3 0 0
// string 1 2 3 4 5 0 0
// enumSwitch 10 99 30 99
// loops 1669845 1999000
// loopCatch 1000 6000000
// HOTSPOT_EXPECTED_END
public class L8W118SwitchLoopSemantics {
    enum Color {
        RED, GREEN, BLUE, BLACK
    }

    static int dense(int k) {
        switch (k) {
            case -1:
                return 100;
            case 0:
                return 101;
            case 1:
                return 102;
            case 2:
                return 103;
            case 3:
                return 104;
            case 5:
                return 106;
            case 6:
                return 107;
            case 7:
                return 108;
            default:
                return -1;
        }
    }

    static int edgeLow(int k) {
        switch (k) {
            case Integer.MIN_VALUE:
                return 1;
            case Integer.MIN_VALUE + 1:
                return 2;
            case Integer.MIN_VALUE + 2:
                return 3;
            default:
                return 0;
        }
    }

    static int edgeHigh(int k) {
        switch (k) {
            case Integer.MAX_VALUE - 2:
                return 1;
            case Integer.MAX_VALUE - 1:
                return 2;
            case Integer.MAX_VALUE:
                return 3;
            default:
                return 0;
        }
    }

    static final int[] SPARSE_KEYS = new int[40];

    static {
        for (int i = 0; i < SPARSE_KEYS.length; i++) {
            SPARSE_KEYS[i] = i * i * 37 - 5000;
        }
    }

    static int sparse(int k) {
        switch (k) {
            case -5000: return 0;
            case -4963: return 1;
            case -4852: return 2;
            case -4667: return 3;
            case -4408: return 4;
            case -4075: return 5;
            case -3668: return 6;
            case -3187: return 7;
            case -2632: return 8;
            case -2003: return 9;
            case -1300: return 10;
            case -523: return 11;
            case 328: return 12;
            case 1253: return 13;
            case 2252: return 14;
            case 3325: return 15;
            case 4472: return 16;
            case 5693: return 17;
            case 6988: return 18;
            case 8357: return 19;
            case 9800: return 20;
            case 11317: return 21;
            case 12908: return 22;
            case 14573: return 23;
            case 16312: return 24;
            case 18125: return 25;
            case 20012: return 26;
            case 21973: return 27;
            case 24008: return 28;
            case 26117: return 29;
            case 28300: return 30;
            case 30557: return 31;
            case 32888: return 32;
            case 35293: return 33;
            case 37772: return 34;
            case 40325: return 35;
            case 42952: return 36;
            case 45653: return 37;
            case 48428: return 38;
            case 51277: return 39;
            default: return -1;
        }
    }

    static int small(int k) {
        switch (k) {
            case -1000000:
                return 1;
            case 7:
                return 2;
            case 1000000:
                return 3;
            default:
                return 0;
        }
    }

    static int string(String s) {
        switch (s) {
            case "alpha":
                return 1;
            case "beta":
                return 2;
            case "Aa":
                return 3;
            case "BB":
                return 4;
            case "":
                return 5;
            default:
                return 0;
        }
    }

    static int color(Color c) {
        switch (c) {
            case RED:
                return 10;
            case BLUE:
                return 30;
            default:
                return 99;
        }
    }

    static long loops() {
        long s = 0;
        for (int i = 0; i < 1000; i++) {
            s += i;
        }
        for (int i = 0; i <= 1000; i++) {
            s += 2;
        }
        for (int i = 1000; i > 0; i--) {
            s += 3;
        }
        for (int i = 1000; i >= 0; i -= 1) {
            s += 4;
        }
        for (int i = 0; i != 999; i += 3) {
            s += 5;
        }
        for (int i = 0; i < 100; i++) {
            s += 6;
        }
        for (int i = 0; i < 30000; i++) {
            s += 7;
        }
        for (int i = 0; i < 100_000; i++) {
            s += 8;
        }
        int[] a = new int[777];
        for (int i = 0; i < a.length; i++) {
            s += 9;
        }
        int n = 555;
        for (int i = 0; i < n; i++) {
            s += 10;
        }
        int m = 321;
        while (m != 0) {
            m--;
            s += 11;
        }
        for (int i = 0; i < 1000; i++) {
            if (i == 500) {
                break;
            }
            if ((i & 1) == 0) {
                continue;
            }
            s += 12;
        }
        for (int i = 0; i < 100; i++) {
            for (int j = 0; j < 100; j++) {
                s += 13;
            }
        }
        return s;
    }

    // The counter is local 6 (past two longs and `s`): `iload 6` loop tests.
    static long wideLoop(long p0, long p1) {
        long s = p0 + p1;
        for (int i = 0; i < 2000; i++) {
            s += i;
        }
        return s;
    }

    static int thrower(int i) {
        if (i % 4 == 0) {
            throw new IllegalArgumentException("i=" + i);
        }
        return i;
    }

    static String loopCatch() {
        int caught = 0;
        long s = 0;
        for (int i = 0; i < 4000; i++) {
            try {
                s += thrower(i);
            } catch (IllegalArgumentException e) {
                caught++;
            }
        }
        return caught + " " + s;
    }

    public static void main(String[] args) {
        // Warm everything so a JIT run also checks compiled answers.
        long warm = 0;
        for (int r = 0; r < 3000; r++) {
            warm += dense(r % 12 - 2) + sparse(SPARSE_KEYS[r % 40]) + small(r) + string("beta");
        }
        if (warm == 42) {
            System.out.println("unreachable");
        }
        StringBuilder sb = new StringBuilder("dense");
        for (int k = -3; k <= 9; k++) {
            sb.append(' ').append(dense(k));
        }
        System.out.println(sb);
        System.out.println("edge " + edgeLow(Integer.MIN_VALUE) + " " + edgeLow(Integer.MIN_VALUE + 2)
                + " " + edgeLow(Integer.MIN_VALUE + 3) + " " + edgeLow(0) + " "
                + edgeHigh(Integer.MAX_VALUE) + " " + edgeHigh(Integer.MAX_VALUE - 2) + " "
                + edgeHigh(Integer.MAX_VALUE - 3) + " " + edgeHigh(-1));
        long hits = 0;
        long misses = 0;
        for (int i = 0; i < SPARSE_KEYS.length; i++) {
            int key = SPARSE_KEYS[i];
            if (sparse(key) == i) {
                hits++;
            }
            if (sparse(key + 1) == -1) {
                misses++;
            }
            if (sparse(key - 1) == -1) {
                misses++;
            }
        }
        System.out.println("sparse " + hits + " " + misses + " " + sparse(Integer.MIN_VALUE) + " "
                + sparse(Integer.MAX_VALUE) + " " + sparse(-5000) + " " + sparse(51277));
        System.out.println("small " + small(-1000000) + " " + small(7) + " " + small(1000000) + " "
                + small(8) + " " + small(Integer.MIN_VALUE));
        System.out.println("string " + string("alpha") + " " + string("beta") + " " + string("Aa")
                + " " + string("BB") + " " + string("") + " " + string("gamma") + " "
                + string("Ab"));
        System.out.println("enumSwitch " + color(Color.RED) + " " + color(Color.GREEN) + " "
                + color(Color.BLUE) + " " + color(Color.BLACK));
        System.out.println("loops " + loops() + " " + wideLoop(0L, 0L));
        System.out.println("loopCatch " + loopCatch());
    }
}
