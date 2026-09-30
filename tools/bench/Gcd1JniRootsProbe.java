// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * gcd d2/i (2026-09-27): the out-of-process JNI run three parked pages wait on
 * before their flags can default on.
 *
 * <ul>
 *   <li>{@code common-w2c-jni-local-refs-are-raw-addresses}
 *       ({@code CRATONVM_JNI_INDIRECT_LOCALS}): a native's own copy of a local
 *       ref, held across an allocating up-call ({@code local-across-upcall})
 *       and across a monitor enter/exit pair ({@code local-monitor-across-upcall});</li>
 *   <li>{@code common-w9g-idle-foreign-threads-run-jni-functions-gc-blocked}
 *       ({@code CRATONVM_JNI_FOREIGN_TRANSITIONS}): an attached pthread's locals
 *       made while it is idle ({@code foreign-idle-locals}) and its element
 *       copies across idle windows ({@code foreign-array-elements});</li>
 *   <li>{@code gengc-r4w4-rooting2-jni-foreign-thread-accessors-and-unrooted-results}:
 *       a {@code CallObjectMethod} result held, with no global ref, across a
 *       second allocating up-call ({@code foreign-call-result}).</li>
 * </ul>
 *
 * <p>A Java churn thread (and the up-calls themselves) collect all the time, so
 * every window above spans collections. The native half is
 * {@code tools/probes/jni/Gcd1JniRootsProbe.c} (Linux, pthreads); it never
 * blocks inside a native method while a collection may be pending (see its
 * header comment).
 *
 * <p>HotSpot ({@code java -XX:+UseSerialGC -Xmx64m}) prints, in this order:
 * <pre>
 *   local-across-upcall: PASS
 *   local-monitor-across-upcall: PASS
 *   foreign-attach: PASS
 *   foreign-idle-locals: PASS
 *   foreign-call-result: PASS
 *   foreign-array-elements: PASS
 *   PASS all 6
 * </pre>
 * and exits 0; otherwise the failing lines read {@code <case>: FAIL <detail>},
 * the summary is {@code FAIL <n> of 6} and the exit code 1. Ends on its own:
 * every wait is bounded, and the only thread that can stay blocked (the
 * monitor case's contender) is a daemon.
 *
 * <p>Commands (Linux):
 * <pre>
 *   gcc -O1 -shared -fPIC -pthread -I"$JDK/include" -I"$JDK/include/linux" \
 *       -o /tmp/libgcd1jniroots.so tools/probes/jni/Gcd1JniRootsProbe.c
 *   javac -d /tmp/gcd1jni tools/bench/Gcd1JniRootsProbe.java
 *   java -XX:+UseSerialGC -Xmx64m -cp /tmp/gcd1jni Gcd1JniRootsProbe /tmp/libgcd1jniroots.so
 *   cratonvm --java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp /tmp/gcd1jni \
 *       Gcd1JniRootsProbe /tmp/libgcd1jniroots.so
 * </pre>
 * The flag matrix and what each arm must print are on the three pages'
 * STATUS blocks.
 */
public final class Gcd1JniRootsProbe {

    /** The object the same-thread cases hand to the native. */
    static final class Holder {
        final int v;
        final String tag;

        Holder(int v, String tag) {
            this.v = v;
            this.tag = tag;
        }
    }

    static native int localAcrossUpcall(Holder o, Runnable churn, int expected);

    static native int monitorAcrossUpcall(Object o, Runnable churn);

    static native int foreignStart(Gcd1JniRootsProbe cb, int iters);

    static native boolean foreignDetached();

    static native void foreignJoin(int[] out);

    static final int FOREIGN_ITERS = 2000;

    /** Short-lived garbage with a small live ring, so young cycles copy survivors. */
    static final Object[] RING = new Object[256];
    static int ringAt;

    static void churn(int bytes) {
        for (int n = 0; n < bytes; n += 4096) {
            RING[ringAt++ & (RING.length - 1)] = new byte[4096];
        }
    }

    // ---- Up-calls from the attached thread -----------------------------------

    final CountDownLatch done = new CountDownLatch(1);
    volatile int acceptBad;

    public void accept(String s, byte[] b, int i) {
        boolean ok = s != null && s.equals("m" + i) && b != null && b.length == 64;
        if (ok) {
            for (int k = 0; k < 64; k++) {
                if (b[k] != (byte) (i + k)) {
                    ok = false;
                    break;
                }
            }
        }
        if (!ok) {
            acceptBad++;
        }
    }

    public String make(int i) {
        return new StringBuilder().append('r').append(i).toString();
    }

    public void churnOnce() {
        churn(256 * 1024);
    }

    public boolean checkResult(Object r, int i) {
        return r instanceof String && r.equals("r" + i);
    }

    public void done() {
        done.countDown();
    }

    // ---- Driver ----------------------------------------------------------------

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
        if (args.length != 1 && args.length != 2) {
            System.out.println("usage: Gcd1JniRootsProbe /abs/path/libgcd1jniroots.so [foreign-iters]");
            System.exit(2);
        }
        System.load(args[0]);
        // gcd d3/k: an optional iteration count for the attached thread, so a
        // roster audit run can make its `accept` up-call tier up
        // (`gcd-d2i-foreign-attached-threads-publish-no-os-tid-20260927.md`).
        int foreignIters = args.length == 2 ? Integer.parseInt(args[1]) : FOREIGN_ITERS;
        Runnable churn = () -> churn(24 * 1024 * 1024);

        // 1. The native's own copy of a local, across an allocating up-call.
        int local = localAcrossUpcall(new Holder(0x1234567, "tag"), churn, 0x1234567);
        report("local-across-upcall", local == 0, "code=" + local);

        // 2. MonitorEnter / MonitorExit through one local, an up-call between.
        Object lock = new Object();
        int mon = monitorAcrossUpcall(lock, churn);
        boolean[] got = new boolean[1];
        Thread contender = new Thread(() -> {
            synchronized (lock) {
                got[0] = true;
            }
        });
        contender.setDaemon(true);
        contender.start();
        contender.join(10_000);
        report("local-monitor-across-upcall", mon == 0 && got[0],
                "code=" + mon + " released=" + got[0]);

        // 3. The attached pthread, with a Java thread collecting beside it.
        Gcd1JniRootsProbe cb = new Gcd1JniRootsProbe();
        boolean[] stop = new boolean[1];
        Thread churner = new Thread(() -> {
            while (!stopped(stop)) {
                churn(1024 * 1024);
            }
        });
        churner.setDaemon(true);
        churner.start();
        int started = foreignStart(cb, foreignIters);
        // Wait in Java, never inside a native: see the C file's header.
        boolean detached = false;
        if (started == 0) {
            long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(300);
            while (!(detached = foreignDetached()) && System.nanoTime() < deadline) {
                cb.done.await(10, TimeUnit.MILLISECONDS);
            }
        }
        synchronized (stop) {
            stop[0] = true;
        }
        churner.join(30_000);
        int[] c = new int[5];
        if (detached) {
            // The attached thread makes no further VM call: the join cannot
            // wait on a pause.
            foreignJoin(c);
        }
        boolean finished = detached && cb.done.getCount() == 0;
        report("foreign-attach", finished && c[0] == 0,
                "started=" + started + " finished=" + finished + " attach=" + c[0]);
        report("foreign-idle-locals", finished && c[1] == 0 && cb.acceptBad == 0,
                "locals=" + c[1] + " accept=" + cb.acceptBad + " exceptions=" + c[4]);
        report("foreign-call-result", finished && c[2] == 0, "results=" + c[2]);
        report("foreign-array-elements", finished && c[3] == 0, "arrays=" + c[3]);

        if (failures == 0) {
            System.out.println("PASS all 6");
        } else {
            System.out.println("FAIL " + failures + " of 6");
            System.exit(1);
        }
    }

    static boolean stopped(boolean[] stop) {
        synchronized (stop) {
            return stop[0];
        }
    }
}
