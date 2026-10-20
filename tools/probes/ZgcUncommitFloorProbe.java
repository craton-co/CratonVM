// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Craton Software Company
// Run with -XX:+UseZGC -Xms64m -Xmx512m and ZUncommitDelay=0 or -ZUncommit.
public class ZgcUncommitFloorProbe {
    static Object[] peak;
    public static void main(String[] args) {
        long floor = 64L << 20;
        Runtime rt = Runtime.getRuntime();
        long min = rt.totalMemory();
        boolean retain = args.length > 0 && args[0].equals("retain");
        long peakTotal = min;
        for (int round = 0; round < 5; round++) {
            peak = new Object[256];
            for (int i = 0; i < peak.length; i++) peak[i] = new byte[256 * 1024];
            peakTotal = Math.max(peakTotal, rt.totalMemory());
            peak = null;
            for (int cycle = 0; cycle < 4; cycle++) {
                System.gc();
                long total = rt.totalMemory();
                min = Math.min(min, total);
                if (retain && total < peakTotal) throw new AssertionError("premature shrink: " + total + " < " + peakTotal);
                if (total < floor) throw new AssertionError("below Xms: " + total);
            }
        }
        System.out.println("ZGC-FLOOR-OK min=" + (min >> 20) + "m peak=" + (peakTotal >> 20) + "m total=" + (rt.totalMemory() >> 20) + "m max=" + (rt.maxMemory() >> 20) + "m");
    }
}
