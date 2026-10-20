// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zr z2/cp (2026-10-12): the footprint of a VM that does (almost) nothing, for
// maxrss rows. `idle` boots and prints; `gc` also runs one System.gc(), so
// `maxrss(gc) - maxrss(idle)` at a large -Xmx is the capacity-proportional
// cost of the first collection
// (docs/known-issues/footprint/peak-rss-is-four-times-hotspot-20261012.md).
//
// Usage: Zr2CpBootFootprint <idle|gc>
// stdout: zr2cp-boot mode=<m> max=<maxMemory> used=<used after>
public class Zr2CpBootFootprint {
    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "idle";
        if (mode.equals("gc")) {
            System.gc();
        }
        Runtime r = Runtime.getRuntime();
        // `used` is printed for the reader, not compared: it is page-granular
        // on HotSpot ZGC (`HeapUsedDelta` census row).
        System.out.println("zr2cp-boot mode=" + mode + " max=" + r.maxMemory() + " used="
                + (r.totalMemory() - r.freeMemory()));
    }
}
