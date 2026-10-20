// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zgc round zr, wave z7, lane cm (2026-10-13).
//
// The FIXED-WORK twin of Zr1MkParmarkCost. That probe runs for a fixed WALL
// time with 1 + <mutators> busy threads, so its process CPU is roughly
// (1 + mutators) x (wall - pause sum) + GC CPU: an arm that pauses LESS lets
// the mutators run (and burn CPU) for longer, and its CPU goes UP with no
// extra GC work at all. With 4 mutators and a pause sum 4.3 s shorter that is
// ~21 s of CPU -- the whole +16-17 % the z3 host rows charged to concurrent
// marking. This probe does a fixed amount of work instead, so its CPU is
// comparable across arms:
//
//   * main allocates exactly <allocM> million short-lived Node objects (plus a
//     16 KiB array every 64 Ki allocations, as Zr1MkParmarkCost does);
//   * each of <mutators> threads performs exactly <swapsK> thousand subtree
//     swaps in the live tree (two reference stores into old objects each: the
//     SATB barrier's load);
//   * then the tree is verified (node count + depth checksum, invariant under
//     the swaps).
//
// It also prints the CPU the Java threads themselves used (ThreadMXBean), so
// `process CPU - java_cpu_ms` is the collector's share (GC workers, pauses).
//
// Usage: Zr7CmFixedWork <depth> <allocM> <swapsK> <mutators>
// Output:
//   ZR7CM-FIXED depth=<d> nodes=<2^d-1> checksum=<c> allocs=<a> swaps=<s> ok
//   (stderr) ZR7CM-FIXED gc_count=<n> gc_ms=<t> wall_ms=<w> java_cpu_ms=<c> main_cpu_ms=<m>
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.util.SplittableRandom;

public class Zr7CmFixedWork {
    static final class Node {
        Node left, right;
        final int depth;
        Node(int depth) { this.depth = depth; }
    }

    static Node build(int d) {
        Node n = new Node(d);
        if (d > 1) {
            n.left = build(d - 1);
            n.right = build(d - 1);
        }
        return n;
    }

    static long[] countAndChecksum(Node n) {
        long c = 0, s = 0;
        java.util.ArrayDeque<Node> st = new java.util.ArrayDeque<>();
        st.push(n);
        while (!st.isEmpty()) {
            Node x = st.pop();
            c++;
            s += x.depth;
            if (x.left != null) st.push(x.left);
            if (x.right != null) st.push(x.right);
        }
        return new long[] {c, s};
    }

    static long threadCpuMs(ThreadMXBean tb, long id) {
        long ns = tb.getThreadCpuTime(id);
        return ns < 0 ? 0 : ns / 1_000_000L;
    }

    public static void main(String[] args) throws Exception {
        final int depth = args.length > 0 ? Integer.parseInt(args[0]) : 20;
        final long allocs = (args.length > 1 ? Long.parseLong(args[1]) : 50) * 1_000_000L;
        final long swapsEach = (args.length > 2 ? Long.parseLong(args[2]) : 2000) * 1_000L;
        final int mutators = args.length > 3 ? Integer.parseInt(args[3]) : 4;
        final ThreadMXBean tb = ManagementFactory.getThreadMXBean();
        long t0 = System.nanoTime();
        final Node root = build(depth);
        final long[] cpu = new long[mutators];
        final long[] done = new long[mutators];
        Thread[] ts = new Thread[mutators];
        for (int m = 0; m < mutators; m++) {
            final int id = m;
            ts[m] = new Thread(() -> {
                SplittableRandom rnd = new SplittableRandom(0x5eed + id);
                long n = 0;
                while (n < swapsEach) {
                    Node a = root;
                    int steps = 1 + rnd.nextInt(Math.max(1, depth - 4));
                    for (int i = 0; i < steps && a.left != null; i++) a = rnd.nextBoolean() ? a.left : a.right;
                    Node l = a.left, r = a.right;
                    if (l == null || r == null || l.left == null || r.left == null) continue;
                    synchronized (a) {
                        Node tmp = l.left;
                        l.left = r.left;
                        r.left = tmp;
                    }
                    n++;
                }
                done[id] = n;
                cpu[id] = threadCpuMs(tb, Thread.currentThread().getId());
            });
            ts[m].start();
        }
        Object[] junk = new Object[4096];
        for (long i = 1; i <= allocs; i++) {
            junk[(int) (i & 4095)] = new Node(0);
            if ((i & 0xFFFF) == 0) junk[(int) ((i >>> 16) & 4095)] = new byte[16 * 1024];
        }
        long mainCpu = threadCpuMs(tb, Thread.currentThread().getId());
        long swaps = 0, javaCpu = mainCpu;
        for (int m = 0; m < mutators; m++) {
            ts[m].join();
            swaps += done[m];
            javaCpu += cpu[m];
        }
        long[] cs = countAndChecksum(root);
        long wantNodes = (1L << depth) - 1;
        long wantSum = 0;
        for (int d = 1; d <= depth; d++) wantSum += (long) d * (1L << (depth - d));
        long gcCount = 0, gcMs = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            gcCount += Math.max(0, b.getCollectionCount());
            gcMs += Math.max(0, b.getCollectionTime());
        }
        boolean ok = cs[0] == wantNodes && cs[1] == wantSum && swaps == swapsEach * mutators;
        System.out.println("ZR7CM-FIXED depth=" + depth + " nodes=" + cs[0] + " checksum=" + cs[1]
                + " allocs=" + allocs + " swaps=" + swaps + (ok ? " ok" : " BAD"));
        System.err.println("ZR7CM-FIXED gc_count=" + gcCount + " gc_ms=" + gcMs + " wall_ms="
                + (System.nanoTime() - t0) / 1_000_000 + " java_cpu_ms=" + javaCpu
                + " main_cpu_ms=" + mainCpu);
    }
}
