// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gen r4w3/hunter (2026-09-23): heap exhaustion on the generational backend
 * must surface as a catchable {@code OutOfMemoryError} with HotSpot's
 * messages, and the VM must be usable again once the application drops the
 * data — for SMALL young allocations (the path that must run a full GC and the
 * last-ditch soft clear before giving up), for a single array larger than the
 * heap, and for an array larger than the VM limit.
 *
 * <p>Step 0 ({@code old-garbage}) is the generational-specific one: two large
 * arrays (which CratonVM's Generational backend allocates straight into the
 * old generation) are dropped, leaving the old generation ~60% GARBAGE — below
 * the 75% major-GC trigger — and then one array larger than the old
 * generation's remaining free space is requested. HotSpot collects and
 * succeeds. Before gen r4w3/hunter's `last_ditch_reclaim` fix, CratonVM ran
 * only young collections on that ladder and threw {@code OutOfMemoryError}.
 *
 * <p>Deterministic output on HotSpot Serial and G1 (Parallel may answer step 1
 * with {@code "GC overhead limit exceeded"}; use {@code -XX:+UseSerialGC} as
 * the oracle, it is the collector this backend models):
 * <pre>
 *   old-garbage ok
 *   small: OutOfMemoryError "Java heap space"
 *   small-recovered ok
 *   big: OutOfMemoryError "Java heap space"
 *   limit: OutOfMemoryError "Requested array size exceeds VM limit"
 *   PASS
 * </pre>
 * Commands:
 * <pre>
 *   java -XX:+UseSerialGC -Xmx128m -cp tools/bench GenR4W3OomProbe
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx128m -cp tools/bench GenR4W3OomProbe
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx128m -XX:+HeapDumpOnOutOfMemoryError \
 *       -XX:HeapDumpPath=/tmp/gen-oom.hprof -cp tools/bench GenR4W3OomProbe
 * </pre>
 * CratonVM currently appends a diagnostic suffix to the message
 * ({@code docs/internal/gc-common-round-20260923/common-f-oome-message-carries-vm-suffix-FIXED-20260923.md},
 * owned by the gc-common round), so the three message lines differ there
 * until that lands.
 * With {@code -XX:+HeapDumpOnOutOfMemoryError} HotSpot additionally prints
 * {@code Dumping heap to /tmp/gen-oom.hprof ...} once (for the first OOM only)
 * and writes the file. A hang, a process abort, or a missing/garbled message
 * is a failure.
 */
public final class GenR4W3OomProbe {
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
    static volatile Object sink;
    static boolean ok = true;

    static void report(String what, OutOfMemoryError e, String expected) {
        final String msg = e.getMessage();
        System.out.println(what + ": OutOfMemoryError \"" + msg + "\"");
        ok &= expected.equals(msg);
    }

    static void fillThenDrop(int part) {
        byte[] a = new byte[part];
        byte[] b = new byte[part];
        a[a.length - 1] = 1;
        b[b.length - 1] = 1;
        sink = a;
        sink = b;
        sink = null;
    }

    public static void main(String[] args) {
        // 0. Old generation mostly garbage, below the major-GC trigger.
        final long max = Runtime.getRuntime().maxMemory();
        final int part = (int) Math.min(Integer.MAX_VALUE - 8, max * 5 / 32);
        final int big = (int) Math.min(Integer.MAX_VALUE - 8, max * 17 / 64);
        fillThenDrop(part); // its own frame, so no dead local can root the arrays
        try {
            byte[] c = new byte[big];
            c[c.length - 1] = 1;
            sink = c;
            sink = null;
            System.out.println("old-garbage ok");
        } catch (OutOfMemoryError e) {
            sink = null;
            System.out.println("old-garbage FAILED: OutOfMemoryError with ~" + (2L * part >> 20)
                    + " MiB of dropped arrays reclaimable (asked " + (big >> 20) + " MiB)");
            ok = false;
        }

        // 1. Many small objects, all reachable: young fills, promotes, old fills.
        try {
            long v = 0;
            while (true) {
                head = new Node(head, v++);
            }
        } catch (OutOfMemoryError e) {
            head = null; // drop everything first, so the println can allocate
            report("small", e, "Java heap space");
        }

        // 2. The heap must be usable again.
        long sum = 0;
        for (int i = 0; i < 200_000; i++) {
            final Node n = new Node(null, i);
            sum += n.d;
        }
        final boolean recovered = sum == 200_000L * 199_999L / 2 + 3L * 200_000L;
        System.out.println("small-recovered " + (recovered ? "ok" : "FAILED sum=" + sum));
        ok &= recovered;

        // 3. One array larger than the whole heap.
        try {
            sink = new long[512 * 1024 * 1024 / 8]; // 512 MiB
            System.out.println("big: no error (heap larger than 512 MiB?)");
            ok = false;
        } catch (OutOfMemoryError e) {
            report("big", e, "Java heap space");
        }

        // 4. Past the VM's array-length limit.
        try {
            sink = new long[Integer.MAX_VALUE];
            System.out.println("limit: no error");
            ok = false;
        } catch (OutOfMemoryError e) {
            report("limit", e, "Requested array size exceeds VM limit");
        }

        System.out.println(ok ? "PASS" : "FAIL");
        if (!ok) {
            System.exit(1);
        }
    }
}
