// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gcd d3/k (2026-09-27): a Java thread that blocks inside a JNI native method
 * must not hold another thread's collections
 * ({@code docs/known-issues/gc/gcd-d2i-jni-native-methods-are-counted-mutators-20260927.md}).
 *
 * <p>Three cases, each with an allocator thread that churns {@code 256 MB} in
 * 4 KB arrays (several collections at {@code -Xmx64m}) while the main thread is
 * inside a native, and only then lets the native return:
 *
 * <ul>
 *   <li>{@code block-in-native}: the native waits in C ({@code usleep} until a C
 *       flag the allocator sets through another native), with no JNI call in
 *       the loop. HotSpot runs it {@code _thread_in_native}: the allocator's
 *       collections do not wait for it. A VM that counts the thread as a
 *       running mutator waits for the native instead, and the native times out
 *       ({@code FAIL code=1});</li>
 *   <li>{@code local-across-block}: the same native's own copy of a local
 *       ref, held across the block, still names its object
 *       ({@code IsSameObject} against a global ref, then a field read) -- the
 *       collections of the block may have moved it;</li>
 *   <li>{@code poll-in-native}: the native polls a Java {@code static volatile
 *       boolean} through {@code GetStaticBooleanField}, allocating a string per
 *       round ({@code NewStringUTF} / {@code GetStringUTFLength} /
 *       {@code DeleteLocalRef}), until the allocator flips it: every JNIEnv
 *       call of a thread in native transitions native -> VM -> native.</li>
 * </ul>
 *
 * <p>HotSpot ({@code java -XX:+UseSerialGC -Xmx64m}) prints, in this order:
 * <pre>
 *   block-in-native: PASS
 *   local-across-block: PASS
 *   poll-in-native: PASS
 *   PASS all 3
 * </pre>
 * and exits 0; a failing case prints {@code <case>: FAIL <detail>}, the summary
 * is {@code FAIL <n> of 3} and the exit code 1. Elapsed times go to stderr
 * only. Ends on its own: each native gives up after {@link #MAX_MS}, and every
 * join is bounded.
 *
 * <p>Commands (Linux):
 * <pre>
 *   gcc -O1 -shared -fPIC -I"$JDK/include" -I"$JDK/include/linux" \
 *       -o /tmp/libgcd1jniblock.so tools/probes/jni/Gcd1JniBlockInNativeProbe.c
 *   javac -d /tmp/gcd1jniblock tools/bench/Gcd1JniBlockInNativeProbe.java
 *   java -XX:+UseSerialGC -Xmx64m -cp /tmp/gcd1jniblock Gcd1JniBlockInNativeProbe /tmp/libgcd1jniblock.so
 *   CRATONVM_JNI_INDIRECT_LOCALS=1 CRATONVM_JNI_NATIVE_TRANSITIONS=1 \
 *     cratonvm --java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp /tmp/gcd1jniblock \
 *       Gcd1JniBlockInNativeProbe /tmp/libgcd1jniblock.so
 * </pre>
 * The arms and what each must print are on the page's STATUS block.
 */
public final class Gcd1JniBlockInNativeProbe {

    /** The object whose local ref the blocking native holds. */
    static final class Holder {
        final int v;

        Holder(int v) {
            this.v = v;
        }
    }

    /** Clear the C flags (entered, released) before a case. */
    static native void reset();

    /** Has the native of the current case entered its wait? */
    static native boolean entered();

    /** Let {@link #blockInC} return (sets its C flag). */
    static native void release();

    /**
     * Wait in C until {@link #release}, at most {@code maxMs}; then check the
     * local {@code o}. Bit 0: timed out; bit 1: {@code IsSameObject} false;
     * bit 2: the field read back wrong.
     */
    static native int blockInC(Holder o, int expected, int maxMs);

    /**
     * Poll {@link #released} through JNI, allocating a string per round, at
     * most {@code maxMs}. Bit 0: timed out; bit 1: a string came back wrong.
     */
    static native int pollInNative(int maxMs);

    /** Read by {@link #pollInNative} through {@code GetStaticBooleanField}. */
    static volatile boolean released;

    static final int MAX_MS = 15_000;
    static final long CHURN_BYTES = 256L << 20;

    /** Short-lived garbage with a small live ring, so young cycles copy survivors. */
    static final Object[] RING = new Object[256];
    static int ringAt;

    static void churn(long bytes) {
        for (long n = 0; n < bytes; n += 4096) {
            RING[ringAt++ & (RING.length - 1)] = new byte[4096];
        }
    }

    /**
     * Wait (bounded) until the case's native has entered its wait, churn, then
     * run {@code release}.
     */
    static Thread allocator(Runnable release) {
        Thread t = new Thread(() -> {
            long deadline = System.nanoTime() + MAX_MS * 1_000_000L;
            while (!entered() && System.nanoTime() < deadline) {
                try {
                    Thread.sleep(1);
                } catch (InterruptedException e) {
                    return;
                }
            }
            churn(CHURN_BYTES);
            release.run();
        }, "gcd1-jni-allocator");
        t.setDaemon(true);
        return t;
    }

    static int failures;

    static void report(String name, boolean pass, String detail) {
        if (pass) {
            System.out.println(name + ": PASS");
        } else {
            failures++;
            System.out.println(name + ": FAIL " + detail);
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: Gcd1JniBlockInNativeProbe /abs/path/libgcd1jniblock.so");
            System.exit(2);
        }
        System.load(args[0]);

        // 1 + 2. Blocked in C, a local held across the block.
        reset();
        Thread a = allocator(Gcd1JniBlockInNativeProbe::release);
        a.start();
        long s0 = System.nanoTime();
        int block = blockInC(new Holder(0x5eed), 0x5eed, MAX_MS);
        long waited = (System.nanoTime() - s0) / 1_000_000L;
        a.join(60_000);
        System.err.println("[gcd1-jni-block] block-in-native waited " + waited + " ms, allocator done="
                + !a.isAlive());
        report("block-in-native", (block & 1) == 0, "code=" + block);
        report("local-across-block", (block & 6) == 0, "code=" + block);

        // 3. JNI calls from the native while the allocator collects.
        released = false;
        reset();
        Thread b = allocator(() -> released = true);
        b.start();
        s0 = System.nanoTime();
        int poll = pollInNative(MAX_MS);
        waited = (System.nanoTime() - s0) / 1_000_000L;
        b.join(60_000);
        System.err.println("[gcd1-jni-block] poll-in-native waited " + waited + " ms, allocator done="
                + !b.isAlive());
        report("poll-in-native", poll == 0, "code=" + poll);

        if (failures == 0) {
            System.out.println("PASS all 3");
        } else {
            System.out.println("FAIL " + failures + " of 3");
            System.exit(1);
        }
    }
}
