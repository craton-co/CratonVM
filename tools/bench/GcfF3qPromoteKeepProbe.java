import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.util.HashMap;

/**
 * gcf f3/q: JIT-warm PROMOTING young-cycle kernels -- the survivor-heavy half of
 * the young allocation premium, which lane y's {@code GcfF3yYoungPremiumProbe}
 * (dead / ring / one-cycle survivors) does not cover. On a JIT-warm Generational
 * process every young cycle is the in-place sweep, and these shapes make every
 * one of them promote: they exercise the selective-promotion walk's census
 * ({@code CRATONVM_GEN_SWEEP_BATCH_CENSUS}), fix-up 3a
 * ({@code CRATONVM_GEN_SWEEP_3A_REPLAY}), fix-up 3c over a large old holder
 * ({@code CRATONVM_GEN_SWEEP_3C_LISTED_SLOTS}) and the evacuation map
 * ({@code CRATONVM_GEN_SWEEP_EVAC_RESERVE}), with dead objects interleaved for the
 * walk size memo ({@code CRATONVM_GEN_WALK_SIZE_MEMO}).
 *
 * <p>stdout: one deterministic line per kernel
 * ({@code PROMOTE kernel=K ops=N checksum=C}, = HotSpot) and a final
 * {@code PROMOTE_OK}. stderr: {@code [promote] kernel=K ns_per_op=X gc_ms=Y
 * gc_count=Z} over the timed passes only.
 *
 * <ul>
 *   <li>{@code young-keep}: every allocation is a 4-field cell; one in eight is
 *       kept in a 1M-slot ring (it lives ~8M allocations: ages, promotes, dies
 *       old); the other seven die young, interleaved with the kept ones.
 *   <li>{@code hashmap-grow}: CratonBench {@code hashmap}'s put phase -- fresh
 *       {@code Integer} boxes put into ONE map grown to 1M entries (every node
 *       and box survives and promotes; the table is a large old array whose
 *       dirty cards 3c reads), then a get pass, then the map is dropped.
 *   <li>{@code tree-keep}: a long-lived depth-16 binary tree rebuilt one subtree
 *       at a time, with short-lived depth-6 trees allocated between (the
 *       bintrees shape: mixed promotion and churn).
 * </ul>
 *
 * <p>Usage: {@code GcfF3qPromoteKeepProbe [opsPerPass] [passes]} (defaults
 * 4000000, 4; warm-up is 2 passes).
 */
public final class GcfF3qPromoteKeepProbe {

    static final class Cell {
        int a;
        int b;
        Cell next;
        long c;

        Cell(int a, int b, Cell next) {
            this.a = a;
            this.b = b;
            this.next = next;
            this.c = (long) a * b;
        }
    }

    static final class Node {
        Node left;
        Node right;
        int v;

        Node(Node left, Node right, int v) {
            this.left = left;
            this.right = right;
            this.v = v;
        }
    }

    static Cell[] KEEP_RING;

    static long youngKeep(int n, int seed) {
        Cell[] ring = KEEP_RING;
        int mask = ring.length - 1;
        long s = 0;
        int k = 0;
        for (int i = 0; i < n; i++) {
            Cell c = new Cell(i, seed, null);
            if ((i & 7) == 0) {
                Cell old = ring[k];
                if (old != null) {
                    s += old.a ^ old.b;
                }
                ring[k] = c;
                k = (k + 1) & mask;
            } else {
                s += c.c & 0xFF;
            }
        }
        return s;
    }

    static long hashmapGrow(int n, int seed) {
        int size = Math.min(n, 1 << 20);
        long s = 0;
        for (int done = 0; done < n; done += size) {
            HashMap<Integer, Integer> m = new HashMap<>();
            int stop = Math.min(size, n - done);
            for (int i = 0; i < stop; i++) {
                m.put(i * 31 + seed, i ^ seed);
            }
            for (int i = 0; i < stop; i += 3) {
                Integer v = m.get(i * 31 + seed);
                if (v != null) {
                    s += v;
                }
            }
            s += m.size();
        }
        return s;
    }

    static Node build(int depth, int v) {
        if (depth == 0) {
            return new Node(null, null, v);
        }
        return new Node(build(depth - 1, 2 * v), build(depth - 1, 2 * v + 1), v);
    }

    static long check(Node t) {
        if (t == null) {
            return 0;
        }
        return t.v + check(t.left) - check(t.right);
    }

    static Node LONG_TREE;

    static long treeKeep(int n, int seed) {
        // One unit of work = one short-lived depth-6 tree (127 nodes).
        int units = Math.max(1, n / 127);
        long s = 0;
        for (int i = 0; i < units; i++) {
            Node shortTree = build(6, i + seed);
            s += check(shortTree);
            if ((i & 63) == 0) {
                // Replace one depth-10 subtree of the long-lived tree: its old
                // nodes die old, the new ones age in young and promote.
                Node p = LONG_TREE;
                int path = i >>> 6;
                for (int d = 0; d < 5; d++) {
                    p = ((path >>> d) & 1) == 0 ? p.left : p.right;
                }
                if (((path >>> 5) & 1) == 0) {
                    p.left = build(10, i);
                } else {
                    p.right = build(10, i);
                }
            }
        }
        return s + check(LONG_TREE);
    }

    static long run(String kernel, int n, int seed) {
        switch (kernel) {
            case "young-keep":
                return youngKeep(n, seed);
            case "hashmap-grow":
                return hashmapGrow(n, seed);
            case "tree-keep":
                return treeKeep(n, seed);
            default:
                throw new IllegalArgumentException(kernel);
        }
    }

    static long[] gcTotals() {
        long ms = 0;
        long count = 0;
        try {
            for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
                ms += Math.max(0, b.getCollectionTime());
                count += Math.max(0, b.getCollectionCount());
            }
        } catch (Throwable t) {
            return new long[] {-1, -1};
        }
        return new long[] {ms, count};
    }

    public static void main(String[] args) {
        int ops = args.length > 0 ? Integer.parseInt(args[0]) : 4_000_000;
        int passes = args.length > 1 ? Integer.parseInt(args[1]) : 4;
        String only = args.length > 2 ? args[2] : null;
        KEEP_RING = new Cell[1 << 20];
        LONG_TREE = build(16, 1);
        String[] kernels = {"young-keep", "hashmap-grow", "tree-keep"};
        for (String k : kernels) {
            if (only != null && !only.equals(k)) {
                continue;
            }
            long check = 0;
            for (int w = 0; w < 2; w++) {
                check = check * 31 + run(k, ops, w);
            }
            long[] g0 = gcTotals();
            long t0 = System.nanoTime();
            for (int p = 0; p < passes; p++) {
                check = check * 31 + run(k, ops, 100 + p);
            }
            long t1 = System.nanoTime();
            long[] g1 = gcTotals();
            double nsPerOp = (t1 - t0) / ((double) ops * passes);
            System.err.printf(
                    "[promote] kernel=%s ns_per_op=%.2f gc_ms=%d gc_count=%d%n",
                    k, nsPerOp, g1[0] - g0[0], g1[1] - g0[1]);
            System.out.println("PROMOTE kernel=" + k + " ops=" + ops + " checksum=" + check);
        }
        System.out.println("PROMOTE_OK");
    }
}
