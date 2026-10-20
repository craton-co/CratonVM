// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import com.sun.jna.Callback;
import com.sun.jna.Library;
import com.sun.jna.Memory;
import com.sun.jna.Native;
import com.sun.jna.Platform;
import com.sun.jna.Pointer;
import java.util.Arrays;
import java.util.Random;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * gci i1/jn (2026-10-11): the {@code jna-upcall} case of
 * {@code tools/bench/GcJniLibrariesProbe.java} alone, timed, for
 * {@code docs/known-issues/gc/gc-jni-linux-generational-jna-upcalls-are-10x-slower-under-churn-20261010.md}.
 * Only JNA is on the class path (no netty / lz4 / zstd); in tools/bench, not
 * tools/probes, because it needs that jar to compile.
 *
 * <p>libc {@code qsort} (msvcrt on Windows) of {@code n} random ints with a JNA
 * {@link Callback} comparator that allocates, {@code rounds} times, while a
 * daemon thread allocates 16 B - 7 KB arrays (unless the third argument is
 * {@code nochurn}). The comparator runs on the calling Java thread inside
 * JNA's JNI native ({@code Native.invokeVoid}).
 *
 * <p>stdout (HotSpot prints the same): {@code jna-upcall: PASS}, exit 0; a
 * wrong sort prints {@code jna-upcall: FAIL <detail>}, exit 1. stderr (not
 * compared): {@code [gci-jn1-jna] rounds=.. n=.. churn=.. upcalls=.. ms=..
 * us_per_upcall=..}; the upcall count depends on the libc's qsort.
 *
 * <p>Arguments: rounds (default 10), n (default 1000), {@code churn} |
 * {@code nochurn} (default churn). Class path: jna 5.14.0
 * ({@code ~/.m2/repository/net/java/dev/jna/jna/5.14.0/jna-5.14.0.jar}).
 */
public final class GciJn1JnaUpcallProbe {

    static volatile boolean churning = true;
    static volatile Object sink;

    public interface CLib extends Library {
        void qsort(Pointer base, long count, long size, Comparator cmp);
    }

    public interface Comparator extends Callback {
        int invoke(Pointer a, Pointer b);
    }

    /** Keeps a small ring of young garbage alive so every collection moves something. */
    static void churn() {
        Object[] ring = new Object[64];
        int i = 0;
        while (churning) {
            ring[i++ & 63] = new byte[(i & 7) * 1024 + 16];
            if ((i & 255) == 0) {
                sink = new int[4096];
            }
        }
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 10;
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 1000;
        boolean churnOn = args.length <= 2 || !args[2].equals("nochurn");
        Thread churner = null;
        if (churnOn) {
            churner = new Thread(GciJn1JnaUpcallProbe::churn, "churn");
            churner.setDaemon(true);
            churner.start();
        }
        CLib c = Native.load(Platform.isWindows() ? "msvcrt" : "c", CLib.class);
        Random r = new Random(42);
        AtomicInteger calls = new AtomicInteger();
        Comparator cmp = (a, b) -> {
            calls.incrementAndGet();
            sink = new byte[256];
            return Integer.compare(a.getInt(0), b.getInt(0));
        };
        String fail = null;
        long t0 = System.nanoTime();
        for (int round = 0; round < rounds && fail == null; round++) {
            int[] data = new int[n];
            for (int i = 0; i < n; i++) {
                data[i] = r.nextInt();
            }
            Memory m = new Memory(4L * n);
            m.write(0, data, 0, n);
            c.qsort(m, n, 4, cmp);
            int[] got = m.getIntArray(0, n);
            java.lang.ref.Reference.reachabilityFence(m);
            java.lang.ref.Reference.reachabilityFence(cmp);
            int[] want = data.clone();
            Arrays.sort(want);
            if (!Arrays.equals(got, want)) {
                fail = "round " + round + " not sorted";
            }
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        churning = false;
        if (churner != null) {
            churner.join();
        }
        if (fail == null && calls.get() == 0) {
            fail = "the comparator never ran";
        }
        int upcalls = calls.get();
        System.err.println("[gci-jn1-jna] rounds=" + rounds + " n=" + n + " churn=" + churnOn
                + " upcalls=" + upcalls + " ms=" + ms
                + " us_per_upcall=" + (upcalls == 0 ? 0 : ms * 1000L / upcalls));
        System.out.println(fail == null ? "jna-upcall: PASS" : "jna-upcall: FAIL " + fail);
        System.exit(fail == null ? 0 : 1);
    }
}
