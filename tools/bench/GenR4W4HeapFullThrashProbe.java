// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.atomic.AtomicInteger;

/**
 * gen r4w4/oom (2026-09-24): a heap filled with LIVE objects must end in a
 * catchable {@code OutOfMemoryError}, promptly, and the heap must be usable
 * again once the program drops the data. The fix under test is
 * {@code gen_heap::young_trigger_floor_after_collection}: when the young
 * trigger's anti-livelock floor is capped and the old generation is wedged,
 * the occupancy trigger stands down until the next completed collection, so
 * the young generation fills, the allocation fails, and the ladder throws.
 * Before it, every cycle freed one TLAB carve and the program never returned
 * ({@code docs/internal/gc/gengc-r4-final-oom-heap-full-thrash-never-throws-FIXED-20260924.md}).
 *
 * <p>Four shapes, each followed by a recovery check:
 * <ul>
 *   <li>{@code chain}: an unbounded reachable linked list built in
 *       {@code main} (interpreted), the orchestrator's measured shape;</li>
 *   <li>{@code chain-hot}: the same list built by a method called in a loop,
 *       so a JIT build compiles it and allocates through the JIT helpers;</li>
 *   <li>{@code arrays}: reachable {@code long[64]} blocks (the array
 *       allocation path);</li>
 *   <li>{@code threads}: two threads filling one shared list; at least one of
 *       them must see the error and both must finish.</li>
 * </ul>
 * Each shape prints one line whatever the timing, and each must finish in
 * well under the timeout below. The elapsed time is NOT printed (it is not
 * comparable), but a CratonVM run taking more than about 30 s per shape is a
 * failure of the fix even if the lines match.
 *
 * <p>HotSpot ({@code -XX:+UseSerialGC -Xmx128m}) prints:
 * <pre>
 *   chain: OutOfMemoryError "Java heap space"
 *   chain-recovered ok
 *   chain-hot: OutOfMemoryError "Java heap space"
 *   chain-hot-recovered ok
 *   arrays: OutOfMemoryError "Java heap space"
 *   arrays-recovered ok
 *   threads: OutOfMemoryError seen, 2 of 2 finished
 *   threads-recovered ok
 *   PASS
 * </pre>
 * Commands (a 300 s timeout is generous; HotSpot needs about 5 s):
 * <pre>
 *   java -XX:+UseSerialGC -Xmx128m -cp tools/bench GenR4W4HeapFullThrashProbe
 *   timeout 300 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx128m -cp tools/bench GenR4W4HeapFullThrashProbe
 *   timeout 300 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx128m --nojit -cp tools/bench GenR4W4HeapFullThrashProbe
 *   timeout 300 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx128m --verbose:gc -cp tools/bench GenR4W4HeapFullThrashProbe
 * </pre>
 * The {@code --verbose:gc} run is the diagnosis: the base binary shows an
 * endless run of {@code moving+major} cycles with identical {@code young=}
 * and {@code old=} figures; the fixed one shows a handful of cycles at a
 * FULL young generation before each OutOfMemoryError. A timeout, a process
 * abort, a {@code FATAL:} line or a missing line is a failure. Parallel GC
 * may say {@code "GC overhead limit exceeded"}; use Serial as the oracle.
 */
public final class GenR4W4HeapFullThrashProbe {
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

    static final class Block {
        final Block next;
        final long[] data;

        Block(Block next) {
            this.next = next;
            this.data = new long[64];
        }
    }

    static Node head;
    static Block blocks;
    static volatile Node shared;
    static boolean ok = true;

    static void report(String what, OutOfMemoryError e) {
        final String msg = e.getMessage();
        System.out.println(what + ": OutOfMemoryError \"" + msg + "\"");
        ok &= "Java heap space".equals(msg);
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

    /** One batch of the hot shape; small enough to be called many times. */
    static Node grow(Node from, long v, int n) {
        Node h = from;
        for (int i = 0; i < n; i++) {
            h = new Node(h, v + i);
        }
        return h;
    }

    public static void main(String[] args) throws Exception {
        // 1. The measured shape, interpreted.
        try {
            long v = 0;
            while (true) {
                head = new Node(head, v++);
            }
        } catch (OutOfMemoryError e) {
            head = null;
            report("chain", e);
        }
        recovered("chain");

        // 2. The same, from a hot method.
        try {
            long v = 0;
            while (true) {
                head = grow(head, v, 1024);
                v += 1024;
            }
        } catch (OutOfMemoryError e) {
            head = null;
            report("chain-hot", e);
        }
        recovered("chain-hot");

        // 3. Arrays.
        try {
            while (true) {
                blocks = new Block(blocks);
            }
        } catch (OutOfMemoryError e) {
            blocks = null;
            report("arrays", e);
        }
        recovered("arrays");

        // 4. Two threads, one shared list.
        final AtomicInteger saw = new AtomicInteger();
        final AtomicInteger finished = new AtomicInteger();
        final Object lock = new Object();
        final Runnable filler = () -> {
            try {
                long v = 0;
                while (true) {
                    final Node n = new Node(null, v++);
                    synchronized (lock) {
                        shared = new Node(shared, n.a);
                    }
                }
            } catch (OutOfMemoryError e) {
                synchronized (lock) {
                    shared = null;
                }
                saw.incrementAndGet();
            } finally {
                finished.incrementAndGet();
            }
        };
        final Thread t1 = new Thread(filler, "filler-1");
        final Thread t2 = new Thread(filler, "filler-2");
        t1.start();
        t2.start();
        t1.join();
        t2.join();
        shared = null;
        final boolean threadsOk = saw.get() >= 1 && finished.get() == 2;
        System.out.println("threads: " + (saw.get() >= 1 ? "OutOfMemoryError seen" : "no OutOfMemoryError")
                + ", " + finished.get() + " of 2 finished");
        ok &= threadsOk;
        recovered("threads");

        System.out.println(ok ? "PASS" : "FAIL");
        if (!ok) {
            System.exit(1);
        }
    }
}
