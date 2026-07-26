import java.util.WeakHashMap;

public class WhmRepro2 {
    public static void main(String[] args) throws Exception {
        WeakHashMap<Thread, String> map = new WeakHashMap<>();
        Thread self = Thread.currentThread();
        map.put(self, "own-value");

        int misses = 0;
        int total = 40000;
        for (int i = 0; i < total; i++) {
            Thread t = new Thread("pad-" + i);
            map.put(t, "pad-value-" + i);
            // Query self repeatedly around every insert/resize, like a hot
            // loop calling WeakHashMap.get() on a stable key while the table
            // grows underneath it.
            for (int j = 0; j < 50; j++) {
                String v = map.get(self);
                if (v == null) {
                    misses++;
                    System.out.println("MISS at outer=" + i + " inner=" + j + " mapSize=" + map.size());
                    if (misses > 20) {
                        System.out.println("Too many misses, stopping early.");
                        System.exit(1);
                    }
                }
            }
        }
        System.out.println("Done. total=" + total + " misses=" + misses + " mapSize=" + map.size());
        if (misses > 0) {
            System.exit(1);
        }
    }
}
