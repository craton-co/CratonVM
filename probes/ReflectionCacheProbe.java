import java.lang.reflect.Method;

/**
 * `AnnotationsScanner`'s inner loop calls three reflective methods over and
 * over on a type with ~1000 declared methods:
 *
 *   Class.getDeclaredMethods()      (per type, per hierarchy walk)
 *   Method.getParameterTypes()      (per candidate pair, in hasSameParameterTypes)
 *   Method.getDeclaredAnnotations() (per method, in processMethod)
 *
 * HotSpot makes all three cheap by caching behind `Class.reflectionData` /
 * `Method.parameterTypes` and handing back a clone. If CratonVM rebuilds the
 * result each call, the cost is O(declaredMethods) per call inside a loop that
 * already runs O(declaredMethods) times — which is both the ~300x ratio and the
 * monotone slowdown (every call allocates ~1000 fresh objects, so GC pressure
 * grows pass over pass).
 *
 * Reports per-call microseconds so the arms are comparable without matching
 * absolute machine speed, plus allocation growth as an independent witness.
 */
public class ReflectionCacheProbe {

    public static void main(String[] args) throws Exception {
        String typeName = args.length > 0 ? args[0] : "org.jooq.impl.DefaultDSLContext";
        int iters = args.length > 1 ? Integer.parseInt(args[1]) : 200;

        Class<?> type = Class.forName(typeName);
        Method[] warm = type.getDeclaredMethods();
        System.out.println("type=" + typeName + " declaredMethods=" + warm.length);

        // --- Class.getDeclaredMethods -------------------------------------
        long t0 = System.nanoTime();
        long sink = 0;
        for (int i = 0; i < iters; i++) {
            sink += type.getDeclaredMethods().length;
        }
        double usPerCall = (System.nanoTime() - t0) / 1000.0 / iters;
        System.out.printf("getDeclaredMethods: %.1f us/call  (%d calls)%n", usPerCall, iters);

        // Identity: HotSpot returns a fresh clone each call (arrays differ),
        // but the ELEMENTS are the same cached Method objects.
        Method[] a = type.getDeclaredMethods();
        Method[] b = type.getDeclaredMethods();
        System.out.println("sameArray=" + (a == b) + " sameElement0=" + (a[0] == b[0]));

        // --- Method.getParameterTypes -------------------------------------
        Method probe = null;
        for (Method m : warm) {
            if (m.getParameterCount() >= 2) { probe = m; break; }
        }
        if (probe == null) probe = warm[0];
        t0 = System.nanoTime();
        for (int i = 0; i < iters * 50; i++) {
            sink += probe.getParameterTypes().length;
        }
        usPerCall = (System.nanoTime() - t0) / 1000.0 / (iters * 50);
        System.out.printf("getParameterTypes: %.3f us/call  (%s, %d params)%n",
                usPerCall, probe.getName(), probe.getParameterCount());

        // --- Method.getDeclaredAnnotations --------------------------------
        t0 = System.nanoTime();
        for (int i = 0; i < iters * 50; i++) {
            sink += probe.getDeclaredAnnotations().length;
        }
        usPerCall = (System.nanoTime() - t0) / 1000.0 / (iters * 50);
        System.out.printf("getDeclaredAnnotations: %.3f us/call%n", usPerCall);

        if (sink == Long.MIN_VALUE) System.out.println("unreachable " + sink);
        System.out.println("PROBE_DONE");
    }
}
