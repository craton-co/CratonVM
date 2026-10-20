// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gci i24/bp (2026-10-15): a JIT-warm program whose main thread sits in a JNI
 * native, under compiled callers that hold young objects only in their frames,
 * while another (compiled) thread churns the young generation. Proposal P2/bp,
 * {@code docs/internal/gc/gch-h4bp-review-findings-RETIRED-UNFIXED-20261018.md} item 1,
 * switch {@code CRATONVM_JNI_IN_NATIVE_PROVEN_DEPTH_CREDIT}.
 *
 * <p>With the switch off every Generational young pause of the wait is refused
 * a moving cycle ({@code [GC] moving_young_fallback_reason: cross-thread-jit-peer})
 * because the in-native thread's JIT depth is never credited. With it on, the
 * thread's entry deposit proves its chain and the pause credits it, so the
 * pinned in-place copy may run; this probe is both the engagement row (the
 * {@code CRATONVM_GC_STATS=1} shutdown lines) and a soundness row (the chains
 * the compiled frames hold across the wait are read back after it).
 *
 * <p>Reuses the natives of {@code tools/bench/Gcd1JniBlockInNativeProbe}
 * ({@code libgcd1jniblock.so}, {@code tools/probes/jni/Gcd1JniBlockInNativeProbe.c}):
 * {@code blockInC} waits in C (no JNI call in its loop) until
 * {@code release}. Cases, each {@link #ROUNDS} times:
 *
 * <ul>
 *   <li>{@code moving}: one compiled frame holds a {@link #CHAIN}-node chain;</li>
 *   <li>{@code deep}: {@link #DEEP} compiled frames of a recursion, each holding
 *       its own short chain, the native at the bottom.</li>
 * </ul>
 *
 * <p>HotSpot ({@code java -XX:+UseSerialGC -Xmx64m}) prints, in this order:
 * <pre>
 *   warm: PASS
 *   moving 1: PASS
 *   moving 2: PASS
 *   moving 3: PASS
 *   moving 4: PASS
 *   deep 1: PASS
 *   deep 2: PASS
 *   deep 3: PASS
 *   deep 4: PASS
 *   PASS all 9
 * </pre>
 * and exits 0; a failing case prints {@code <case>: FAIL code=<n>} (low three
 * bits: the native's own code; above: the bad-node count, plus 1000000 for a
 * short chain), the summary is {@code FAIL <n> of 9} and the exit code 1.
 * Waits and the allocator's progress go to stderr only. Each native gives up
 * after {@link #MAX_MS}; every join is bounded.
 *
 * <p>Commands (Linux; the library as for {@code Gcd1JniBlockInNativeProbe}):
 * <pre>
 *   javac -d /tmp/bp24 tools/bench/Gcd1JniBlockInNativeProbe.java tools/probes/GciBp24InNativeMovingProbe.java
 *   java -XX:+UseSerialGC -Xmx64m -cp /tmp/bp24 GciBp24InNativeMovingProbe /abs/libgcd1jniblock.so
 *   CRATONVM_GC_STATS=1 CRATONVM_JNI_IN_NATIVE_PROVEN_DEPTH_CREDIT=1 \
 *     cratonvm --java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp /tmp/bp24 \
 *       GciBp24InNativeMovingProbe /abs/libgcd1jniblock.so
 * </pre>
 */
public final class GciBp24InNativeMovingProbe {

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

    static final int MAX_MS = 20_000;
    static final long CHURN_BYTES = 256L << 20;
    static final int WARM = 12_000;
    static final int CHAIN = 64;
    static final int DEEP = 6;
    static final int ROUNDS = 4;

    /** The allocator's small live set: a ring of recent arrays. */
    static final Object[] RING = new Object[512];
    static int ringAt;

    /** Short-lived garbage with a small live ring; compiled during the warm-up. */
    static long churn(long bytes) {
        long sum = 0;
        for (long n = 0; n < bytes; n += 2048) {
            byte[] b = new byte[2048];
            b[(int) (n & 2047)] = (byte) n;
            RING[ringAt++ & (RING.length - 1)] = b;
            sum += b.length;
        }
        return sum;
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

    /** The chain lives only in this compiled frame across the native. */
    static int movingAcross(int seed, int len, int maxMs) {
        Node head = chain(seed, len);
        int code = Gcd1JniBlockInNativeProbe.blockInC(new Gcd1JniBlockInNativeProbe.Holder(seed), seed, maxMs);
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

    /** Wait (bounded) until the case's native has entered its wait, churn, then release it. */
    static Thread allocator() {
        Thread t = new Thread(() -> {
            long deadline = System.nanoTime() + MAX_MS * 1_000_000L;
            while (!Gcd1JniBlockInNativeProbe.entered() && System.nanoTime() < deadline) {
                try {
                    Thread.sleep(1);
                } catch (InterruptedException e) {
                    return;
                }
            }
            long churned = churn(CHURN_BYTES);
            System.err.println("[bp24] allocator churned " + (churned >> 20) + " MiB");
            Gcd1JniBlockInNativeProbe.release();
        }, "bp24-allocator");
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
        Thread a = allocator();
        a.start();
        long s0 = System.nanoTime();
        int code = body.getAsInt();
        long waited = (System.nanoTime() - s0) / 1_000_000L;
        a.join(60_000);
        System.err.println("[bp24] " + name + " waited " + waited + " ms, allocator done=" + !a.isAlive());
        return code;
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: GciBp24InNativeMovingProbe /abs/path/libgcd1jniblock.so");
            System.exit(2);
        }
        System.load(args[0]);

        // Warm-up: the native returns at once (its release flag is set), so the
        // callers and the churn loop are called often enough to be compiled.
        Gcd1JniBlockInNativeProbe.reset();
        Gcd1JniBlockInNativeProbe.release();
        int warm = 0;
        long warmBytes = 0;
        for (int i = 0; i < WARM; i++) {
            warm |= movingAcross(i, 8, 1000);
            warm |= deepAcross(DEEP - 1, i, 2, 1000);
            if ((i & 63) == 0) {
                warmBytes += churn(64L << 10);
            }
        }
        System.err.println("[bp24] warm churned " + (warmBytes >> 10) + " KiB");
        report("warm", warm);

        for (int r = 1; r <= ROUNDS; r++) {
            final int seed = 0x5000 + r;
            report("moving " + r, runBlocking("moving " + r, () -> movingAcross(seed, CHAIN, MAX_MS)));
        }
        for (int r = 1; r <= ROUNDS; r++) {
            final int seed = 0x6000 + r;
            report("deep " + r, runBlocking("deep " + r, () -> deepAcross(DEEP - 1, seed, CHAIN / DEEP, MAX_MS)));
        }

        int total = 1 + 2 * ROUNDS;
        if (failures == 0) {
            System.out.println("PASS all " + total);
        } else {
            System.out.println("FAIL " + failures + " of " + total);
            System.exit(1);
        }
    }
}
