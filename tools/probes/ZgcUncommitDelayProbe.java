// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Craton Software Company
// Run with -XX:+UseZGC -Xms64m -Xmx512m -XX:ZUncommitDelay=2.
public class ZgcUncommitDelayProbe {
    static Object[] peak;
    public static void main(String[] args) throws Exception {
        Runtime rt = Runtime.getRuntime();
        peak = new Object[256];
        for (int i = 0; i < peak.length; i++) peak[i] = new byte[256 * 1024];
        long peakTotal = rt.totalMemory();
        peak = null;
        System.gc();
        System.gc();
        long before = rt.totalMemory();
        if (before < peakTotal) throw new AssertionError("shrank before delay: " + before);
        long after = before;
        // HotSpot uncommits on its service thread; CratonVM evaluates at GC.
        // Allow that service to run without repeatedly recycling free pages.
        for (int attempt = 0; attempt < 10 && after >= before; attempt++) {
            Thread.sleep(3000);
            System.gc();
            after = rt.totalMemory();
        }
        if (after < (64L << 20) || after >= before)
            throw new AssertionError("delayed give-back missing or below Xms: " + before + " -> " + after);
        System.out.println("ZGC-DELAY-OK before=" + (before >> 20) + "m after=" + (after >> 20) + "m");
    }
}
