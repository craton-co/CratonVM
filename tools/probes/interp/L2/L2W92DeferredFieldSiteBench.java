// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench -- Interpreter round i4 wave 92, lane L2. A hot method whose COLD
// branch touches a field of a class that is not loaded when the method gets
// hot. The compile doors resolve every field site at compile time and refuse
// the WHOLE method on one they cannot resolve (`unresolved-field-site` /
// `static_field_resolve` / `field_resolve`), so before
// `CRATONVM_JIT_DEFERRED_FIELD_SITES` such a method never compiled; with it the
// cold site compiles as a deferred site (`DirectHelperTable::field_site_cp`)
// and the hot path runs compiled. Page:
// docs/internal/fixed-bugs/interpreter-i38-L2-proposal-constant-pool-indexed-field-sites-DONE-20261127.md
// ("Wave 92").
//
// Rows (ns per call, median of 7 interleaved reps; NOT deterministic):
//   static-cold    `step` with a cold `getstatic` + `putstatic` of an unloaded class
//   field-cold     `stepField` with a cold `new` + `putfield` + `getfield`
//   control        `stepControl`, the same body with the cold site's class
//                  loaded before warm-up (the compiled ceiling)
// With the switch OFF static-cold / field-cold sit at interpreter speed and
// control well below them; with it ON all three should be within noise of
// each other.
//
// A/B (one binary, one variable; interleave the arms, take medians):
//   $J/javac -d out tools/probes/interp/L2/L2W92DeferredFieldSiteBench.java
//   $B --java-home $J/.. -cp out L2W92DeferredFieldSiteBench
//   CRATONVM_JIT_DEFERRED_FIELD_SITES=1 $B --java-home $J/.. -cp out L2W92DeferredFieldSiteBench
//   (each with --compatible too)
//   $J/java -cp out L2W92DeferredFieldSiteBench    (reference)
// Census (switch ON): CRATONVM_DBG_JITC=1 prints
//   [cratonvm-jitc] deferred-field-sites L2W92DeferredFieldSiteBench.step(II)I: 2 site(s) ...
//   [cratonvm-jitc] deferred-field-sites L2W92DeferredFieldSiteBench.stepField(II)I: 2 site(s) ...
// (switch OFF: `resolver-bail site=static_field_resolve` / `field_resolve`
// for the same two methods instead).
// The last lines are deterministic:
//   check-static 1105
//   check-field 1113
public class L2W92DeferredFieldSiteBench {
    static final int CALLS = 200_000;
    static final int REPS = 7;

    static int step(int i, int cold) {
        int a = i;
        for (int k = 0; k < 16; k++) {
            a = a * 31 + k;
            a ^= a >>> 7;
        }
        if (cold == 1) {
            L2W92BenchColdStatic.seen = a;
            return L2W92BenchColdStatic.BASE + i;
        }
        return a;
    }

    static int stepField(int i, int cold) {
        int a = i;
        for (int k = 0; k < 16; k++) {
            a = a * 31 + k;
            a ^= a >>> 7;
        }
        if (cold == 1) {
            L2W92BenchColdBox b = new L2W92BenchColdBox();
            b.v = 1100 + i;
            return b.v;
        }
        return a;
    }

    static int stepControl(int i, int cold) {
        int a = i;
        for (int k = 0; k < 16; k++) {
            a = a * 31 + k;
            a ^= a >>> 7;
        }
        if (cold == 1) {
            L2W92BenchWarmStatic.seen = a;
            return L2W92BenchWarmStatic.BASE + i;
        }
        return a;
    }

    static long timeStatic() {
        int sink = 0;
        long t0 = System.nanoTime();
        for (int i = 0; i < CALLS; i++) {
            sink += step(i, 0);
        }
        long t = System.nanoTime() - t0;
        if (sink == 7) {
            System.out.println("!");
        }
        return t;
    }

    static long timeField() {
        int sink = 0;
        long t0 = System.nanoTime();
        for (int i = 0; i < CALLS; i++) {
            sink += stepField(i, 0);
        }
        long t = System.nanoTime() - t0;
        if (sink == 7) {
            System.out.println("!");
        }
        return t;
    }

    static long timeControl() {
        int sink = 0;
        long t0 = System.nanoTime();
        for (int i = 0; i < CALLS; i++) {
            sink += stepControl(i, 0);
        }
        long t = System.nanoTime() - t0;
        if (sink == 7) {
            System.out.println("!");
        }
        return t;
    }

    static long median(long[] xs) {
        long[] c = xs.clone();
        java.util.Arrays.sort(c);
        return c[c.length / 2];
    }

    public static void main(String[] args) {
        // The control's cold class is loaded and initialised up front.
        L2W92BenchWarmStatic.seen = 0;
        long[] s = new long[REPS];
        long[] f = new long[REPS];
        long[] c = new long[REPS];
        for (int r = 0; r < REPS; r++) {
            s[r] = timeStatic();
            f[r] = timeField();
            c[r] = timeControl();
        }
        System.out.println("static-cold " + median(s) / CALLS + " ns/call");
        System.out.println("field-cold " + median(f) / CALLS + " ns/call");
        System.out.println("control " + median(c) / CALLS + " ns/call");
        System.out.println("check-static " + step(5, 1));
        System.out.println("check-field " + stepField(13, 1));
    }
}

class L2W92BenchColdStatic {
    static int seen;
    static int BASE;

    static {
        BASE = 1100;
    }
}

class L2W92BenchWarmStatic {
    static int seen;
    static int BASE;

    static {
        BASE = 1100;
    }
}

class L2W92BenchColdBox {
    int v;
}
