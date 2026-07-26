import java.util.HashMap;
import java.util.Map;

public class ClassUtilsProbe2 {
    // Warm HashMap.put/putVal/newNode/afterNodeInsertion into JIT-hot state
    // BEFORE ClassUtils.<clinit> runs, replicating a real Spring Boot boot
    // where those methods are already hot by the time ClassUtils loads
    // (SPB.1's ban comment: "a long sequence of put -> putVal -> newNode ->
    // Node.<init> -> afterNodeInsertion cycles" right as the crash frame).
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
        System.out.println("warmup done, triggering ClassUtils.<clinit>...");

        Class<?> cu = Class.forName("org.springframework.util.ClassUtils");
        System.out.println("clinit ran OK");

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
                boolean isArrayShapeOk = key.endsWith("[]");
                if (!isArrayShapeOk) {
                    System.out.println("MISMATCH: key=" + key + " -> " + valClass.getName());
                    mismatches++;
                }
            }
        }
        System.out.println("mismatches=" + mismatches);
        System.out.println(mismatches == 0 ? "PROBE_PASS" : "PROBE_FAIL");
    }
}
