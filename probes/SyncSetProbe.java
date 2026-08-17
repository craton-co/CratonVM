import java.util.*;

/**
 * Does `Collections.synchronizedSet/Map/List` actually take its `mutex`?
 *
 * Every thread adds N distinct elements and removes them again; at the end the
 * collection must be EMPTY and its size must be 0. A wrapper that forwards
 * without locking lets concurrent HashMap mutations corrupt the backing table,
 * which shows up as a non-zero (often NEGATIVE) size.
 */
public class SyncSetProbe {
    static final int THREADS = 8;
    static final int PER_THREAD = 4000;

    public static void main(String[] a) throws Exception {
        Set<Object> set = Collections.synchronizedSet(new HashSet<>());
        Map<Object, Object> map = Collections.synchronizedMap(new HashMap<>());
        List<Object> list = Collections.synchronizedList(new ArrayList<>());
        Thread[] ts = new Thread[THREADS];
        for (int i = 0; i < THREADS; i++) {
            final int base = i * PER_THREAD;
            ts[i] = new Thread(() -> {
                for (int k = 0; k < PER_THREAD; k++) {
                    Object key = "e" + (base + k);
                    set.add(key);
                    map.put(key, key);
                    list.add(key);
                }
                for (int k = 0; k < PER_THREAD; k++) {
                    Object key = "e" + (base + k);
                    set.remove(key);
                    map.remove(key);
                }
            });
            ts[i].start();
        }
        for (Thread t : ts) t.join();
        System.out.println("set.size()=" + set.size() + " (expect 0)");
        System.out.println("set.isEmpty()=" + set.isEmpty() + " (expect true)");
        System.out.println("map.size()=" + map.size() + " (expect 0)");
        System.out.println("list.size()=" + list.size() + " (expect " + (THREADS * PER_THREAD) + ")");
    }
}
