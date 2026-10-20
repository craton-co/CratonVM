// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i6 wave 118, lane L8: two switches, each default off.
//
//   CRATONVM_INTERP_FAST_SWITCH_ARMS      `tableswitch` / `lookupswitch` on the
//     fast path instead of the decoded path (SWITCH rows)
//   CRATONVM_INTERP_FUSED_LOOP_STEP_TEST  `iinc; goto` also runs the loop test
//     at its target: one dispatch of loop overhead per javac `for` trip
//     instead of two (LOOP rows)
//
// Rows:
//   dense        switch (i & 7), 8 cases: tableswitch       SWITCH (+ LOOP)
//   sparse       switch over 16 sparse keys: lookupswitch  SWITCH (+ LOOP)
//   strings      switch over 6 strings                     SWITCH (+ LOOP)
//   forLoop      for (i < n) s += i                        LOOP
//   forArray     for (i < a.length) s += a[i]              LOOP
//   whileZero    while (n != 0) { s += n; n--; }           LOOP (ifeq at top,
//                                                          with the default-on
//                                                          zero-branch fusion)
//   control      do { s += i; i++; } while (i < n)         neither (no goto)
// `control` should be flat in every arm.
//
// Every row runs REPS timed repetitions after one untimed warm-up and
// reports the MINIMUM. Compare arms on the SAME binary, interleaved, medians
// of the per-round minima. On the Windows dev box pin to the P-cores
// (logical CPUs 0-15), or an E-core run reads 1.8-2.8x slower
// (docs/known-issues/interpreter/gap-hybrid-cpu-e-core-scheduling-halves-interpreter-timings-20261018.md):
//   start /affinity FFFF /b /wait cratonvm --java-home <jdk25> --nojit -cp <dir> L8W118SwitchLoopBench
// An optional argument scales every row (`L8W118SwitchLoopBench 100` for a
// JIT run, so each TIME line is milliseconds); the checksums below are for
// scale 1.
//
// Run (arms):
//   cratonvm --java-home <jdk25> --nojit -cp <dir> L8W118SwitchLoopBench
//   CRATONVM_INTERP_FAST_SWITCH_ARMS=1 cratonvm ... L8W118SwitchLoopBench
//   CRATONVM_INTERP_FUSED_LOOP_STEP_TEST=1 cratonvm ... L8W118SwitchLoopBench
//   (both) and once without --nojit, scale 100 (JIT arm: expected flat)
// stdout: per row one deterministic `<row> checksum=<n>` line and one
// `TIME <row> <x> ns/op <y> us` line (not compared). About 2-3 s under --nojit.
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// dense checksum=2850000
// sparse checksum=2550000
// strings checksum=187500
// forLoop checksum=44999850000
// forArray checksum=153434128
// whileZero checksum=45000150000
// control checksum=44999850000
// HOTSPOT_EXPECTED_END
public class L8W118SwitchLoopBench {
    interface Row {
        long run();
    }

    static final int REPS = 5;
    static final int ITERS = 300_000;
    static int scale = 1;
    static int loops = ITERS;

    static final int[] SPARSE = {3, 17, 99, 250, 1001, 4096, 7777, 10000, 12345, 20000, 33333,
        40000, 50505, 65536, 77777, 99999};
    static final String[] WORDS = {"get", "put", "delete", "head", "options", "other"};
    static int[] array;

    static long dense() {
        long s = 0;
        int n = loops;
        for (int i = 0; i < n; i++) {
            switch (i & 7) {
                case 0: s += 1; break;
                case 1: s += 3; break;
                case 2: s += 5; break;
                case 3: s += 7; break;
                case 4: s += 11; break;
                case 5: s += 13; break;
                case 6: s += 17; break;
                default: s += 19; break;
            }
        }
        return s;
    }

    static int sparseCase(int k) {
        switch (k) {
            case 3: return 1;
            case 17: return 2;
            case 99: return 3;
            case 250: return 4;
            case 1001: return 5;
            case 4096: return 6;
            case 7777: return 7;
            case 10000: return 8;
            case 12345: return 9;
            case 20000: return 10;
            case 33333: return 11;
            case 40000: return 12;
            case 50505: return 13;
            case 65536: return 14;
            case 77777: return 15;
            case 99999: return 16;
            default: return 0;
        }
    }

    static long sparse() {
        long s = 0;
        int[] keys = SPARSE;
        int n = loops;
        for (int i = 0; i < n; i++) {
            s += sparseCase(keys[i & 15]);
        }
        return s;
    }

    static int verb(String w) {
        switch (w) {
            case "get": return 1;
            case "put": return 2;
            case "delete": return 3;
            case "head": return 4;
            case "options": return 5;
            default: return 0;
        }
    }

    static long strings() {
        long s = 0;
        String[] words = WORDS;
        int n = loops / 4;
        for (int i = 0; i < n; i++) {
            s += verb(words[i % 6]);
        }
        return s;
    }

    static long forLoop() {
        long s = 0;
        int n = loops;
        for (int i = 0; i < n; i++) {
            s += i;
        }
        return s;
    }

    static long forArray() {
        long s = 0;
        int[] a = array;
        for (int r = 0; r < scale; r++) {
            for (int i = 0; i < a.length; i++) {
                s += a[i];
            }
        }
        return s;
    }

    static long whileZero() {
        long s = 0;
        int n = loops;
        while (n != 0) {
            s += n;
            n--;
        }
        return s;
    }

    static long control() {
        long s = 0;
        int n = loops;
        int i = 0;
        do {
            s += i;
            i++;
        } while (i < n);
        return s;
    }

    static void time(String name, Row row) {
        long checksum = row.run(); // untimed warm-up
        long best = Long.MAX_VALUE;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            long c = row.run();
            long dt = System.nanoTime() - t0;
            if (dt < best) {
                best = dt;
            }
            if (c != checksum) {
                checksum = -1;
            }
        }
        System.out.println(name + " checksum=" + checksum);
        System.out.printf(java.util.Locale.ROOT, "TIME %-10s %8.2f ns/op %10.1f us (min of %d, scale %d)%n",
                name, (double) best / loops, best / 1000.0, REPS, scale);
    }

    public static void main(String[] args) {
        if (args.length > 0) {
            scale = Integer.parseInt(args[0]);
            loops = ITERS * scale;
        }
        array = new int[ITERS];
        for (int i = 0; i < ITERS; i++) {
            array[i] = i & 1023;
        }
        time("dense", L8W118SwitchLoopBench::dense);
        time("sparse", L8W118SwitchLoopBench::sparse);
        time("strings", L8W118SwitchLoopBench::strings);
        time("forLoop", L8W118SwitchLoopBench::forLoop);
        time("forArray", L8W118SwitchLoopBench::forArray);
        time("whileZero", L8W118SwitchLoopBench::whileZero);
        time("control", L8W118SwitchLoopBench::control);
    }
}
