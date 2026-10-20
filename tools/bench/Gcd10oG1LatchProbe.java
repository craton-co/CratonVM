// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gcd d10/o retirement (2026-10-05): drives G1's LATCHED GC-overhead exit at
 * each allocation door, so its one exit ({@code g1_latched_overhead_exit}: a
 * marking cycle, judged, then one attempt after a productive cycle or on the
 * bounded grace) is exercised where it lives.
 * {@code docs/internal/gc/gcd-d10o-g1-latched-overhead-exit-skips-the-marking-cycle-FIXED-20261005.md},
 * {@code docs/internal/gc/gcg-g3o-g1-latched-overhead-exit-differs-across-the-three-doors-FIXED-20261005.md}.
 *
 * <p>Each round fills the heap with SMALL live nodes (no humongous object, so
 * G1's Old regions fill to within a sliver and the overhead streak can latch,
 * which {@code GenR4W4NativeStringOomProbe}'s 1 KiB blocks never achieve on
 * G1: it throws with over 6 % of the heap free), catches the
 * {@code OutOfMemoryError}, drops everything, and allocates once through one
 * door:
 * <ul>
 * <li>{@code interp} -- a method that has never run, so it is interpreted;</li>
 * <li>{@code jit} -- a method warmed beforehand, so it runs compiled;</li>
 * <li>{@code native} -- natives that allocate ({@code getStackTrace},
 *     {@code getDeclaredMethods});</li>
 * <li>{@code exception} -- a VM-created {@code ArithmeticException}.</li>
 * </ul>
 * The dropped data sits in Old regions that only a marking cycle reclaims, so
 * the allocation succeeds exactly when the door ran the cycle. Prints
 * {@code fill <door>: OutOfMemoryError "Java heap space"} and
 * {@code after <door>: ok} per round, then {@code PASS}; HotSpot 25 G1 prints
 * the same. The streak latches on G1 only under an explicit limit (HotSpot 25's
 * default has none off Parallel):
 * <pre>
 *   CRATONVM_GC_OVERHEAD_LIMIT=5 CRATONVM_DBG_GC_OVERHEAD=1 \
 *     cratonvm -XX:+UseG1GC -Xmx128m -cp out Gcd10oG1LatchProbe
 * </pre>
 * and each round shows one {@code [GC_OVERHEAD] g1-overhead-cycle:} line with
 * {@code productive=true} (the drop), after the fill's futile ones.
 */
public final class Gcd10oG1LatchProbe {
    static final class Node {
        Node next;
        final long[] pad = new long[14];
    }

    static Node head;
    static volatile Object sink;
    static boolean ok = true;

    static int fill() {
        int n = 0;
        try {
            while (true) {
                Node x = new Node();
                x.next = head;
                head = x;
                n++;
            }
        } catch (OutOfMemoryError e) {
            head = null;
            return -1 - n;
        }
    }

    /** Warmed before the rounds, so its allocation runs compiled. */
    static Node hotAlloc(Node next) {
        Node x = new Node();
        x.next = next;
        return x;
    }

    /** Never called before its round: interpreted. */
    static Object coldAlloc() {
        Node a = new Node();
        a.next = new Node();
        return a;
    }

    static Object nativeAlloc() {
        Object[] two = new Object[2];
        two[0] = new Throwable().getStackTrace();
        two[1] = Gcd10oG1LatchProbe.class.getDeclaredMethods();
        return two;
    }

    static int divide(int a, int b) {
        return a / b;
    }

    static Object exceptionAlloc() {
        try {
            sink = divide(1, 0);
            return null;
        } catch (ArithmeticException e) {
            return e;
        }
    }

    static void round(String door) {
        int r = fill();
        // `head` is already null: everything the fill made is garbage now.
        String msg = r < 0 ? "OutOfMemoryError \"Java heap space\"" : "no OutOfMemoryError";
        Object got;
        try {
            switch (door) {
                case "interp":
                    got = coldAlloc();
                    break;
                case "jit":
                    got = hotAlloc(null);
                    break;
                case "native":
                    got = nativeAlloc();
                    break;
                default:
                    got = exceptionAlloc();
                    break;
            }
        } catch (OutOfMemoryError e) {
            got = null;
        }
        sink = got;
        boolean good = r < 0 && got != null;
        ok &= good;
        System.out.println("fill " + door + ": " + msg);
        System.out.println("after " + door + ": " + (got != null ? "ok" : "OutOfMemoryError"));
    }

    public static void main(String[] args) {
        Node warm = null;
        for (int i = 0; i < 200_000; i++) {
            warm = hotAlloc((i & 1023) == 0 ? null : warm);
            sink = divideSafe(i);
        }
        sink = warm;
        warm = null;
        String[] doors = args.length > 0 ? args : new String[] {"interp", "jit", "native", "exception"};
        for (String d : doors) {
            round(d);
        }
        System.out.println(ok ? "PASS" : "FAIL");
        if (!ok) {
            System.exit(1);
        }
    }

    static int divideSafe(int i) {
        return divide(i, (i & 7) + 1);
    }
}
