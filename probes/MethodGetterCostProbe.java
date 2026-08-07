import java.lang.reflect.Method;
import java.lang.reflect.Modifier;

/**
 * Per-call cost of the four `java.lang.reflect.Method` getters that Spring's
 * `AnnotationsScanner.isOverride` runs in its inner loop.
 *
 * A native-invocation census of one `MethodIntrospector.selectMethods` pass
 * over `org.jooq.impl.DefaultDSLContext` (1003 declared methods) counted:
 *
 *   Method.getParameterCount  3 075 043
 *   Method.getModifiers       1 549 873
 *   Method.getName              942 430
 *
 * in 74.6 s of wall clock, so what matters is the cost of ONE call, and
 * whether it depends on how many methods the declaring class has — a getter
 * that re-derives its answer from the declaring class's method table is
 * O(declared methods) per call, which this probe makes visible by sweeping
 * the type.
 *
 * Everything reported is ns/call measured inside one process, so it is valid
 * on a loaded host and comparable across VMs.
 */
public class MethodGetterCostProbe {

    public static void main(String[] args) throws Exception {
        String[] types = args.length > 0
                ? args
                : new String[] {"Wide125", "Wide250", "Wide500", "Wide1000",
                                "org.jooq.impl.DefaultDSLContext"};
        System.out.printf("%-38s %8s %10s %10s %10s %10s %12s%n",
                "type", "n", "getName", "getModif", "getParamC", "getParamT", "isOverride");
        for (String t : types) {
            Class<?> c;
            try {
                c = Class.forName(t);
            } catch (Throwable e) {
                System.out.println(t + " SKIP " + e);
                continue;
            }
            Method[] ms = c.getDeclaredMethods();
            Method m = ms[0];
            int iters = 20000;

            // warm
            for (int i = 0; i < 2000; i++) {
                sink += m.getName().length() + m.getModifiers() + m.getParameterCount();
            }

            long t0 = System.nanoTime();
            for (int i = 0; i < iters; i++) sink += m.getName().length();
            double nName = (System.nanoTime() - t0) / (double) iters;

            t0 = System.nanoTime();
            for (int i = 0; i < iters; i++) sink += m.getModifiers();
            double nMod = (System.nanoTime() - t0) / (double) iters;

            t0 = System.nanoTime();
            for (int i = 0; i < iters; i++) sink += m.getParameterCount();
            double nPc = (System.nanoTime() - t0) / (double) iters;

            t0 = System.nanoTime();
            for (int i = 0; i < iters; i++) sink += m.getParameterTypes().length;
            double nPt = (System.nanoTime() - t0) / (double) iters;

            // the real inner loop: Spring's AnnotationsScanner.isOverride
            int pairs = Math.min(ms.length, 200);
            t0 = System.nanoTime();
            int hits = 0;
            for (int i = 0; i < pairs; i++) {
                for (int j = 0; j < pairs; j++) {
                    Method a = ms[i], b = ms[j];
                    if (!Modifier.isPrivate(b.getModifiers())
                            && b.getName().equals(a.getName())
                            && b.getParameterCount() == a.getParameterCount()) {
                        hits++;
                    }
                }
            }
            double nOv = (System.nanoTime() - t0) / (double) (pairs * pairs);
            sink += hits;

            System.out.printf("%-38s %8d %10.0f %10.0f %10.0f %10.0f %12.0f%n",
                    t, ms.length, nName, nMod, nPc, nPt, nOv);
            System.out.flush();
        }
        if (sink == Long.MIN_VALUE) System.out.println("unreachable");
        System.out.println("PROBE_DONE");
    }

    static long sink = 0;
}
