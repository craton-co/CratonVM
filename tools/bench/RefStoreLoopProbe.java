// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * A tight, JIT-friendly loop whose body is almost nothing but REFERENCE STORES
 * into young receivers.
 *
 * This is the shape the compiled reference-store barrier gates exist for. Under
 * `-XX:+UseGenerationalGC` each `node.next = other` used to be a full
 * `jit_putfield_object` call: a JIT/Rust boundary notification, a receiver
 * plausibility check, an `ObjectRef` construction, a slot bounds check, then
 * `set_field` and its write barrier. With a barrier plan published, a store
 * whose receiver is YOUNG (`GC_FLAG_OLD_GEN` clear) skips the call entirely.
 *
 * Why not `BinTreesClassic`: its hot method is recursive, so its reference
 * stores are spread over allocation and recursion and it spends most of its
 * time elsewhere. A flat counted loop over a preallocated array compiles as a
 * single method and does nothing else, which is what makes the barrier the
 * measurable term rather than a rounding error.
 *
 * Deliberately allocation-free in the steady state: the nodes are allocated
 * once, up front. An arm that collects more is measuring the collector, not
 * the barrier.
 *
 * Usage: RefStoreLoopProbe [nodes] [rounds]
 *   nodes   receivers cycled through (default 4096)
 *   rounds  passes over the array (default 20000)
 *
 * The checksum is a pure function of the arguments.
 */
public final class RefStoreLoopProbe {
    static final class Node {
        Node next;
        Node other;
        int v;
        Node(int v) { this.v = v; }
    }

    /** The hot method: nothing but reference stores and an int add. */
    static long churn(Node[] a, int rounds) {
        long acc = 0;
        int n = a.length;
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < n; i++) {
                Node cur = a[i];
                Node nxt = a[(i + 1) % n];
                cur.next = nxt;
                cur.other = a[(i + 7) % n];
                acc += cur.v;
            }
        }
        return acc;
    }

    public static void main(String[] args) {
        int nodes = args.length > 0 ? Integer.parseInt(args[0]) : 4096;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 20000;

        Node[] a = new Node[nodes];
        for (int i = 0; i < nodes; i++) {
            a[i] = new Node(i);
        }

        long acc = churn(a, rounds);

        // Every store must be readable back, or a "faster" arm is one that
        // dropped writes.
        long walk = 0;
        for (int i = 0; i < nodes; i++) {
            Node cur = a[i];
            if (cur.next != a[(i + 1) % nodes] || cur.other != a[(i + 7) % nodes]) {
                throw new IllegalStateException("a reference store was lost at " + i);
            }
            walk += cur.next.v + cur.other.v;
        }

        System.out.println("nodes=" + nodes + " rounds=" + rounds
                + " acc=" + acc + " walk=" + walk);
        System.out.println("PASS RefStoreLoopProbe");
    }
}
