// zr zs s1/mc: CPU of the whole-heap STW mark, per collection, on three live-set shapes.
//
// Builds a live set of `liveMb` MiB, then runs `rounds` rounds of: replace every 4th
// retained object (fresh garbage + new survivors on every page), then System.gc().
//
//   prim   int[64] arrays held by one ArrayList (the Zr2CpCapacity `grow` / near-OOM shape:
//          every marked object is a primitive array -- the CRATONVM_ZGC_MARK_ONE_DECODE arm)
//   tree   a balanced binary tree of small compact objects (two references and a long):
//          reference-heavy, every marked object is an ObjectKind::Object
//   wide   Object[1024] blocks of small objects that each hold an int[4]: reference
//          arrays, objects and primitive arrays interleaved
//
// STDOUT is deterministic (identical on every JVM, so the harness can compare it with
// HotSpot's byte for byte):
//   zs1mc shape=<s> live_mb=<n> rounds=<r> objects=<k> check=<c>
// STDERR carries the timing, which the row reads but never compares:
//   zs1mc-cpu shape=<s> cpu_ms=<t> cpu_ms_per_round=<x> java_cpu_ms=<j> gcs=<g>
// `cpu_ms` is process CPU (user+sys, com.sun.management) over the rounds; `java_cpu_ms` is
// the ThreadMXBean CPU of the live Java threads over the same span (the collecting thread is
// one of them, so it is NOT a clean mutator share -- read the `[GC] zgc-mark-parallel:`
// `cpu_us=` / `[GC] zgc-mark-serial:` `cpu_us=` lines for the mark's own CPU).
//
// Usage: Zs1McMarkCpu <prim|tree|wide|all> [liveMb=96] [rounds=20]
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.util.ArrayList;
import java.util.List;

public class Zs1McMarkCpu {
    static final class Node {
        Node left;
        Node right;
        long v;
        Node(long v) { this.v = v; }
    }

    static final class Holder {
        int[] data;
        long v;
        Holder(long v) {
            this.v = v;
            this.data = new int[4];
            this.data[0] = (int) v;
            this.data[3] = (int) (v * 3);
        }
    }

    static long gcCount() {
        long n = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            long c = b.getCollectionCount();
            if (c > 0) n += c;
        }
        return n;
    }

    static long cpuNanos() {
        java.lang.management.OperatingSystemMXBean os = ManagementFactory.getOperatingSystemMXBean();
        if (os instanceof com.sun.management.OperatingSystemMXBean) {
            return ((com.sun.management.OperatingSystemMXBean) os).getProcessCpuTime();
        }
        return -1;
    }

    static long javaCpuNanos() {
        ThreadMXBean t = ManagementFactory.getThreadMXBean();
        long sum = 0;
        for (long id : t.getAllThreadIds()) {
            long c = t.getThreadCpuTime(id);
            if (c > 0) sum += c;
        }
        return sum;
    }

    // ---- prim -------------------------------------------------------------
    static int[] prim(int i) {
        int[] a = new int[64];
        a[0] = i;
        a[63] = i * 7;
        return a;
    }

    // ---- tree -------------------------------------------------------------
    static Node build(int depth, long[] next) {
        Node n = new Node(next[0]++);
        if (depth > 0) {
            n.left = build(depth - 1, next);
            n.right = build(depth - 1, next);
        }
        return n;
    }

    static long walk(Node n) {
        long sum = 0;
        // Iterative: depth stays small (balanced), but avoid recursion limits anyway.
        ArrayList<Node> stack = new ArrayList<>();
        stack.add(n);
        while (!stack.isEmpty()) {
            Node x = stack.remove(stack.size() - 1);
            sum += x.v * 3 + (x.left != null ? 1 : 0);
            if (x.left != null) stack.add(x.left);
            if (x.right != null) stack.add(x.right);
        }
        return sum;
    }

    static void run(String shape, int liveMb, int rounds) {
        long objects;
        List<Object> live = new ArrayList<>();
        Node[] tops = null;
        int n;
        if (shape.equals("prim")) {
            n = liveMb * 3800; // ~272 B per int[64]
            for (int i = 0; i < n; i++) live.add(prim(i));
            objects = n;
        } else if (shape.equals("tree")) {
            // 64 subtrees of depth d: 64 * (2^(d+1) - 1) nodes of ~32 B.
            int perTree = Math.max(1, liveMb * 32768 / 64);
            int d = 0;
            while ((2 << d) - 1 < perTree) d++;
            tops = new Node[64];
            long[] next = {0};
            for (int t = 0; t < 64; t++) tops[t] = build(d, next);
            objects = next[0];
            n = 64;
        } else {
            // wide: Object[1024] blocks of Holder (~32 B + int[4] ~32 B).
            int blocks = Math.max(1, liveMb * 16);
            long k = 0;
            for (int b = 0; b < blocks; b++) {
                Object[] block = new Object[1024];
                for (int j = 0; j < block.length; j++) block[j] = new Holder(k++);
                live.add(block);
            }
            objects = k;
            n = blocks;
        }
        System.gc();
        long g0 = gcCount();
        long c0 = cpuNanos();
        long j0 = javaCpuNanos();
        long[] next = {1_000_000_000L};
        for (int r = 0; r < rounds; r++) {
            if (shape.equals("prim")) {
                for (int i = r & 3; i < n; i += 4) live.set(i, prim(i));
            } else if (shape.equals("tree")) {
                // Replace one quarter of the subtrees' left halves with fresh copies.
                for (int t = r & 3; t < 64; t += 4) {
                    Node top = tops[t];
                    int depth = 0;
                    for (Node x = top.left; x != null; x = x.left) depth++;
                    long base = next[0];
                    long[] mirror = {base};
                    Node fresh = build(depth - 1 < 0 ? 0 : depth - 1, mirror);
                    // Keep the values identical on every JVM: renumber from the old subtree.
                    renumber(fresh, top.left);
                    top.left = fresh;
                }
            } else {
                for (int b = r & 3; b < n; b += 4) {
                    Object[] block = (Object[]) live.get(b);
                    for (int j = 0; j < block.length; j++) {
                        Holder h = (Holder) block[j];
                        block[j] = new Holder(h.v);
                    }
                }
            }
            System.gc();
        }
        long c1 = cpuNanos();
        long j1 = javaCpuNanos();
        long g1 = gcCount();

        long check = 0;
        if (shape.equals("prim")) {
            for (Object o : live) {
                int[] a = (int[]) o;
                check += a[0] + 3L * a[63];
            }
        } else if (shape.equals("tree")) {
            for (Node t : tops) check += walk(t);
        } else {
            for (Object o : live) {
                for (Object e : (Object[]) o) {
                    Holder h = (Holder) e;
                    check += h.v + h.data[0] + 5L * h.data[3];
                }
            }
        }
        System.out.printf(java.util.Locale.ROOT,
                "zs1mc shape=%s live_mb=%d rounds=%d objects=%d check=%d%n",
                shape, liveMb, rounds, objects, check);
        double cpuMs = (c1 - c0) / 1e6;
        System.err.printf(java.util.Locale.ROOT,
                "zs1mc-cpu shape=%s cpu_ms=%.0f cpu_ms_per_round=%.1f java_cpu_ms=%.0f gcs=%d%n",
                shape, cpuMs, cpuMs / rounds, (j1 - j0) / 1e6, g1 - g0);
    }

    // Copy `old`'s values into `fresh` (same shape: both are full subtrees of one depth).
    static void renumber(Node fresh, Node old) {
        ArrayList<Node> a = new ArrayList<>();
        ArrayList<Node> b = new ArrayList<>();
        a.add(fresh);
        b.add(old);
        while (!a.isEmpty()) {
            Node x = a.remove(a.size() - 1);
            Node y = b.remove(b.size() - 1);
            if (y == null) {
                continue;
            }
            x.v = y.v;
            if (x.left != null) { a.add(x.left); b.add(y.left); }
            if (x.right != null) { a.add(x.right); b.add(y.right); }
        }
    }

    public static void main(String[] args) {
        String which = args.length > 0 ? args[0] : "all";
        int liveMb = args.length > 1 ? Integer.parseInt(args[1]) : 96;
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 20;
        for (String s : new String[] {"prim", "tree", "wide"}) {
            if (which.equals("all") || which.equals(s)) {
                run(s, liveMb, rounds);
            }
        }
    }
}
