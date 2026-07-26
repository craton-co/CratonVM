import java.util.HashMap;
import java.util.Map;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.ArrayList;
import java.util.List;

public class ClassUtilsProbe3 {
    static void warmHashMapMachinery() {
        for (int round = 0; round < 500; round++) {
            Map<String, Class<?>> m = new HashMap<>();
            for (int i = 0; i < 200; i++) {
                m.put("key" + round + "_" + i, Object.class);
            }
            if (m.size() != 200) throw new IllegalStateException("warmup corruption: " + m.size());
        }
    }

    public static void main(String[] args) throws Exception {
        System.out.println("warming HashMap machinery...");
        warmHashMapMachinery();
        System.out.println("warmup done");

        // Concurrent GC-pressure thread, matching the ban comment's
        // "esp. across a GC-triggering call" trigger condition -- keeps
        // allocating and forcing GCs on a background thread while the main
        // thread triggers ClassUtils.<clinit> repeatedly (by using a fresh
        // URLClassLoader each round so <clinit> genuinely re-runs, since a
        // JVM only runs a given class's clinit once per loader).
        AtomicBoolean stop = new AtomicBoolean(false);
        Thread gcPressure = new Thread(() -> {
            List<byte[]> garbage = new ArrayList<>();
            while (!stop.get()) {
                garbage.add(new byte[64 * 1024]);
                if (garbage.size() > 500) garbage.clear();
                if (garbage.size() % 50 == 0) System.gc();
            }
        });
        gcPressure.setDaemon(true);
        gcPressure.start();

        int mismatchTotal = 0;
        int rounds = 30;
        java.io.File jarFile = new java.io.File(System.getProperty("spring.core.jar"));
        java.net.URL jarUrl = jarFile.toURI().toURL();
        for (int r = 0; r < rounds; r++) {
            java.net.URLClassLoader loader = new java.net.URLClassLoader(
                new java.net.URL[]{jarUrl}, ClassUtilsProbe3.class.getClassLoader().getParent());
            Class<?> cu = Class.forName("org.springframework.util.ClassUtils", true, loader);
            java.lang.reflect.Field f = cu.getDeclaredField("commonClassCache");
            f.setAccessible(true);
            Map<?, ?> cache = (Map<?, ?>) f.get(null);
            int mismatches = 0;
            for (Map.Entry<?, ?> e : cache.entrySet()) {
                String key = (String) e.getKey();
                Object val = e.getValue();
                if (!(val instanceof Class)) { mismatches++; continue; }
                Class<?> valClass = (Class<?>) val;
                if (!valClass.getName().equals(key) && !key.equals(valClass.getSimpleName()) && !key.endsWith("[]")) {
                    mismatches++;
                }
            }
            mismatchTotal += mismatches;
            System.out.println("round=" + r + " cacheSize=" + cache.size() + " mismatches=" + mismatches);
            loader.close();
        }
        stop.set(true);
        System.out.println("TOTAL_MISMATCHES=" + mismatchTotal);
        System.out.println(mismatchTotal == 0 ? "PROBE_PASS" : "PROBE_FAIL");
    }
}
