// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: ZGC's helper-first post-init `new` under
// contention (`CRATONVM_JIT_IR_ZGC_POST_INIT_HELPER_FIRST`, default ON; page
// `r21-zgc-ir-post-init-new-commits-before-it-registers-20261018.md`: a peer
// frozen between the commit and `jit_post_tlab_init`'s registration owned a
// committed, unregistered object).
//
// Risk targeted: the reordered sequence (helper with the cursor uncommitted,
// then the commit) must stay correct when the helper itself is the slow
// path's GC point: four threads allocate object graphs from compiled `new`s
// of finalizer-free classes with several reference and primitive fields
// (nested `new` inside a constructor, so a second post-init runs while the
// first object is only half built), keep a share of them alive across
// collections forced by a fifth thread, and verify every surviving object's
// fields and its identityHashCode taken at birth. A TLAB-sized burst
// (objects larger than a few hundred bytes) forces refills mid-burst. The
// checksums are exact and independent of scheduling.
//
// Run:
//   javac -d out R21Fz2ZgcNew2.java
//   java -Xmx512m -XX:+UseZGC -cp out R21Fz2ZgcNew2                       # HotSpot 25
//   cratonvm -Xmx512m -XX:+UseZGC -cp out R21Fz2ZgcNew2
//   CRATONVM_JIT_IR_ZGC_POST_INIT_HELPER_FIRST=0 cratonvm -Xmx512m -XX:+UseZGC -cp out R21Fz2ZgcNew2 (kill)
//   CRATONVM_ZGC_SKIP_COVERS_INFLIGHT=0 cratonvm -Xmx512m -XX:+UseZGC ... (the GC-side cover off: the fix alone)
//   cratonvm -Xmx128m -XX:+UseZGC ... (more cycles); cratonvm -Xmx512m -cp out ..., -XX:+UseG1GC ...
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

import java.util.ArrayList;
import java.util.List;

public class R21Fz2ZgcNew2 {
    static final int THREADS = 4;
    static final int PER_THREAD = 150_000;
    static volatile boolean stop;

    static final class Leaf {
        final int a;
        final long b;
        Object c;

        Leaf(int a) {
            this.a = a;
            this.b = (long) a * 7919;
        }
    }

    static final class Node {
        final Leaf left;
        final Leaf right;
        final int id;
        final int hash;
        double weight;
        Node link;

        Node(int id, Node link) {
            this.left = new Leaf(id);
            this.right = new Leaf(-id);
            this.id = id;
            this.link = link;
            this.weight = id * 0.5;
            this.hash = System.identityHashCode(this);
            left.c = this;
        }
    }

    static final class Big {
        final long[] pad = new long[48];
        final int id;

        Big(int id) {
            this.id = id;
            pad[47] = id;
        }
    }

    static long verify(List<Node> kept) {
        long s = 0;
        for (Node n : kept) {
            boolean ok = n.left.a == n.id && n.right.a == -n.id && n.left.b == n.id * 7919L && n.left.c == n
                    && n.right.c == null && n.weight == n.id * 0.5 && n.hash == System.identityHashCode(n)
                    && (n.link == null || n.link.id == n.id - 1);
            s = s * 31 + (ok ? n.id : -1_000_000);
        }
        return s;
    }

    static long work(int t) {
        List<Node> kept = new ArrayList<>();
        Node prev = null;
        long bigs = 0;
        for (int i = 0; i < PER_THREAD; i++) {
            Node n = new Node(i, prev);
            prev = (i & 1) == 0 ? n : null;
            if (i % 97 == 0) {
                kept.add(n);
                prev = null;
            }
            if ((i & 63) == 0) {
                bigs += new Big(i).pad[47];
            }
        }
        return verify(kept) * 31 + bigs + t * 0;
    }

    public static void main(String[] args) throws Exception {
        System.out.println("single " + work(0));
        Thread gc = new Thread(() -> {
            while (!stop) {
                System.gc();
                try {
                    Thread.sleep(3);
                } catch (InterruptedException e) {
                    return;
                }
            }
        });
        gc.start();
        for (int round = 0; round < 3; round++) {
            long[] res = new long[THREADS];
            Thread[] ts = new Thread[THREADS];
            for (int t = 0; t < THREADS; t++) {
                final int id = t;
                ts[t] = new Thread(() -> res[id] = work(id));
                ts[t].start();
            }
            for (Thread t : ts) {
                t.join();
            }
            StringBuilder sb = new StringBuilder("round " + round);
            for (long r : res) {
                sb.append(' ').append(r);
            }
            System.out.println(sb);
        }
        stop = true;
        gc.join();
        System.out.println("after " + work(1));
    }
}
