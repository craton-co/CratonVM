// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * JIT round 16 wave 11 (lane jnieintr): a JNI native blocked in
 * {@code usleep} / {@code poll} must not fail with {@code EINTR} because the
 * VM collected meanwhile
 * ({@code docs/internal/fixed-bugs/r16-ffm-takeover-signal-interrupts-blocking-syscalls-in-native-FIXED-20261005.md}).
 *
 * <p>A "sleeper" thread calls, from a compiled method ({@link #sleepVia}, warmed
 * first), a JNI native that blocks once in {@code usleep(200000)} and then one
 * that blocks in {@code poll(NULL, 0, 200)}, {@link #ROUNDS} times each, and
 * counts the calls that returned {@code -EINTR}. Meanwhile a "churner" thread
 * allocates in a compiled loop ({@link #churn}), so the VM collects while a
 * thread is in compiled code and another is blocked -- the pauses that run the
 * Linux take-over and helper-window passes (their {@code SIGUSR2}). HotSpot
 * never signals a thread in native, so it prints
 * <pre>
 *   sleeper usleep: ok
 *   sleeper poll: ok
 *   PASS
 * </pre>
 * A VM that signals the sleeper prints {@code sleeper usleep: interrupted N
 * times, errno 4} (and the same for poll) and {@code FAIL}. Exit code 0 either
 * way; elapsed time goes to stderr only.
 *
 * <p>Commands (Linux):
 * <pre>
 *   gcc -O1 -shared -fPIC -I"$JDK/include" -I"$JDK/include/linux" \
 *       -o /tmp/libr16jniusleep.so tools/probes/jni/R16JniUsleepProbe.c
 *   javac -d /tmp/r16jniusleep tools/bench/R16JniUsleepProbe.java
 *   java -Xmx64m -cp /tmp/r16jniusleep R16JniUsleepProbe /tmp/libr16jniusleep.so
 *   # fixed arm (the JNI in-native package; the no-signal mark is on with it):
 *   CRATONVM_JNI_NATIVE_TRANSITIONS=1 CRATONVM_DBG_XT_FORCE_TAKEOVER=1 \
 *     cratonvm --java-home $JDK -Xmx64m -cp /tmp/r16jniusleep \
 *       R16JniUsleepProbe /tmp/libr16jniusleep.so
 *   # positive control (must print "interrupted"): the same plus
 *   #   CRATONVM_JNI_IN_NATIVE_NO_SIGNAL=0
 *   # counted-native arm (page item 1, not fixed; expected "interrupted"):
 *   #   no CRATONVM_JNI_NATIVE_TRANSITIONS
 * </pre>
 */
public final class R16JniUsleepProbe {

    static final int ROUNDS = 10;
    static final int WARM = 20_000;

    static native int sleepOnce(int us);

    static native int pollOnce(int ms);

    static volatile boolean done;
    static volatile long sink;

    /** The compiled caller of both natives: {@code which} 0 = usleep, 1 = poll. */
    static int sleepVia(int which, int amount) {
        return which == 0 ? sleepOnce(amount) : pollOnce(amount);
    }

    /** The compiled allocation loop of the churner. */
    static long churn(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            byte[] b = new byte[4096];
            b[i & 4095] = (byte) i;
            acc += b[(i * 7) & 4095];
        }
        return acc;
    }

    public static void main(String[] args) throws Exception {
        System.load(args[0]);
        for (int i = 0; i < WARM; i++) {
            sleepVia(i & 1, 0);
        }
        for (int i = 0; i < 200; i++) {
            sink += churn(1_000);
        }
        int[] eintr = new int[2];
        int[] lastErr = new int[2];
        Thread sleeper = new Thread(() -> {
            for (int r = 0; r < ROUNDS; r++) {
                for (int which = 0; which < 2; which++) {
                    int rc = sleepVia(which, which == 0 ? 200_000 : 200);
                    if (rc != 0) {
                        eintr[which]++;
                        lastErr[which] = -rc;
                    }
                }
            }
            done = true;
        }, "sleeper");
        Thread churner = new Thread(() -> {
            while (!done) {
                sink += churn(10_000);
            }
        }, "churner");
        long t0 = System.nanoTime();
        churner.start();
        sleeper.start();
        sleeper.join(120_000);
        done = true;
        churner.join(120_000);
        System.err.println("elapsed " + (System.nanoTime() - t0) / 1_000_000 + " ms");
        String[] names = {"usleep", "poll"};
        boolean ok = true;
        for (int which = 0; which < 2; which++) {
            if (eintr[which] == 0) {
                System.out.println("sleeper " + names[which] + ": ok");
            } else {
                ok = false;
                System.out.println("sleeper " + names[which] + ": interrupted " + eintr[which]
                        + " times, errno " + lastErr[which]);
            }
        }
        System.out.println(ok ? "PASS" : "FAIL");
    }
}
