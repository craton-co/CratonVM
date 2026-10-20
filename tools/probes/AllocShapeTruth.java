import com.sun.management.ThreadMXBean;
import java.lang.management.ManagementFactory;
import java.util.*;
import java.util.function.IntFunction;

/**
 * Per-shape allocation cost with a GROUND TRUTH beside it.
 *
 * `AllocShapeProbe` reports what the allocation counter says each shape costs,
 * which is the right question only once the counter itself is trusted. It was
 * not: `getTotalThreadAllocatedBytes` over-reported by 2-3x for one reason and
 * under-reported the entire native allocation surface for another, and a
 * per-shape table read off it named mechanisms that were partly instrument.
 * See docs/known-issues/hibernate/ for what that cost.
 *
 * This probe RETAINS every object it allocates and reads the live set, so each
 * row carries an independent number the counter can be checked against. A row
 * where `retained` and `counted` agree is a real cost; a row where they do not
 * is a counter defect, and the probe says which.
 *
 * Shapes that allocate transitively (an iteration that builds temporaries) are
 * not measurable this way -- they retain nothing -- so this probe deliberately
 * covers only CONSTRUCTORS, whose product is exactly what is held.
 *
 * Usage: AllocShapeTruth [iters]
 */
public class AllocShapeTruth {
    static final ThreadMXBean TMX = (ThreadMXBean) ManagementFactory.getThreadMXBean();
    static Object[] keep;

    public static void main(String[] a) throws Exception {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 200000;
        System.out.println(String.format("%-28s %10s %10s %10s %8s",
                "shape", "retained", "perThread", "process", "ratio"));
        measure("plain Object", n, i -> new Object());
        measure("Integer.valueOf(large)", n, i -> Integer.valueOf(1_000_000 + i));
        measure("Object[4]", n, i -> new Object[4]);
        measure("int[8]", n, i -> new int[8]);
        measure("byte[64]", n, i -> new byte[64]);
        measure("ArrayList empty", n, i -> new ArrayList<String>());
        measure("ArrayList 4 adds", n, i -> {
            ArrayList<Integer> l = new ArrayList<>();
            for (int j = 0; j < 4; j++) l.add(j);
            return l;
        });
        measure("HashMap empty", n, i -> new HashMap<String, String>());
        measure("HashMap 4 entries", n, i -> {
            HashMap<Integer, Integer> m = new HashMap<>();
            for (int j = 0; j < 4; j++) m.put(j, j);
            return m;
        });
        measure("HashSet empty", n, i -> new HashSet<String>());
        measure("LinkedHashMap empty", n, i -> new LinkedHashMap<String, String>());
        measure("String(new, 16 chars)", n, i -> new String(new char[16]));
        measure("StringBuilder.toString", n, i -> new StringBuilder().append("ab").append(i).toString());
        System.out.println("SHAPETRUTH_END");
    }

    static void measure(String name, int n, IntFunction<Object> make) throws Exception {
        for (int i = 0; i < 2000; i++) make.apply(i);      // warm, unmeasured
        Runtime rt = Runtime.getRuntime();
        Object[] hold = new Object[n];                      // outside the window
        keep = hold;
        settle(rt);
        long h0 = rt.totalMemory() - rt.freeMemory();
        long p0 = TMX.getCurrentThreadAllocatedBytes();
        long q0 = TMX.getTotalThreadAllocatedBytes();
        for (int i = 0; i < n; i++) hold[i] = make.apply(i);
        long pt = TMX.getCurrentThreadAllocatedBytes() - p0;
        long pr = TMX.getTotalThreadAllocatedBytes() - q0;
        settle(rt);
        long retained = (rt.totalMemory() - rt.freeMemory()) - h0;
        if (keep.length != n) throw new IllegalStateException("unreachable");
        double r = retained / (double) n;
        double c = pt / (double) n;
        System.out.println(String.format(Locale.ROOT, "%-28s %10.1f %10.1f %10.1f %8s",
                name, r, c, pr / (double) n,
                r <= 0 ? "n/a" : String.format(Locale.ROOT, "%.2f", c / r)));
        keep = null;
    }

    static void settle(Runtime rt) throws Exception {
        for (int i = 0; i < 2; i++) { rt.gc(); Thread.sleep(50); }
    }
}
