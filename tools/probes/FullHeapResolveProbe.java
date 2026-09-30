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
// (docs/known-issues/gc/common-w34-dev-f144bb3d2-probe-regressions-heap-oome-and-gc-notifications.md).
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
            junk = new byte[pieces][];
            try {
                for (int i = 0; i < pieces; i++) junk[i] = new byte[pieceBytes];
                // Top up with small pieces until the heap refuses.
                Object[] tail = new Object[1 << 16];
                for (int i = 0; i < tail.length; i++) tail[i] = new byte[256];
                junk[0] = null;
            } catch (OutOfMemoryError e) {
                // Heap is full now; junk is still live.
            }
            String what;
            try {
                what = site(r);
                ok++;
            } catch (OutOfMemoryError e) {
                what = "OOME";
                oome++;
            }
            junk = null;
            System.out.println("round " + r + " " + (what.length() > 40 ? what.substring(0, 40) : what));
        }
        System.out.println("linked=" + ok + " oome=" + oome + " PROBE-OK");
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
