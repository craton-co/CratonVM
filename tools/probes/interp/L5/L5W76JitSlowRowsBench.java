// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3, wave 76, lane L5 -- why three `L5W75ZipSplitBench` rows
// are SLOWER with the JIT than with --nojit (`raf-seek-read` ~11 300 vs ~6 900
// ns, `weak-set-churn` ~15 400 vs ~8 000, `inflater-cache` ~6 200 vs ~2 400 per
// iteration) while the zip lookups are 2-5x faster with it
// (`docs/internal/jit-proposals/i76-L5-proposal-osr-loops-pay-the-monitor-helper-20261109-RETIRED-20261009.md`).
//
// All three slow rows hold `synchronized` blocks in the bench's own loop, a
// lambda body run once per round (so it can only be compiled by OSR); the
// extra JIT time per iteration is ~1 us per monitorenter / monitorexit in all
// three. Each row here runs the same loop body three ways:
//
//   <row>-osr      the loop in a lambda run once per round (W75's shape: OSR)
//   <row>-method   the loop body in a static method called once per iteration
//                  (a normal method-entry compile, no OSR)
//   <row>-nosync   the OSR shape without the `synchronized` blocks
//
// rows: sync-empty (three empty `synchronized` blocks on one object: the
// monitors alone), weak-set-churn (W75's), raf-seek-read (W75's, two 64-byte
// reads), inflater-cache (W75's).
//
// Read: if <row>-osr is slow and <row>-method / <row>-nosync are not, the cost
// is the OSR artifact's monitor sites (census: CRATONVM_DBG_JITC=1 prints each
// compiled monitor site's inline-or-helper verdict and its refusal reason).
//
// stdout: per row one deterministic "<row> <count>" line (no "ns/") and one
// timing line "<row> <n> ns/op (median of rounds 1..ROUNDS-1, per iteration)".
// One row family only: `L5W76JitSlowRowsBench <family>` (e.g. sync-empty).
//
// Run (JIT on and --nojit, interleaved, three repetitions, medians):
//   $J/javac -d out L5W76JitSlowRowsBench.java
//   $B --java-home $J/.. -cp out L5W76JitSlowRowsBench
//   $B --java-home $J/.. --nojit -cp out L5W76JitSlowRowsBench
//   env CRATONVM_DBG_JITC=1 $B --java-home $J/.. -cp out L5W76JitSlowRowsBench sync-empty 2>&1 | grep -i monitor
//   $J/java -cp out L5W76JitSlowRowsBench                                     # HotSpot
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// sync-empty-osr count=2000
// sync-empty-method count=2000
// sync-empty-nosync count=2000
// weak-set-churn-osr count=2000
// weak-set-churn-method count=2000
// weak-set-churn-nosync count=2000
// raf-seek-read-osr count=4000
// raf-seek-read-method count=4000
// raf-seek-read-nosync count=4000
// inflater-cache-osr count=2000
// inflater-cache-method count=2000
// inflater-cache-nosync count=2000
// HOTSPOT_EXPECTED_END

import java.io.File;
import java.io.IOException;
import java.io.RandomAccessFile;
import java.nio.file.Files;
import java.util.ArrayDeque;
import java.util.Arrays;
import java.util.Collections;
import java.util.Deque;
import java.util.Set;
import java.util.WeakHashMap;
import java.util.zip.Inflater;

public class L5W76JitSlowRowsBench {
    static final int ITERS = 2000;
    static final int ROUNDS = 7;
    static final int READ = 64;

    interface Round {
        long run() throws Exception;
    }

    static void row(String label, Round round) throws Exception {
        long[] perIter = new long[ROUNDS];
        long result = 0;
        for (int r = 0; r < ROUNDS; r++) {
            long t0 = System.nanoTime();
            result = round.run();
            perIter[r] = (System.nanoTime() - t0) / ITERS;
        }
        long[] later = Arrays.copyOfRange(perIter, 1, ROUNDS);
        Arrays.sort(later);
        System.out.println(label + " count=" + result);
        System.out.println(label + " " + later[later.length / 2]
                + " ns/op (median of rounds 1.." + (ROUNDS - 1) + ", per iteration)");
    }

    // ---- sync-empty ----
    static final Object LOCK = new Object();
    static long syncCount;

    static void syncOnce() {
        synchronized (LOCK) {
            syncCount++;
        }
        synchronized (LOCK) {
            syncCount++;
        }
        synchronized (LOCK) {
            syncCount--;
        }
    }

    // ---- weak-set-churn ----
    static long churnOnce(Set<Object> streams) {
        Object stream = new Object();
        long added = 0;
        synchronized (streams) {
            if (streams.add(stream)) {
                added++;
            }
        }
        synchronized (streams) {
            streams.remove(stream);
        }
        synchronized (streams) {
            streams.remove(stream);
        }
        return added;
    }

    // ---- raf-seek-read ----
    static long rafOnce(RandomAccessFile f, byte[] buf, int i) throws IOException {
        long pos = ((long) i * 1409L) % (f.length() - 2 * READ);
        synchronized (f) {
            f.seek(pos);
            f.readFully(buf, 0, READ);
        }
        synchronized (f) {
            f.seek(pos + READ);
            f.readFully(buf, 0, READ);
        }
        return 2;
    }

    // ---- inflater-cache ----
    static long cacheOnce(Deque<Inflater> cache) {
        Inflater inf;
        long polled = 0;
        synchronized (cache) {
            inf = cache.poll();
        }
        if (inf == null) {
            inf = new Inflater(true);
        } else {
            polled++;
        }
        synchronized (cache) {
            inf.reset();
            cache.add(inf);
        }
        return polled;
    }

    static boolean wanted(String only, String family) {
        return only == null || only.equals(family);
    }

    public static void main(String[] args) throws Exception {
        String only = args.length > 0 ? args[0] : null;
        File file = Files.createTempFile("l5w76jit", ".bin").toFile();
        try {
            byte[] data = new byte[64 * 1024];
            for (int i = 0; i < data.length; i++) {
                data[i] = (byte) (i * 31);
            }
            Files.write(file.toPath(), data);

            if (wanted(only, "sync-empty")) {
                row("sync-empty-osr", () -> {
                    long n = 0;
                    for (int i = 0; i < ITERS; i++) {
                        synchronized (LOCK) {
                            syncCount++;
                        }
                        synchronized (LOCK) {
                            syncCount++;
                        }
                        synchronized (LOCK) {
                            syncCount--;
                        }
                        n++;
                    }
                    return n;
                });
                row("sync-empty-method", () -> {
                    long n = 0;
                    for (int i = 0; i < ITERS; i++) {
                        syncOnce();
                        n++;
                    }
                    return n;
                });
                row("sync-empty-nosync", () -> {
                    long n = 0;
                    for (int i = 0; i < ITERS; i++) {
                        syncCount++;
                        syncCount++;
                        syncCount--;
                        n++;
                    }
                    return n;
                });
            }
            if (wanted(only, "weak-set-churn")) {
                row("weak-set-churn-osr", () -> {
                    Set<Object> streams = Collections.newSetFromMap(new WeakHashMap<>());
                    long added = 0;
                    for (int i = 0; i < ITERS; i++) {
                        Object stream = new Object();
                        synchronized (streams) {
                            if (streams.add(stream)) {
                                added++;
                            }
                        }
                        synchronized (streams) {
                            streams.remove(stream);
                        }
                        synchronized (streams) {
                            streams.remove(stream);
                        }
                    }
                    return added;
                });
                row("weak-set-churn-method", () -> {
                    Set<Object> streams = Collections.newSetFromMap(new WeakHashMap<>());
                    long added = 0;
                    for (int i = 0; i < ITERS; i++) {
                        added += churnOnce(streams);
                    }
                    return added;
                });
                row("weak-set-churn-nosync", () -> {
                    Set<Object> streams = Collections.newSetFromMap(new WeakHashMap<>());
                    long added = 0;
                    for (int i = 0; i < ITERS; i++) {
                        Object stream = new Object();
                        if (streams.add(stream)) {
                            added++;
                        }
                        streams.remove(stream);
                        streams.remove(stream);
                    }
                    return added;
                });
            }
            if (wanted(only, "raf-seek-read")) {
                try (RandomAccessFile f = new RandomAccessFile(file, "r")) {
                    byte[] buf = new byte[READ];
                    row("raf-seek-read-osr", () -> {
                        long reads = 0;
                        for (int i = 0; i < ITERS; i++) {
                            long pos = ((long) i * 1409L) % (f.length() - 2 * READ);
                            synchronized (f) {
                                f.seek(pos);
                                f.readFully(buf, 0, READ);
                            }
                            synchronized (f) {
                                f.seek(pos + READ);
                                f.readFully(buf, 0, READ);
                            }
                            reads += 2;
                        }
                        return reads;
                    });
                    row("raf-seek-read-method", () -> {
                        long reads = 0;
                        for (int i = 0; i < ITERS; i++) {
                            reads += rafOnce(f, buf, i);
                        }
                        return reads;
                    });
                    row("raf-seek-read-nosync", () -> {
                        long reads = 0;
                        for (int i = 0; i < ITERS; i++) {
                            long pos = ((long) i * 1409L) % (f.length() - 2 * READ);
                            f.seek(pos);
                            f.readFully(buf, 0, READ);
                            f.seek(pos + READ);
                            f.readFully(buf, 0, READ);
                            reads += 2;
                        }
                        return reads;
                    });
                }
            }
            if (wanted(only, "inflater-cache")) {
                Deque<Inflater> cache = new ArrayDeque<>();
                cache.add(new Inflater(true));
                try {
                    row("inflater-cache-osr", () -> {
                        long polled = 0;
                        for (int i = 0; i < ITERS; i++) {
                            Inflater inf;
                            synchronized (cache) {
                                inf = cache.poll();
                            }
                            if (inf == null) {
                                inf = new Inflater(true);
                            } else {
                                polled++;
                            }
                            synchronized (cache) {
                                inf.reset();
                                cache.add(inf);
                            }
                        }
                        return polled;
                    });
                    row("inflater-cache-method", () -> {
                        long polled = 0;
                        for (int i = 0; i < ITERS; i++) {
                            polled += cacheOnce(cache);
                        }
                        return polled;
                    });
                    row("inflater-cache-nosync", () -> {
                        long polled = 0;
                        for (int i = 0; i < ITERS; i++) {
                            Inflater inf = cache.poll();
                            if (inf == null) {
                                inf = new Inflater(true);
                            } else {
                                polled++;
                            }
                            inf.reset();
                            cache.add(inf);
                        }
                        return polled;
                    });
                } finally {
                    Inflater inf;
                    while ((inf = cache.poll()) != null) {
                        inf.end();
                    }
                }
            }
        } finally {
            file.delete();
        }
    }
}
