// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// gc-common w9 verification: resolve invokedynamic call sites (LambdaMetafactory:
// MethodType + MethodHandle static args; StringConcatFactory: String recipe
// constant) for the FIRST time while the heap is full. Expected: each site
// either links or throws OutOfMemoryError; the process never aborts with
// "FATAL: heap exhausted".
//
// Moved here from the round orchestrator's scratch set (`probes9/`) by
// gc-common w36-e
// (docs/internal/gc/common-w34-dev-f144bb3d2-probe-regressions-heap-oome-and-gc-notifications.md).
// The body is unchanged; line numbers are 15 higher than in the scratch copy
// (its line 17, `junk = new byte[pieces][]`, is line 32 here).
//
//   cratonvm --compatible -XX:+UseZGC -Xmx128m -cp <dir> FullHeapResolveProbe
//
// Run it on all three collectors under --compatible and --jdk-only. Expected
// on every one, and on HotSpot (Serial, G1 and ZGC, -Xmx128m):
//   linked=6 oome=0 PROBE-OK
import java.util.function.Supplier;

public class FullHeapResolveProbe {
    static byte[][] junk;

    public static void main(String[] a) {
        long max = Runtime.getRuntime().maxMemory();
        int pieces = 4096;
        int pieceBytes = (int) Math.max(1024, (max * 9 / 10) / pieces);
        int ok = 0, oome = 0;
        for (int r = 0; r < 6; r++) {
            try {
                // The outer array can itself fail after a preceding full-heap
                // round. Keep it inside the catch just like the fill below.
                junk = new byte[pieces][];
                for (int i = 0; i < pieces; i++) junk[i] = new byte[pieceBytes];
                // Top up with small pieces until the heap refuses.
                Object[] tail = new Object[1 << 16];
                for (int i = 0; i < tail.length; i++) tail[i] = new byte[256];
                junk[0] = null;
            } catch (OutOfMemoryError e) {
                // Heap is full now; junk is still live.
            }
            try {
                site(r);
                ok++;
            } catch (OutOfMemoryError e) {
                oome++;
            }
            junk = null;
        }
        boolean passed = ok + oome == 6;
        // A site may legitimately link or throw a caught OOME while the fill
        // remains live. Keep reporting allocation-free because the heap may
        // still be full after the last site.
        if (!passed) {
            System.out.println("PROBE-FAIL");
            System.exit(1);
        }
        System.out.println(switch (oome) {
            case 0 -> "linked=6 oome=0 PROBE-OK";
            case 1 -> "linked=5 oome=1 PROBE-OK";
            case 2 -> "linked=4 oome=2 PROBE-OK";
            case 3 -> "linked=3 oome=3 PROBE-OK";
            case 4 -> "linked=2 oome=4 PROBE-OK";
            case 5 -> "linked=1 oome=5 PROBE-OK";
            default -> "linked=0 oome=6 PROBE-OK";
        });
    }

    static String site(int r) {
        long t = System.nanoTime();
        switch (r) {
            case 0: { Supplier<String> s = () -> "lambda-0"; return s.get(); }
            case 1: { Supplier<String> s = () -> "lambda-1:" + t; return s.get(); }
            case 2: return "concat-2 " + t + " end";
            case 3: { java.util.function.Function<Integer, String> f = Integer::toHexString; return f.apply(r); }
            case 4: return "concat-4 [" + r + "] " + (t & 7);
            default: { Runnable run = () -> {}; run.run(); return "runnable-" + r; }
        }
    }
}
