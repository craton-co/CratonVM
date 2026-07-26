import org.springframework.util.ClassUtils;
import java.lang.reflect.Method;
import java.util.Map;

public class ClassUtilsProbe {
    public static void main(String[] args) throws Exception {
        // Force ClassUtils.<clinit> to run, which internally calls the
        // private registerCommonClasses(Class<?>...) ~100-put hot loop
        // that SPB.1's ban comment implicates.
        Class<?> cu = Class.forName("org.springframework.util.ClassUtils");
        System.out.println("clinit ran OK");

        // Read back the private static commonClassCache via reflection and
        // sanity-check its contents match what registerCommonClasses should
        // have populated (int.class, int[].class, Void.TYPE, etc.) -- if the
        // allocate-then-putfield miscompile corrupted a stored entry, this
        // should show up as a wrong/null value or a ClassCastException.
        java.lang.reflect.Field f = cu.getDeclaredField("commonClassCache");
        f.setAccessible(true);
        Map<?, ?> cache = (Map<?, ?>) f.get(null);
        System.out.println("commonClassCache size=" + cache.size());

        int mismatches = 0;
        for (Map.Entry<?, ?> e : cache.entrySet()) {
            String key = (String) e.getKey();
            Object val = e.getValue();
            if (!(val instanceof Class)) {
                System.out.println("BAD ENTRY (not a Class): key=" + key + " val=" + val);
                mismatches++;
                continue;
            }
            Class<?> valClass = (Class<?>) val;
            if (!valClass.getName().equals(key) && !key.equals(valClass.getSimpleName())) {
                // primitive array entries use simple names like "int[]" as key
                // while class.getName() gives "[I" -- allow that one known shape,
                // flag anything else.
                boolean isArrayShapeOk = key.endsWith("[]");
                if (!isArrayShapeOk) {
                    System.out.println("MISMATCH: key=" + key + " -> " + valClass.getName());
                    mismatches++;
                }
            }
        }
        System.out.println("mismatches=" + mismatches);

        // Exercise ClassUtils.forName repeatedly (the real consumer of the
        // cache, and the method most likely to get JIT-compiled hot) to
        // pressure-test corrupted entries under sustained JIT activity.
        Method forName = cu.getMethod("forName", String.class, ClassLoader.class);
        String[] probes = {"int", "long", "double", "boolean", "byte", "char",
                "float", "short", "void", "int[]", "long[]", "double[]",
                "boolean[]", "byte[]", "char[]", "float[]", "short[]"};
        int errors = 0;
        for (int iter = 0; iter < 200000; iter++) {
            for (String p : probes) {
                try {
                    Object r = forName.invoke(null, p, null);
                    if (r == null) { errors++; if (errors < 20) System.out.println("NULL result for " + p + " at iter " + iter); }
                } catch (Exception ex) {
                    errors++;
                    if (errors < 20) System.out.println("EXCEPTION for " + p + " at iter " + iter + ": " + ex);
                }
            }
            if (iter % 20000 == 0) System.out.println("iter=" + iter + " errors=" + errors);
        }
        System.out.println("DONE errors=" + errors);
    }
}
