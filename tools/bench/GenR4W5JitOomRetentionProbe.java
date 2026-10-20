// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.List;

/**
 * gen r4w5/oomjit5 (2026-09-24): after an {@code OutOfMemoryError} thrown from
 * COMPILED code, data the program drops must be collectable. The two failing
 * steps of wave 4 made standalone, and made deterministic by forcing the JIT
 * with the compile-threshold flags instead of relying on warm-up timing:
 * <ul>
 *   <li>{@code GenR4W4HeapFullThrashProbe} step {@code chain-hot}: the OOME is
 *       thrown inside compiled {@code grow}, the caller catches it, sets
 *       {@code head = null}, and its {@code println} threw a second, uncaught
 *       OOME on CratonVM (HotSpot recovers);</li>
 *   <li>{@code GenR4W4NativeStringOomProbe}: the same after its fill loop and
 *       {@code fill = null}.</li>
 * </ul>
 * Page: {@code docs/internal/gaps/gengc-r4w4-final-oome-from-compiled-code-leaves-dropped-data-reachable-20260924.md}.
 *
 * <p>Three steps, each a different catching frame:
 * <ul>
 *   <li>{@code chain-hot}: the loop and the {@code catch} are in
 *       {@link #chainHot()}, a method of its own, so {@code CRATONVM_JIT_THRESHOLD=1}
 *       compiles it (and {@code grow}) at the first call — the catch runs in
 *       compiled code;</li>
 *   <li>{@code chain-hot-main}: the original shape, loop and {@code catch}
 *       inline in {@code main}, which only OSR (or a threshold-1 compile of
 *       {@code main} itself) puts in compiled code;</li>
 *   <li>{@code list-hot}: the list shape — {@code fill.add(new long[128])}
 *       until the heap is full, a sliver released, then {@code fill = null}.</li>
 * </ul>
 * After each drop, {@code recovered} allocates ~10 MB of short-lived objects
 * and prints; a heap that still holds the dropped data cannot, and throws
 * again. A second OOME is caught in {@code main} and reported as
 * {@code FAILED} so one run shows every step.
 *
 * <p>HotSpot ({@code -XX:+UseSerialGC -Xmx128m}) prints:
 * <pre>
 *   chain-hot: OutOfMemoryError "Java heap space"
 *   chain-hot-recovered ok
 *   chain-hot-main: OutOfMemoryError "Java heap space"
 *   chain-hot-main-recovered ok
 *   list-hot: OutOfMemoryError "Java heap space"
 *   list-hot-recovered ok
 *   PASS
 * </pre>
 * Commands (a 300 s timeout is generous):
 * <pre>
 *   javac -d tools/bench tools/bench/GenR4W5JitOomRetentionProbe.java
 *   java -XX:+UseSerialGC -Xmx128m -cp tools/bench GenR4W5JitOomRetentionProbe
 *   # every method compiled at its first call:
 *   CRATONVM_JIT_THRESHOLD=1 timeout 300 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx128m -cp tools/bench GenR4W5JitOomRetentionProbe
 *   # main's loop entered through OSR at its first back edge:
 *   CRATONVM_JIT_OSR=1 CRATONVM_TIER_OSR_BACKEDGE=1 timeout 300 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx128m -cp tools/bench GenR4W5JitOomRetentionProbe
 *   # the control, which passes:
 *   timeout 300 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx128m --nojit -cp tools/bench GenR4W5JitOomRetentionProbe
 *   # the diagnosis: which root category keeps the dropped data alive
 *   CRATONVM_DBG=oldmark-root-census,root-source CRATONVM_JIT_THRESHOLD=1 timeout 600 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx128m -cp tools/bench GenR4W5JitOomRetentionProbe 2>census.log
 * </pre>
 * A {@code FAILED} line, a missing line, a timeout or a process abort is a
 * failure. In the census log, read the {@code [oldmark-root-census]} block of
 * the LAST major before a {@code FAILED} line: the category with the dropped
 * data's bytes is the holder.
 */
public final class GenR4W5JitOomRetentionProbe {
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

    static Node head;
    static List<long[]> fill;
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

    /** One batch of the hot shape; the throw site of the compiled OOME. */
    static Node grow(Node from, long v, int n) {
        Node h = from;
        for (int i = 0; i < n; i++) {
            h = new Node(h, v + i);
        }
        return h;
    }

    /** {@code chain-hot}: loop and catch in a method compiled at its first call. */
    static void chainHot() {
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
    }

    /** {@code list-hot}: the list shape of {@code GenR4W4NativeStringOomProbe}. */
    static void listHot() {
        fill = new ArrayList<>();
        try {
            while (true) {
                fill.add(new long[128]);
            }
        } catch (OutOfMemoryError e) {
            // Release a sliver so the println below can run.
            for (int i = 0; i < 64 && !fill.isEmpty(); i++) {
                fill.remove(fill.size() - 1);
            }
            report("list-hot", e);
        }
        fill = null;
        recovered("list-hot");
    }

    static void failed(String what) {
        head = null;
        fill = null;
        ok = false;
        System.out.println(what + ": FAILED, a second OutOfMemoryError after the data was dropped");
    }

    public static void main(String[] args) {
        try {
            chainHot();
        } catch (OutOfMemoryError again) {
            failed("chain-hot");
        }

        try {
            try {
                long v = 0;
                while (true) {
                    head = grow(head, v, 1024);
                    v += 1024;
                }
            } catch (OutOfMemoryError e) {
                head = null;
                report("chain-hot-main", e);
            }
            recovered("chain-hot-main");
        } catch (OutOfMemoryError again) {
            failed("chain-hot-main");
        }

        try {
            listHot();
        } catch (OutOfMemoryError again) {
            failed("list-hot");
        }

        System.out.println(ok ? "PASS" : "FAIL");
        if (!ok) {
            System.exit(1);
        }
    }
}
