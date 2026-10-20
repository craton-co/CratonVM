// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zr z2/cp (2026-10-12): how much LIVE data fits under -Xmx before the first
// OutOfMemoryError, per allocation shape. Separates the causes behind
// ProxyBoxingOomeProbe fitting 39 % of HotSpot ZGC's live set:
//
//   dense     int[64] retained back to back, no garbage, refs held in ONE
//             preallocated Object[] (no ArrayList growth): per-object size,
//             side tables, the large-object reserve. No fragmentation possible.
//   mosaic    int[64] retained with ~1.5x its size of short-lived garbage
//             allocated between (the ProxyBoxing shape) and refs held in ONE
//             preallocated Object[]: fragmentation that only small requests see.
//   grow      mosaic, but refs held in an ArrayList that grows by 1.5x: every
//             growth needs one CONTIGUOUS block (the ProxyBoxing failure:
//             a 4.3 MB Object[] on a heap 60 % free in 128-512 byte holes).
//   dense16k  byte[16384] retained with 2 KiB garbage between (the GchMj3
//             shape), refs in an ArrayList: a mosaic at ~87 % page density.
//   large     byte[1 MiB] retained, nothing else: the large-object end alone.
//
// One stdout line per shape:
//   zr2cp shape=<s> retained=<n> live_mb=<payload MiB retained> oom=<bool>
// live_mb counts element payload only (n * element bytes), the same on every
// JVM, so the HotSpot twin's line is directly comparable. The pass criterion
// is a ratio against the twin, not equality (object layouts differ).
//
// Usage: Zr2CpCapacity <shape|all>   (run at -Xmx256m; each shape in its own
// JVM is the clean reading, `all` is a convenience).
import java.util.ArrayList;
import java.util.List;

public class Zr2CpCapacity {
    static volatile Object sink;

    public static void main(String[] args) {
        String which = args.length > 0 ? args[0] : "all";
        String[] shapes = {"dense", "mosaic", "grow", "dense16k", "large"};
        for (String s : shapes) {
            if (which.equals("all") || which.equals(s)) {
                run(s);
                System.gc();
            }
        }
    }

    static void run(String shape) {
        long payload = 0;
        int n = 0;
        boolean oom = false;
        Object[] fixed = null;
        List<Object> grow = null;
        try {
            switch (shape) {
                case "dense": {
                    fixed = new Object[2_000_000];
                    for (int i = 0; i < fixed.length; i++) {
                        fixed[i] = new int[64];
                        n++;
                    }
                    break;
                }
                case "mosaic": {
                    fixed = new Object[2_000_000];
                    for (int i = 0; i < fixed.length; i++) {
                        fixed[i] = new int[64];
                        n++;
                        churn(i);
                    }
                    break;
                }
                case "grow": {
                    grow = new ArrayList<>();
                    for (int i = 0; i < 2_000_000; i++) {
                        grow.add(new int[64]);
                        n++;
                        churn(i);
                    }
                    break;
                }
                case "dense16k": {
                    grow = new ArrayList<>();
                    for (int i = 0; i < 1_000_000; i++) {
                        grow.add(new byte[16 * 1024]);
                        n++;
                        for (int k = 0; k < 16; k++) {
                            sink = new byte[2048];
                        }
                    }
                    break;
                }
                case "large": {
                    grow = new ArrayList<>(4096);
                    for (int i = 0; i < 4096; i++) {
                        grow.add(new byte[1 << 20]);
                        n++;
                    }
                    break;
                }
                default:
                    throw new IllegalArgumentException(shape);
            }
        } catch (OutOfMemoryError e) {
            oom = true;
        }
        fixed = null;
        grow = null;
        long elem;
        switch (shape) {
            case "dense16k": elem = 16 * 1024; break;
            case "large": elem = 1 << 20; break;
            default: elem = 64 * 4; break;
        }
        payload = n * elem;
        System.out.println("zr2cp shape=" + shape + " retained=" + n + " live_mb="
                + (payload >> 20) + " oom=" + oom);
    }

    /** ~400 bytes of short-lived objects per retained int[64], as boxing does. */
    static void churn(int i) {
        for (int k = 0; k < 8; k++) {
            Object[] args = {Integer.valueOf(i + 1000 + k), Long.valueOf(i * 31L + k),
                    Double.valueOf(i + 0.5)};
            sink = args;
        }
    }
}
