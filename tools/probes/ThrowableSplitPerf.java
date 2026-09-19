// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Splits Throwable's stack-trace cost into fillInStackTrace (new Throwable()) and StackTraceElement
// materialisation (getStackTrace), at two stack depths. Run under the default mode and --jdk-only.
// See docs/jdk-only/heap-bytebuffer-and-chm-run-interpreted-under-jdk-only-20260918.md
public class ThrowableSplitPerf {
    static long sink;
    interface Body { void run(int n); }

    static void time(String name, int n, Body b) {
        long best = Long.MAX_VALUE;
        for (int r = 0; r < 4; r++) {
            long t = System.nanoTime();
            b.run(n);
            best = Math.min(best, System.nanoTime() - t);
        }
        System.out.printf("%-44s %10.1f ns/call%n", name, best / (double) n);
    }

    static int deep(int d, Runnable r) { if (d == 0) { r.run(); return 0; } return 1 + deep(d - 1, r); }

    public static void main(String[] a) {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 50_000;
        time("new Object()  [control]", n, k -> { for (int i = 0; i < k; i++) sink += new Object().hashCode(); });
        time("new Throwable()            depth~6", n, k -> { for (int i = 0; i < k; i++) sink += new Throwable().hashCode(); });
        time("new Throwable().getStackTrace() depth~6", n, k -> { for (int i = 0; i < k; i++) sink += new Throwable().getStackTrace().length; });
        time("new Exception(\"m\")         depth~6", n, k -> { for (int i = 0; i < k; i++) sink += new Exception("m").hashCode(); });
        time("new Throwable()            depth~56", n / 5, k -> { for (int i = 0; i < k; i++) sink += deep(50, () -> sink += new Throwable().hashCode()); });
        time("new Throwable().getStackTrace() depth~56", n / 5, k -> { for (int i = 0; i < k; i++) sink += deep(50, () -> sink += new Throwable().getStackTrace().length); });
        Throwable held = new Throwable();
        time("held.getStackTrace() (cached, clone)", n, k -> { for (int i = 0; i < k; i++) sink += held.getStackTrace().length; });
        time("Thread.currentThread().getStackTrace()", n / 5, k -> { for (int i = 0; i < k; i++) sink += Thread.currentThread().getStackTrace().length; });
        System.out.println("sink=" + sink);
    }
}
