// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.atomic.AtomicInteger;

/**
 * gen r4w5/thrash5 (2026-09-24): several threads filling a heap with LIVE data
 * through one hot lock must end in a catchable {@code OutOfMemoryError}, and
 * the heap must be usable again afterwards. This isolates step {@code threads}
 * of {@code GenR4W4HeapFullThrashProbe}, which ran 1 183 {@code moving+major}
 * cycles in 120 s on the wave-4 binary without finishing
 * ({@code docs/internal/gc/gengc-r4w4-final-two-thread-heap-full-thrash-FIXED-20260924.md}).
 *
 * <p>The cause, established by reading: every CONTENDED {@code monitorenter}
 * retired the thread's TLAB before parking ({@code vm_exec::monitor_enter_blocking}),
 * so each hand-off of the lock buried most of a 256 KiB TLAB under a filler.
 * The young trigger counted the filler as occupancy and collected every 13
 * hand-offs, freeing only filler, while the program linked a few hundred
 * bytes per cycle. The fix under test: a bounded spin before parking
 * ({@code Monitor::spin_try_enter}), plus the mutator-progress half of the
 * GC-overhead limit ({@code gc_cycle_is_unproductive}).
 *
 * <p>Three shapes, each followed by a recovery check:
 * <ul>
 *   <li>{@code threads}: two threads, a {@code synchronized} block (the
 *       wave-4 step, verbatim);</li>
 *   <li>{@code threads-method}: two threads, a {@code static synchronized}
 *       method (the {@code ACC_SYNCHRONIZED} contended path,
 *       {@code monitor_enter_synchronized_method});</li>
 *   <li>{@code threads-4}: four threads on the block.</li>
 * </ul>
 *
 * <p>HotSpot ({@code -XX:+UseSerialGC -Xmx128m}) prints (the lines are
 * deterministic: every filler loops until it sees the error itself):
 * <pre>
 *   threads: OutOfMemoryError seen, 2 of 2 finished
 *   threads-recovered ok
 *   threads-method: OutOfMemoryError seen, 2 of 2 finished
 *   threads-method-recovered ok
 *   threads-4: OutOfMemoryError seen, 4 of 4 finished
 *   threads-4-recovered ok
 *   PASS
 * </pre>
 * Commands:
 * <pre>
 *   java -XX:+UseSerialGC -Xmx128m -cp tools/bench GenR4W5ThreadsOomProbe
 *   timeout 300 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx128m -cp tools/bench GenR4W5ThreadsOomProbe
 *   timeout 300 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx128m --nojit -cp tools/bench GenR4W5ThreadsOomProbe
 *   CRATONVM_DBG_GC_OVERHEAD=1 timeout 300 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx128m --nojit --verbose:gc -cp tools/bench GenR4W5ThreadsOomProbe
 * </pre>
 * A timeout, a {@code FATAL:} line or a missing line is a failure; so is a
 * CratonVM run taking more than about 60 s per shape. The {@code --verbose:gc}
 * run is the diagnosis: the base binary shows {@code young=16973824->13565952}
 * on every cycle (13 carves of filler freed, live flat); the fixed one shows
 * the young live set growing between cycles until the error. A/B levers, same
 * binary: {@code CRATONVM_MONITOR_ENTER_SPIN=0} (no spin) and
 * {@code CRATONVM_GC_OVERHEAD_PROGRESS=0} (no progress half).
 */
public final class GenR4W5ThreadsOomProbe {
    static final class Node {
        final Node next;
        final long a, b, c, d;

        Node(Node next, long v) {
            this.next = next;
            this.a = v;
            this.b = v + 1;
            this.c = v + 2;
            this.d = v + 3;
        }
    }

    static volatile Node shared;
    static final Object LOCK = new Object();
    static boolean ok = true;

    static synchronized void link(long v) {
        shared = new Node(shared, v);
    }

    static synchronized void clearShared() {
        shared = null;
    }

    /** The heap must be usable again: allocate and use ~10 MB of short-lived objects. */
    static void recovered(String what) {
        long sum = 0;
        for (int i = 0; i < 200_000; i++) {
            final Node n = new Node(null, i);
            sum += n.d;
        }
        final boolean good = sum == 200_000L * 199_999L / 2 + 3L * 200_000L;
        System.out.println(what + "-recovered " + (good ? "ok" : "FAILED sum=" + sum));
        ok &= good;
    }

    static void run(String what, int threads, boolean method) throws InterruptedException {
        final AtomicInteger saw = new AtomicInteger();
        final AtomicInteger finished = new AtomicInteger();
        final Runnable filler = () -> {
            try {
                long v = 0;
                while (true) {
                    final Node n = new Node(null, v++);
                    if (method) {
                        link(n.a);
                    } else {
                        synchronized (LOCK) {
                            shared = new Node(shared, n.a);
                        }
                    }
                }
            } catch (OutOfMemoryError e) {
                if (method) {
                    clearShared();
                } else {
                    synchronized (LOCK) {
                        shared = null;
                    }
                }
                saw.incrementAndGet();
            } finally {
                finished.incrementAndGet();
            }
        };
        final Thread[] ts = new Thread[threads];
        for (int i = 0; i < threads; i++) {
            ts[i] = new Thread(filler, what + "-" + (i + 1));
        }
        for (Thread t : ts) {
            t.start();
        }
        for (Thread t : ts) {
            t.join();
        }
        shared = null;
        final boolean good = saw.get() >= 1 && finished.get() == threads;
        System.out.println(what + ": " + (saw.get() >= 1 ? "OutOfMemoryError seen" : "no OutOfMemoryError")
                + ", " + finished.get() + " of " + threads + " finished");
        ok &= good;
        recovered(what);
    }

    public static void main(String[] args) throws Exception {
        run("threads", 2, false);
        run("threads-method", 2, true);
        run("threads-4", 4, false);
        System.out.println(ok ? "PASS" : "FAIL");
        if (!ok) {
            System.exit(1);
        }
    }
}
