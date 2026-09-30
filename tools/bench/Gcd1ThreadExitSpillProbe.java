// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;

/**
 * gcd d2/j (2026-09-27): two threads fill the heap with ONE live chain through
 * a {@code static synchronized} method until each catches an
 * {@code OutOfMemoryError}, then clear the chain and exit. The shape of
 * {@code GenR4W6JitOomRootProbe}'s {@code oome-thread-exit}, alone.
 *
 * <p>The loser of the class monitor blocks inside the compiled
 * {@code monitorenter} helper, a helper window that keeps every young
 * collection NON-moving, so young fills with live {@code Node}s nothing can
 * promote. The JIT's {@code new} used to force a young cycle BEFORE each
 * allocation that missed young and then spill the object to the old
 * generation anyway: one futile collection per {@code Node}, and the
 * GC-overhead limit could not end it because the old generation was far from
 * full (`docs/internal/gc/gcd-d1b-thread-exit-shape-livelocks-on-forced-young-cycles-FIXED-20260928.md`).
 * The futile-young backoff ({@code CRATONVM_GC_FUTILE_YOUNG_BACKOFF}, default
 * on) spills first while the last forced cycle was futile.
 *
 * <p>The probe ends on its own: {@code main} waits at most
 * {@link #DEADLINE_MS} for the fillers and then halts with exit code 2 after
 * printing {@code FAIL thread-exit: fillers still running}.
 *
 * <p>HotSpot ({@code java -XX:+UseSerialGC -Xmx64m -cp tools/bench Gcd1ThreadExitSpillProbe})
 * prints, in this order:
 * <pre>
 *   thread-exit: OutOfMemoryError seen, 2 of 2 finished
 *   thread-exit-recovered ok
 *   PASS
 * </pre>
 * and exits 0.
 *
 * <p>Commands:
 * <pre>
 *   javac -d tools/bench tools/bench/Gcd1ThreadExitSpillProbe.java
 *   P="--java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp tools/bench"
 *   cratonvm $P Gcd1ThreadExitSpillProbe
 *   CRATONVM_GC_FUTILE_YOUNG_BACKOFF=0 cratonvm $P Gcd1ThreadExitSpillProbe   # control
 *   cratonvm $P --nojit Gcd1ThreadExitSpillProbe
 * </pre>
 */
public final class Gcd1ThreadExitSpillProbe {
    /** How long {@code main} waits for the two fillers before it gives up. */
    static final long DEADLINE_MS = 240_000L;

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

    static volatile Node sharedHead;
    static volatile int oomes;
    static long sink;

    static synchronized void link(long v) {
        sharedHead = new Node(sharedHead, v);
    }

    static synchronized void clearShared() {
        sharedHead = null;
        oomes++;
    }

    /** 24 MB of fresh, short-lived allocation must succeed. */
    static boolean usable() {
        try {
            long s = 0;
            for (int i = 0; i < 24; i++) {
                final long[] chunk = new long[128 * 1024];
                chunk[i] = i;
                s += chunk[i];
            }
            sink += s;
            return true;
        } catch (OutOfMemoryError again) {
            return false;
        }
    }

    public static void main(String[] args) throws InterruptedException {
        sharedHead = new Node(null, -1);
        final WeakReference<Node> ref = new WeakReference<>(sharedHead);
        final Runnable filler = () -> {
            try {
                long v = 0;
                while (true) {
                    link(v++);
                }
            } catch (OutOfMemoryError e) {
                clearShared();
            }
        };
        final Thread t1 = new Thread(filler, "spill-filler-1");
        final Thread t2 = new Thread(filler, "spill-filler-2");
        t1.start();
        t2.start();
        final long deadline = System.currentTimeMillis() + DEADLINE_MS;
        t1.join(Math.max(1L, deadline - System.currentTimeMillis()));
        t2.join(Math.max(1L, deadline - System.currentTimeMillis()));
        if (t1.isAlive() || t2.isAlive()) {
            System.out.println("FAIL thread-exit: fillers still running");
            Runtime.getRuntime().halt(2);
        }
        sharedHead = null;
        final int seen = oomes;
        boolean cleared = false;
        for (int i = 0; i < 3 && !cleared; i++) {
            System.gc();
            cleared = ref.get() == null;
        }
        final boolean usable = cleared && usable();
        final boolean ok = seen == 2 && cleared && usable;
        // gcd d4/o: which half failed, on stderr only (stdout stays HotSpot's):
        // `cleared=false` is a root still reaching the chain; `cleared=true
        // usable=false` is an allocation door failing on a heap whose data is
        // unreachable.
        System.err.println("[probe] thread-exit cleared=" + cleared + " usable=" + usable);
        System.out.println("thread-exit: OutOfMemoryError seen, " + seen + " of 2 finished");
        System.out.println(ok ? "thread-exit-recovered ok" : "thread-exit-recovered FAILED");
        System.out.println(ok ? "PASS" : "FAIL");
        if (!ok) {
            System.exit(1);
        }
    }
}
