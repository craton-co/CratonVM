// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gch h4/bp (2026-10-09): a compiled caller's own objects, held only in its
 * frame across a JNI native that blocks while another thread collects
 * ({@code docs/internal/gc/gcg-g4jn-proposal-in-native-band-scanned-at-the-pause-DONE-20261015.md},
 * {@code CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE}).
 *
 * <p>Reuses the natives of {@code tools/bench/Gcd1JniBlockInNativeProbe}
 * ({@code libgcd1jniblock.so}, {@code tools/probes/jni/Gcd1JniBlockInNativeProbe.c}),
 * called from THIS class's methods, which the warm-up compiles. Each case
 * builds a chain of {@code Node}s that no field and no argument names -- only
 * the compiled frame -- calls the native, which waits in C (or polls through
 * JNI) while an allocator thread churns {@link #CHURN_BYTES} at
 * {@code -Xmx64m} (several collections), and then walks the chain:
 *
 * <ul>
 *   <li>{@code block-across}: {@code blockInC} (six JNIEnv calls, then a wait
 *       in C with no JNI call), one compiled frame holding a 4096-node chain;</li>
 *   <li>{@code deep-across}: the same native under eight compiled frames of a
 *       recursion, each holding its own chain;</li>
 *   <li>{@code poll-across}: {@code pollInNative} (a JNIEnv call per round,
 *       {@code NewStringUTF} included) under one compiled frame.</li>
 * </ul>
 *
 * <p>Under the JNI in-native package the thread is in native (excluded) for
 * every collection of the wait, so the collector sees its compiled frames
 * only through what the entry deposited -- or, with the band-at-pause switch,
 * through the pause's scan of the band. A missed root shows as a FAIL line
 * (a chain node read back wrong, or a short chain) or as an exception.
 *
 * <p>HotSpot ({@code java -XX:+UseSerialGC -Xmx64m}) prints, in this order:
 * <pre>
 *   warm: PASS
 *   block-across 1: PASS
 *   block-across 2: PASS
 *   block-across 3: PASS
 *   deep-across: PASS
 *   poll-across: PASS
 *   PASS all 6
 * </pre>
 * and exits 0; a failing case prints {@code <case>: FAIL code=<n>} (low three
 * bits: the native's own code; above: the bad-node count, plus 1000000 for a
 * short chain), the summary is {@code FAIL <n> of 6} and the exit code 1.
 * Waits and the allocator's state go to stderr only. Each native gives up
 * after {@link #MAX_MS}; every join is bounded.
 *
 * <p>Commands (Linux; the library as for {@code Gcd1JniBlockInNativeProbe}):
 * <pre>
 *   javac -d /tmp/bp4 tools/bench/Gcd1JniBlockInNativeProbe.java tools/probes/GchBp4BandAtPauseProbe.java
 *   java -XX:+UseSerialGC -Xmx64m -cp /tmp/bp4 GchBp4BandAtPauseProbe /tmp/libgcd1jniblock.so
 *   CRATONVM_JNI_NATIVE_TRANSITIONS=1 CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE=1 \
 *     cratonvm --java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp /tmp/bp4 \
 *       GchBp4BandAtPauseProbe /tmp/libgcd1jniblock.so
 * </pre>
 */
public final class GchBp4BandAtPauseProbe {

    /** A chain node with a small payload whose last word encodes the value. */
    static final class Node {
        final int v;
        final Node next;
        final int[] pad;

        Node(int v, Node next) {
            this.v = v;
            this.next = next;
            this.pad = new int[6];
            this.pad[5] = v * 31 + 7;
        }
    }

    static final int MAX_MS = 15_000;
    static final long CHURN_BYTES = 192L << 20;
    static final int WARM = 12_000;
    static final int CHAIN = 4096;
    static final int DEEP = 8;

    /** Short-lived garbage with a small live ring (the allocator's only). */
    static final Object[] RING = new Object[256];
    static int ringAt;

    static void churn(long bytes) {
        for (long n = 0; n < bytes; n += 4096) {
            RING[ringAt++ & (RING.length - 1)] = new byte[4096];
        }
    }

    static Node chain(int seed, int len) {
        Node h = null;
        for (int i = 0; i < len; i++) {
            h = new Node(seed + i, h);
        }
        return h;
    }

    /** Bad nodes of the chain {@link #chain} built, plus 1000000 if it is short. */
    static int check(Node h, int seed, int len) {
        int bad = 0;
        int n = 0;
        for (Node p = h; p != null; p = p.next) {
            int want = seed + len - 1 - n;
            if (p.v != want || p.pad.length != 6 || p.pad[5] != want * 31 + 7) {
                bad++;
            }
            n++;
        }
        return n == len ? bad : bad + 1_000_000;
    }

    /** The chain lives only in this frame across the native. */
    static int blockAcross(int seed, int len, int maxMs) {
        Node head = chain(seed, len);
        int code = Gcd1JniBlockInNativeProbe.blockInC(new Gcd1JniBlockInNativeProbe.Holder(seed), seed, maxMs);
        return code | (check(head, seed, len) << 3);
    }

    /** As {@link #blockAcross}, with the native polling through JNI. */
    static int pollAcross(int seed, int len, int maxMs) {
        Node head = chain(seed, len);
        int code = Gcd1JniBlockInNativeProbe.pollInNative(maxMs);
        return code | (check(head, seed, len) << 3);
    }

    /** {@code depth + 1} compiled frames, each holding its own chain, the native at the bottom. */
    static int deepAcross(int depth, int seed, int len, int maxMs) {
        int mySeed = seed + depth * 100_000;
        Node mine = chain(mySeed, len);
        int below = depth == 0
                ? Gcd1JniBlockInNativeProbe.blockInC(new Gcd1JniBlockInNativeProbe.Holder(seed), seed, maxMs)
                : deepAcross(depth - 1, seed, len, maxMs);
        return below | (check(mine, mySeed, len) << 3);
    }

    /** Wait (bounded) until the case's native has entered its wait, churn, then run {@code release}. */
    static Thread allocator(Runnable release) {
        Thread t = new Thread(() -> {
            long deadline = System.nanoTime() + MAX_MS * 1_000_000L;
            while (!Gcd1JniBlockInNativeProbe.entered() && System.nanoTime() < deadline) {
                try {
                    Thread.sleep(1);
                } catch (InterruptedException e) {
                    return;
                }
            }
            churn(CHURN_BYTES);
            release.run();
        }, "bp4-allocator");
        t.setDaemon(true);
        return t;
    }

    static int failures;

    static void report(String name, int code) {
        if (code == 0) {
            System.out.println(name + ": PASS");
        } else {
            failures++;
            System.out.println(name + ": FAIL code=" + code);
        }
    }

    static int runBlocking(String name, java.util.function.IntSupplier body) throws InterruptedException {
        Gcd1JniBlockInNativeProbe.reset();
        Thread a = allocator(Gcd1JniBlockInNativeProbe::release);
        a.start();
        long s0 = System.nanoTime();
        int code = body.getAsInt();
        long waited = (System.nanoTime() - s0) / 1_000_000L;
        a.join(60_000);
        System.err.println("[bp4] " + name + " waited " + waited + " ms, allocator done=" + !a.isAlive());
        return code;
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: GchBp4BandAtPauseProbe /abs/path/libgcd1jniblock.so");
            System.exit(2);
        }
        System.load(args[0]);

        // Warm-up: every native returns at once (both release flags set), so
        // the three callers are called often enough to be compiled.
        Gcd1JniBlockInNativeProbe.reset();
        Gcd1JniBlockInNativeProbe.release();
        Gcd1JniBlockInNativeProbe.released = true;
        int warm = 0;
        for (int i = 0; i < WARM; i++) {
            warm |= blockAcross(i, 8, 1000);
            warm |= pollAcross(i, 8, 1000);
            warm |= deepAcross(DEEP - 1, i, 4, 1000);
        }
        report("warm", warm);

        for (int r = 1; r <= 3; r++) {
            final int seed = 0x5000 + r;
            report("block-across " + r, runBlocking("block-across " + r, () -> blockAcross(seed, CHAIN, MAX_MS)));
        }
        report("deep-across", runBlocking("deep-across", () -> deepAcross(DEEP - 1, 0x6000, CHAIN / DEEP, MAX_MS)));

        Gcd1JniBlockInNativeProbe.released = false;
        Gcd1JniBlockInNativeProbe.reset();
        Thread b = allocator(() -> Gcd1JniBlockInNativeProbe.released = true);
        b.start();
        long s0 = System.nanoTime();
        int poll = pollAcross(0x7000, CHAIN, MAX_MS);
        long waited = (System.nanoTime() - s0) / 1_000_000L;
        b.join(60_000);
        System.err.println("[bp4] poll-across waited " + waited + " ms, allocator done=" + !b.isAlive());
        report("poll-across", poll);

        if (failures == 0) {
            System.out.println("PASS all 6");
        } else {
            System.out.println("FAIL " + failures + " of 6");
            System.exit(1);
        }
    }
}
