package org.apache.commons.rng.simple.internal;

import java.util.concurrent.locks.ReentrantLock;
import org.apache.commons.rng.UniformRandomProvider;
import org.apache.commons.rng.core.source64.RandomLongSource;
import org.apache.commons.rng.core.source64.SplitMix64;

/**
 * Corpus-harness stand-in for commons-rng-simple 1.7's {@code SeedFactory}, placed FIRST on both arms'
 * classpath by {@code corpora.d/commons-math.sh}. Every "unseeded" {@code RandomSource.X.create()} the
 * commons-math tests make draws its seed from here, and here the seeds come from a SplitMix64 seeded by
 * env {@code CORPUS_RNG_SEED} (default 1). A whole JUnit run is therefore a pure function of that number,
 * on HotSpot and on CratonVM alike.
 *
 * <p>Why: about 35 commons-math tests draw unseeded random input and fail on HotSpot itself at rates from
 * 1/37 to 37/37 (measured 2026-10-10). Upstream's seed mixes {@code System.nanoTime()} and identity hash
 * codes, so the pass/fail counts of two correct VMs diverged on a coin flip, and the only alternative was
 * to exclude those tests from the comparison. Seeded, they compare exactly -- failure messages, which
 * print computed doubles, included.
 *
 * <p>Same public/package API as upstream 1.7 (javap); {@code ensureNonZero} keeps upstream's
 * all-zero-range contract but not its exact refill sequence, which only an all-zero seed reaches.
 */
public final class SeedFactory {
    private static final ReentrantLock LOCK = new ReentrantLock();
    private static final UniformRandomProvider SEED_GENERATOR;
    static {
        String s = System.getenv("CORPUS_RNG_SEED");
        long seed = s == null || s.trim().isEmpty() ? 1L : Long.parseLong(s.trim());
        SEED_GENERATOR = new SplitMix64(seed);
        System.err.println("CORPUS_RNG_SEED=" + seed);
    }

    private SeedFactory() {}

    public static int createInt() {
        LOCK.lock();
        try { return SEED_GENERATOR.nextInt(); } finally { LOCK.unlock(); }
    }

    public static long createLong() {
        LOCK.lock();
        try { return SEED_GENERATOR.nextLong(); } finally { LOCK.unlock(); }
    }

    public static int[] createIntArray(int n) { return createIntArray(n, 0, Math.min(n, 1)); }

    static int[] createIntArray(int n, int from, int to) {
        int[] a = new int[n];
        LOCK.lock();
        try { for (int i = 0; i < n; i++) a[i] = SEED_GENERATOR.nextInt(); } finally { LOCK.unlock(); }
        ensureNonZero(a, from, to);
        return a;
    }

    public static long[] createLongArray(int n) { return createLongArray(n, 0, Math.min(n, 1)); }

    static long[] createLongArray(int n, int from, int to) {
        long[] a = new long[n];
        LOCK.lock();
        try { for (int i = 0; i < n; i++) a[i] = SEED_GENERATOR.nextLong(); } finally { LOCK.unlock(); }
        ensureNonZero(a, from, to);
        return a;
    }

    static byte[] createByteArray(UniformRandomProvider rng, int n, int from, int to) {
        byte[] b = new byte[n];
        rng.nextBytes(b);
        ensureNonZero(b, from, to, rng);
        return b;
    }

    static void ensureNonZero(int[] a, int from, int to) {
        if (from >= to) return;
        for (int i = from; i < to; i++) if (a[i] != 0) return;
        int v = createInt() << 1;
        for (int i = from; i < to; i++) { v -= 0x61C88647; a[i] = MixFunctions.murmur3(v); }
    }

    static void ensureNonZero(long[] a, int from, int to) {
        if (from >= to) return;
        for (int i = from; i < to; i++) if (a[i] != 0) return;
        long v = createLong() << 1;
        for (int i = from; i < to; i++) { v += 0x9E3779B97F4A7C15L; a[i] = MixFunctions.stafford13(v); }
    }

    static void ensureNonZero(byte[] b, int from, int to, UniformRandomProvider rng) {
        if (from >= to) return;
        for (int i = from; i < to; i++) if (b[i] != 0) return;
        for (int i = from; i < to; i++) {
            byte x;
            do { x = (byte) rng.nextInt(); } while (x == 0);
            b[i] = x;
        }
    }

    static long ensureNonZero(RandomLongSource source, long seed) {
        long v = seed;
        while (v == 0) v = source.next();
        return v;
    }
}
