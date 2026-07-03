import java.util.WeakHashMap;

public class WhmRepro {
    public static void main(String[] args) throws Exception {
        WeakHashMap<Thread, String> map = new WeakHashMap<>();
        Thread self = Thread.currentThread();
        map.put(self, "own-value");

        // Pad the map with lots of other entries so hash()/indexFor()/matchesKey()
        // get plenty of realistic bucket traffic (matches WeakHashMap growing to a
        // real table size, like in the ES repro with 600+ live weak refs).
        Thread[] padding = new Thread[2000];
        for (int i = 0; i < padding.length; i++) {
            padding[i] = new Thread("pad-" + i);
            map.put(padding[i], "pad-value-" + i);
        }

        int misses = 0;
        int iterations = 2_000_000;
        for (int i = 0; i < iterations; i++) {
            String v = map.get(self);
            if (v == null) {
                misses++;
                System.out.println("MISS at iteration " + i + " mapSize=" + map.size());
                if (misses > 20) {
                    System.out.println("Too many misses, stopping early.");
                    break;
                }
            }
            // Occasionally touch other entries too, like a real hot loop would.
            if ((i & 0xFFF) == 0) {
                map.get(padding[i % padding.length]);
            }
        }
        System.out.println("Done. iterations=" + iterations + " misses=" + misses + " mapSize=" + map.size());
        if (misses > 0) {
            System.exit(1);
        }
    }
}
