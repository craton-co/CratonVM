import java.lang.reflect.Method;
import java.util.Map;

import org.springframework.context.event.EventListener;
import org.springframework.core.MethodIntrospector;
import org.springframework.core.annotation.AnnotatedElementUtils;
import org.springframework.util.ConcurrentReferenceHashMap;

/**
 * Why is Spring's annotation scan ~40x HotSpot on a jOOQ-sized type under
 * CratonVM (`JooqAutoConfigurationTests`, 6s -> 245s for one test that runs no
 * SQL)? A `--stack-sample-ms 100` profile of that test put **1910 of 2042
 * samples (93.5%) under `EventListenerMethodProcessor`**, so this probe runs
 * exactly that processor's call, not an approximation of it:
 *
 *   EventListenerMethodProcessor.processBean
 *     -> MethodIntrospector.selectMethods(targetType, MetadataLookup)
 *       -> ReflectionUtils.doWithMethods   (declared methods, WHOLE hierarchy)
 *         -> AnnotatedElementUtils.findMergedAnnotation(method, EventListener)
 *
 * Note `findMergedAnnotation(m, Deprecated.class)` does NOT exercise this:
 * `AnnotationFilter.PLAIN` rejects `java.lang.*` annotations up front, so a
 * probe using it measures the filter and reports milliseconds. `@EventListener`
 * is what the real path looks for.
 *
 * Also checks whether Spring's `ConcurrentReferenceHashMap` (SOFT refs, the
 * backing store for `AnnotationsScanner`'s caches) retains what it is given —
 * if entries evaporate, every scan is a cold scan.
 *
 * Everything reported is a ratio or a count measured inside one process, so it
 * is valid on a loaded host and comparable across VMs.
 */
public class AnnotationScanCacheProbe {

    public static void main(String[] args) throws Exception {
        String typeName = args.length > 0 ? args[0] : "org.jooq.impl.DefaultDSLContext";
        int entries = args.length > 1 ? Integer.parseInt(args[1]) : 200_000;

        // ---- 1. ConcurrentReferenceHashMap retention -----------------------
        Map<String, String> crhm = new ConcurrentReferenceHashMap<>(16);
        String[] keys = new String[entries];
        for (int i = 0; i < entries; i++) {
            keys[i] = "k" + i;
            crhm.put(keys[i], "v" + i);
        }
        // `keys` keeps every key strongly reachable, so a correct SOFT-ref map
        // under no memory pressure must still hold every value.
        int missing = 0, wrong = 0;
        for (int i = 0; i < entries; i++) {
            String v = crhm.get(keys[i]);
            if (v == null) missing++;
            else if (!v.equals("v" + i)) wrong++;
        }
        System.out.println("CRHM entries=" + entries + " missing=" + missing + " wrong=" + wrong
                + " size=" + crhm.size());
        System.out.println("RETENTION=" + (missing == 0 && wrong == 0 ? "OK" : "BROKEN"));

        // ---- 2. the real EventListenerMethodProcessor scan ------------------
        Class<?> type = Class.forName(typeName);
        System.out.println("type=" + typeName
                + " publicMethods=" + type.getMethods().length
                + " declaredMethods=" + type.getDeclaredMethods().length
                + " interfaces=" + type.getInterfaces().length);

        int passes = args.length > 2 ? Integer.parseInt(args[2]) : 3;
        long p1 = -1, pLast = -1;
        for (int i = 1; i <= passes; i++) {
            long ms = selectMethods(type);
            if (i == 1) p1 = ms;
            pLast = ms;
            Runtime rt = Runtime.getRuntime();
            long usedMb = (rt.totalMemory() - rt.freeMemory()) / (1024 * 1024);
            System.out.println("pass" + i + "_ms=" + ms + " heapUsedMb=" + usedMb);
            System.out.flush();
        }
        // A working cache makes later passes CHEAPER. If the last pass costs as
        // much as (or more than) the first, the cache is not the thing that
        // matters here and something is degrading instead.
        String ratio = (pLast == 0) ? "inf" : String.format("%.2f", (double) p1 / (double) pLast);
        System.out.println("first_over_last=" + ratio);
        System.out.println("PROBE_DONE");
    }

    private static long selectMethods(Class<?> type) {
        long t0 = System.nanoTime();
        Map<Method, EventListener> found = MethodIntrospector.selectMethods(type,
                (MethodIntrospector.MetadataLookup<EventListener>) method ->
                        AnnotatedElementUtils.findMergedAnnotation(method, EventListener.class));
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        if (found == null) throw new IllegalStateException("selectMethods returned null");
        return ms;
    }
}
