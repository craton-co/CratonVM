/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 */

import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;

/**
 * gci i14/pn (2026-10-13): JNI critical sections held open while another
 * thread churns the heap and calls System.gc(). HotSpot 25 G1 pins the array's
 * region (JEP 423: isCopy 0, the native writes the array's own body, every
 * release mode keeps the writes); HotSpot Serial holds the GC locker (same
 * answers). Every stdout line is HotSpot's exact answer; diagnostics go to
 * stderr. Native half: {@code tools/probes/jni/GciPn14CriticalPinStormProbe.c}.
 *
 * <p>Usage: {@code GciPn14CriticalPinStormProbe /abs/path/gcipn14.dll [reps] [ms]}
 * (defaults 10 and 40).
 *
 * <p>Each hold: the native takes the section, writes {@code seed + i} into
 * every element through the pointer, publishes "inside", sleeps {@code ms}
 * (the stormer sees "inside" and runs System.gc() while it churns), then
 * counts the elements that no longer read {@code seed + i} through the SAME
 * pointer ({@code inside=}: a body that moved or was reset reads zeros), writes
 * {@code seed + i + 1} and releases with {@code mode} (0, 1 = JNI_COMMIT,
 * 2 = JNI_ABORT). Java then counts elements that are not {@code seed + i + 1}
 * ({@code after=}: HotSpot ignores the mode, so 0 for every mode).
 * Arrays: {@code small} (a fresh 1024-int array, young), {@code humongous}
 * (300 000 ints, above half a 1 MiB region), {@code old} (one array kept from
 * the start, promoted by the warm-up churn).
 */
public final class GciPn14CriticalPinStormProbe {
    /** Returns the inside mismatch count (or -1 when Get failed); isCopyOut[0] = isCopy. */
    static native int holdCritical(int[] a, int seed, int ms, int mode, int[] isCopyOut);

    /** Two open sections (src read, dst written); returns the inside mismatch count. */
    static native int holdTwo(int[] src, int[] dst, int ms);

    /** GetStringCritical held across the sleep; returns the inside mismatch count. */
    static native int holdString(String s, int ms, int[] isCopyOut);

    /** 1 while a native holds a section (after its writes, before its re-check). */
    static native int inside();

    static volatile boolean stop;
    static volatile int holdId;
    static volatile Object sink;
    static int gcsInside;
    static long churnBytes;

    static void stormer() {
        Object[] ring = new Object[64];
        int seen = 0;
        int i = 0;
        while (!stop) {
            ring[i & 63] = new byte[64 * 1024];
            i++;
            churnBytes += 64 * 1024;
            int id = holdId;
            if (id != seen && inside() == 1) {
                seen = id;
                System.gc();
                gcsInside++;
            }
        }
        sink = ring;
    }

    static long gcCount() {
        long c = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            long x = b.getCollectionCount();
            if (x > 0) {
                c += x;
            }
        }
        return c;
    }

    static int[] arrayFor(String name, int[] old) {
        if (name.equals("small")) {
            return new int[1024];
        }
        if (name.equals("humongous")) {
            return new int[300000];
        }
        return old;
    }

    static String utf16(int r) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 200; i++) {
            sb.append((char) ('a' + (i + r) % 26));
            if (i % 10 == 0) {
                sb.append((char) 0x3b1); // GREEK SMALL LETTER ALPHA: a UTF16-coded String
            }
        }
        return sb.toString();
    }

    public static void main(String[] args) throws Exception {
        if (args.length < 1) {
            System.out.println("usage: GciPn14CriticalPinStormProbe /abs/path/gcipn14.dll [reps] [ms]");
            return;
        }
        System.load(args[0]);
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 10;
        int ms = args.length > 2 ? Integer.parseInt(args[2]) : 40;
        int[] old = new int[4096];
        for (int k = 0; k < 2000; k++) {
            sink = new byte[64 * 1024];
        }
        long gc0 = gcCount();
        long t0 = System.nanoTime();
        Thread t = new Thread(GciPn14CriticalPinStormProbe::stormer, "pn14-stormer");
        t.setDaemon(true);
        t.start();
        String[] names = {"small", "humongous", "old"};
        int[] modes = {0, 1, 2};
        for (String name : names) {
            for (int mode : modes) {
                int inside = 0;
                int after = 0;
                int copies = 0;
                int failed = 0;
                for (int r = 0; r < reps; r++) {
                    int[] a = arrayFor(name, old);
                    int seed = r * 7 + mode;
                    int[] ic = new int[1];
                    holdId++;
                    int bad = holdCritical(a, seed, ms, mode, ic);
                    if (bad < 0) {
                        failed++;
                        continue;
                    }
                    inside += bad;
                    copies += ic[0];
                    for (int i = 0; i < a.length; i++) {
                        if (a[i] != seed + i + 1) {
                            after++;
                        }
                    }
                }
                System.out.println("storm " + name + " mode=" + mode + ": isCopy=" + copies + "/"
                        + reps + " inside=" + inside + " after=" + after + " failed=" + failed);
            }
        }
        int inside2 = 0;
        int after2 = 0;
        for (int r = 0; r < reps; r++) {
            int[] src = new int[2048];
            for (int i = 0; i < src.length; i++) {
                src[i] = i + r;
            }
            int[] dst = new int[2048];
            holdId++;
            inside2 += holdTwo(src, dst, ms);
            for (int i = 0; i < dst.length; i++) {
                if (dst[i] != 2 * (i + r)) {
                    after2++;
                }
            }
        }
        System.out.println("storm two-sections: inside=" + inside2 + " after=" + after2);
        int insideS = 0;
        int copiesS = 0;
        for (int r = 0; r < reps; r++) {
            String s = utf16(r);
            int[] ic = new int[1];
            holdId++;
            insideS += holdString(s, ms, ic);
            copiesS += ic[0];
        }
        System.out.println("storm string-utf16: isCopy=" + copiesS + "/" + reps + " inside=" + insideS);
        stop = true;
        t.join();
        System.err.println("[gci-pn14] system-gcs-inside=" + gcsInside + " gcs=" + (gcCount() - gc0)
                + " churn-mb=" + (churnBytes >> 20) + " ms=" + ((System.nanoTime() - t0) / 1000000));
        System.out.println("done");
    }
}
