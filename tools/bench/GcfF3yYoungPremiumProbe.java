import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.util.HashMap;

/**
 * gcf f3/y: JIT-warm short-lived-allocation kernels, the shapes of the young
 * allocation "premium" (JIT round 15's {@code R15GcpageYoungPremium}, whose source
 * is not in this tree), for the pinned-young-copy default flip's perf rows.
 *
 * <p>Every kernel runs warm-up passes (so it is compiled and its young cycles are
 * JIT-warm term-4 cycles), then timed passes. stdout carries ONE deterministic
 * line per kernel ({@code PREMIUM kernel=K ops=N checksum=C}, = HotSpot) and a
 * final {@code PREMIUM_OK}; stderr carries the timing
 * ({@code [premium] kernel=K ns_per_op=X gc_ms=Y gc_count=Z}, wall and the
 * collector MXBeans' time over the timed passes only).
 *
 * <ul>
 *   <li>{@code alloc-dead}: a 4-field object allocated, read, dropped.
 *   <li>{@code alloc-ring}: stored into a 16-slot young ring, dies 16 later.
 *   <li>{@code alloc-medium}: stored into a 64K-slot ring -- survives about one
 *       young cycle (the survivor-copy / tenuring shape).
 *   <li>{@code hashmap-pair}: {@code HashMap<Integer,Integer>} put/get of fresh
 *       boxes, the map replaced every 65536 puts.
 *   <li>{@code old-store}: fresh objects stored into a long-lived (old) array.
 * </ul>
 *
 * <p>Usage: {@code GcfF3yYoungPremiumProbe [opsPerPass] [passes]} (defaults
 * 4000000, 5; warm-up is 2 passes).
 */
public final class GcfF3yYoungPremiumProbe {

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

    static Cell[] OLD_TABLE;

    static long allocDead(int n, int seed) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            Cell c = new Cell(i, seed, null);
            s += c.a + c.c;
        }
        return s;
    }

    static long allocRing(int n, int seed) {
        Cell[] ring = new Cell[16];
        long s = 0;
        for (int i = 0; i < n; i++) {
            Cell old = ring[i & 15];
            if (old != null) {
                s += old.a;
            }
            ring[i & 15] = new Cell(i, seed, old == null ? null : old.next);
        }
        return s;
    }

    static long allocMedium(int n, int seed) {
        Cell[] ring = new Cell[65536];
        long s = 0;
        for (int i = 0; i < n; i++) {
            int k = i & 65535;
            Cell old = ring[k];
            if (old != null) {
                s += old.b ^ old.a;
            }
            ring[k] = new Cell(i, seed, null);
        }
        return s;
    }

    static long hashmapPair(int n, int seed) {
        HashMap<Integer, Integer> m = new HashMap<>();
        long s = 0;
        for (int i = 0; i < n; i++) {
            if ((i & 65535) == 0) {
                m = new HashMap<>();
            }
            int k = (int) ((i * 2654435761L) % 100003);
            m.put(k, i ^ seed);
            Integer v = m.get(k);
            if (v != null) {
                s += v;
            }
        }
        return s + m.size();
    }

    static long oldStore(int n, int seed) {
        Cell[] t = OLD_TABLE;
        long s = 0;
        for (int i = 0; i < n; i++) {
            int k = i & (t.length - 1);
            Cell old = t[k];
            if (old != null) {
                s += old.a;
            }
            t[k] = new Cell(i, seed, null);
        }
        return s;
    }

    static long run(String kernel, int n, int seed) {
        switch (kernel) {
            case "alloc-dead":
                return allocDead(n, seed);
            case "alloc-ring":
                return allocRing(n, seed);
            case "alloc-medium":
                return allocMedium(n, seed);
            case "hashmap-pair":
                return hashmapPair(n, seed);
            case "old-store":
                return oldStore(n, seed);
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
        int passes = args.length > 1 ? Integer.parseInt(args[1]) : 5;
        OLD_TABLE = new Cell[1 << 16];
        for (int i = 0; i < 3; i++) {
            System.gc(); // promote the table: the old-store kernel's holder is old
        }
        String[] kernels = {"alloc-dead", "alloc-ring", "alloc-medium", "hashmap-pair", "old-store"};
        for (String k : kernels) {
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
                    "[premium] kernel=%s ns_per_op=%.2f gc_ms=%d gc_count=%d%n",
                    k, nsPerOp, g1[0] - g0[0], g1[1] - g0[1]);
            System.out.println("PREMIUM kernel=" + k + " ops=" + ops + " checksum=" + check);
        }
        System.out.println("PREMIUM_OK");
    }
}
