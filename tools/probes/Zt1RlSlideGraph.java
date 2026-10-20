/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 */

import java.lang.ref.WeakReference;
import java.util.ArrayList;

/**
 * zt t1/rl: a relocation probe whose stdout is deterministic.
 *
 * Each round allocates n nodes and keeps one in four on a linked list (each
 * survivor carries a small int[] whose contents depend on its id), so the
 * survivors sit on sparse pages between garbage and the selector moves them.
 * Every 64th survivor is also tracked through a WeakReference and its
 * identity hash. Two System.gc() calls later the probe walks the list and
 * checks every survivor's id order and array contents, that each weak
 * referent is still the same object as the strong one (the reference
 * processor read the cycle's PointerMap), and that each identity hash is
 * unchanged across the move.
 *
 * It is the row probe for the zt t1/rl switches that touch the slide's
 * output: CRATONVM_ZGC_POINTER_MAP_SORTED, CRATONVM_ZGC_RELOCATIONS_CEILING_SCALED,
 * CRATONVM_DBG_ZGC_QUARANTINE_DEAD_CHECK (with CRATONVM_ZGC_SLIDE_EVACUATE=1 and
 * CRATONVM_ZGC_EVAC_QUARANTINE=1) and CRATONVM_ZGC_RELOCATE_POOL. The GC-side
 * readings are on stderr (`[GC] zgc-relocate:`, `[GC] zgc-uncommitter:`).
 *
 * Output (HotSpot prints exactly these lines for the same arguments):
 *   rounds R errors 0
 *   checksum C
 *
 * Run: java -XX:+UseZGC -Xmx256m Zt1RlSlideGraph [rounds] [nodes]
 */
public class Zt1RlSlideGraph {
    static final class Node {
        Node next;
        int[] arr;
        long id;

        Node(long id) {
            this.id = id;
        }
    }

    static volatile Object sink;

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 20;
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 200_000;
        long errors = 0;
        long checksum = 0;
        for (int r = 0; r < rounds; r++) {
            Node head = null;
            ArrayList<Node> tracked = new ArrayList<>();
            ArrayList<WeakReference<Node>> weaks = new ArrayList<>();
            ArrayList<Integer> hashes = new ArrayList<>();
            long kept = 0;
            for (int i = 0; i < n; i++) {
                if ((i & 3) == 0) {
                    Node x = new Node(i);
                    int len = i % 13;
                    x.arr = new int[len];
                    for (int k = 0; k < len; k++) {
                        x.arr[k] = i + k;
                    }
                    x.next = head;
                    head = x;
                    if ((kept & 63) == 0) {
                        tracked.add(x);
                        weaks.add(new WeakReference<>(x));
                        hashes.add(System.identityHashCode(x));
                    }
                    kept++;
                } else {
                    sink = new long[(i % 7) + 1];
                }
            }
            sink = null;
            System.gc();
            System.gc();
            long expect = (long) (n - 1) & ~3L;
            long seen = 0;
            for (Node x = head; x != null; x = x.next) {
                if (x.id != expect) {
                    errors++;
                }
                int len = (int) (x.id % 13);
                if (x.arr == null || x.arr.length != len) {
                    errors++;
                } else {
                    for (int k = 0; k < len; k++) {
                        if (x.arr[k] != (int) x.id + k) {
                            errors++;
                        }
                    }
                }
                checksum += x.id;
                expect -= 4;
                seen++;
            }
            if (seen != kept) {
                errors++;
            }
            for (int t = 0; t < tracked.size(); t++) {
                Node strong = tracked.get(t);
                if (weaks.get(t).get() != strong) {
                    errors++;
                }
                if (System.identityHashCode(strong) != hashes.get(t)) {
                    errors++;
                }
            }
        }
        System.out.println("rounds " + rounds + " errors " + errors);
        System.out.println("checksum " + checksum);
    }
}
