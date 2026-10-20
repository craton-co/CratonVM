// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zgc round zr, wave z3, lane mt (2026-10-12).
//
// The graph shapes that decide the three STW mark-loop switches of zr z3/mt
// (`ZMarkLoopOpts`, gc/src/zgc.rs) beyond the binary tree of Zr1MkParmarkCost:
//
//   hubs   a linked list of `n` Leaf objects, each holding `next` plus three
//          references into 64 shared Hub objects. Three edges in four point at
//          an object that is already marked: the best case for
//          CRATONVM_ZGC_MARK_TEST_FIRST (`mark_first_skips=` on
//          `[GC] zgc-mark-serial:`).
//   mixed  a linked list cycling through four classes with different field
//          shapes: every object is a class change, the WORST case for
//          CRATONVM_ZGC_MARK_LAYOUT_MEMO (`memo_hits=` ~ 0, `memo_misses=` ~
//          objects). The memo must not cost CPU here.
//   tree   a complete binary tree of 2^k - 1 nodes of one class (`n` = k): the
//          best case for the memo (memo_hits ~ objects).
//
// A churn loop allocates short-lived garbage for `seconds` so the collector
// runs on its own; the live structure is verified at the end. Every arm and
// HotSpot must print the same stdout line.
//
// Usage: Zr3MtMarkShapes <hubs|mixed|tree> <n> <seconds>
// Output:
//   ZR3MT-SHAPES shape=<s> objects=<o> checksum=<c> ok
//   (stderr) ZR3MT-SHAPES gc_count=<n> gc_ms=<t> wall_ms=<w>
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;

public class Zr3MtMarkShapes {
    static final class Hub {
        final int id;
        long pad0, pad1;
        Hub(int id) { this.id = id; }
    }

    static final class Leaf {
        Leaf next;
        Hub a, b, c;
        int seq;
    }

    // Four field shapes for the `mixed` list. Each has `next` (as Object) and
    // a different number / position of primitive fields.
    static final class M0 { Object next; int v; }
    static final class M1 { long x; Object next; int v; }
    static final class M2 { int y; int v; Object next; long z; }
    static final class M3 { Object next; Object self; int v; double w; }

    static final class T {
        T left, right;
        final int depth;
        T(int depth) { this.depth = depth; }
    }

    static T build(int d) {
        T t = new T(d);
        if (d > 1) {
            t.left = build(d - 1);
            t.right = build(d - 1);
        }
        return t;
    }

    static Object hubs;  // keeps the live structure reachable
    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        String shape = args.length > 0 ? args[0] : "hubs";
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 1_000_000;
        int seconds = args.length > 2 ? Integer.parseInt(args[2]) : 10;
        long t0 = System.nanoTime();

        Object live;
        switch (shape) {
            case "hubs": {
                Hub[] hub = new Hub[64];
                for (int i = 0; i < hub.length; i++) hub[i] = new Hub(i);
                Leaf head = null;
                for (int i = 0; i < n; i++) {
                    Leaf l = new Leaf();
                    l.seq = i;
                    l.a = hub[i & 63];
                    l.b = hub[(i * 7) & 63];
                    l.c = hub[(i * 13) & 63];
                    l.next = head;
                    head = l;
                }
                hubs = hub;
                live = head;
                break;
            }
            case "mixed": {
                Object head = null;
                for (int i = 0; i < n; i++) {
                    switch (i & 3) {
                        case 0: { M0 m = new M0(); m.v = i; m.next = head; head = m; break; }
                        case 1: { M1 m = new M1(); m.v = i; m.next = head; head = m; break; }
                        case 2: { M2 m = new M2(); m.v = i; m.next = head; head = m; break; }
                        default: { M3 m = new M3(); m.v = i; m.self = m; m.next = head; head = m; break; }
                    }
                }
                live = head;
                break;
            }
            case "tree":
                live = build(n);
                break;
            default:
                throw new IllegalArgumentException("shape must be hubs, mixed or tree: " + shape);
        }

        // Churn: short-lived garbage so the collector triggers on its own.
        long deadline = System.nanoTime() + seconds * 1_000_000_000L;
        long churned = 0;
        while (System.nanoTime() < deadline) {
            for (int i = 0; i < 10_000; i++) {
                sink = new long[16];
            }
            churned++;
        }

        long objects = 0;
        long checksum = 0;
        switch (shape) {
            case "hubs":
                for (Leaf l = (Leaf) live; l != null; l = l.next) {
                    objects++;
                    checksum += l.a.id + l.b.id + l.c.id;
                }
                break;
            case "mixed": {
                Object o = live;
                while (o != null) {
                    objects++;
                    if (o instanceof M0) { checksum += ((M0) o).v; o = ((M0) o).next; }
                    else if (o instanceof M1) { checksum += ((M1) o).v; o = ((M1) o).next; }
                    else if (o instanceof M2) { checksum += ((M2) o).v; o = ((M2) o).next; }
                    else { M3 m = (M3) o; if (m.self != m) throw new AssertionError("self"); checksum += m.v; o = m.next; }
                }
                break;
            }
            default: {
                java.util.ArrayDeque<T> st = new java.util.ArrayDeque<>();
                st.push((T) live);
                while (!st.isEmpty()) {
                    T t = st.pop();
                    objects++;
                    checksum += t.depth;
                    if (t.left != null) st.push(t.left);
                    if (t.right != null) st.push(t.right);
                }
            }
        }
        System.out.println("ZR3MT-SHAPES shape=" + shape + " objects=" + objects
                + " checksum=" + checksum + " ok");

        long gcCount = 0, gcMs = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            gcCount += Math.max(0, b.getCollectionCount());
            gcMs += Math.max(0, b.getCollectionTime());
        }
        System.err.println("ZR3MT-SHAPES gc_count=" + gcCount + " gc_ms=" + gcMs
                + " wall_ms=" + (System.nanoTime() - t0) / 1_000_000 + " churn=" + churned);
    }
}
