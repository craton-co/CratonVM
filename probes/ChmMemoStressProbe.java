import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;

/**
 * Concurrent stress for the per-thread ConcurrentHashMap String-node memo.
 *
 * Readers run the exact shape the memo is built for — the same key String
 * object looked up over and over on one thread — while writers replace, remove
 * and reinsert those same keys, and other writers force segment growth. Every
 * value the map can legally hold for a key encodes that key, so a memo entry
 * that outlives its node is caught as a mismatched value rather than only as a
 * rare wrong number.
 *
 * Also covers the reservation-marker window: a computeIfAbsent whose mapping
 * function is slow keeps a marker parked in the value slot while readers hammer
 * the same key, so a memo that captured the marker would surface it as a
 * non-String value (or as a stale null after the real value is published).
 */
public final class ChmMemoStressProbe {

    private static final int KEYS = 8;
    private static final String[] KEY = new String[KEYS];
    static {
        for (int i = 0; i < KEYS; i++) {
            KEY[i] = "key-" + i;
        }
    }

    private static String valueFor(String key, int round) {
        return key + "#" + round;
    }

    private static final AtomicBoolean stop = new AtomicBoolean();
    private static final AtomicLong reads = new AtomicLong();
    private static final AtomicLong errors = new AtomicLong();

    public static void main(String[] args) throws Exception {
        int seconds = args.length > 0 ? Integer.parseInt(args[0]) : 12;
        ConcurrentHashMap<String, Object> map = new ConcurrentHashMap<>();
        for (String k : KEY) {
            map.put(k, valueFor(k, 0));
        }

        Thread[] readers = new Thread[6];
        for (int t = 0; t < readers.length; t++) {
            final int id = t;
            readers[t] = new Thread(() -> {
                // Each reader pins ONE key object so its lookups are exactly
                // the memo's hot shape.
                String key = KEY[id % KEYS];
                long local = 0;
                while (!stop.get()) {
                    Object v = map.get(key);
                    local++;
                    if (v == null) {
                        continue; // legal: a writer removed it
                    }
                    if (!(v instanceof String)) {
                        errors.incrementAndGet();
                        System.out.println("FAIL non-String value leaked: " + v.getClass());
                        continue;
                    }
                    String s = (String) v;
                    if (!s.startsWith(key + "#")) {
                        errors.incrementAndGet();
                        System.out.println("FAIL wrong value for " + key + ": " + s);
                    }
                }
                reads.addAndGet(local);
            }, "reader-" + t);
        }

        Thread[] writers = new Thread[3];
        for (int t = 0; t < writers.length; t++) {
            final int id = t;
            writers[t] = new Thread(() -> {
                int round = 1;
                while (!stop.get()) {
                    for (String k : KEY) {
                        map.put(k, valueFor(k, round));
                    }
                    String victim = KEY[(round + id) % KEYS];
                    map.remove(victim);
                    map.put(victim, valueFor(victim, round));
                    round++;
                }
            }, "writer-" + t);
        }

        // Forces segment growth/relinking underneath the readers' keys.
        Thread grower = new Thread(() -> {
            int round = 0;
            while (!stop.get()) {
                for (int i = 0; i < 4096; i++) {
                    map.put("filler-" + round + "-" + i, "filler");
                }
                for (int i = 0; i < 4096; i++) {
                    map.remove("filler-" + round + "-" + i);
                }
                round++;
            }
        }, "grower");

        // Reservation-marker window: a slow mapping function holds the marker.
        Thread computer = new Thread(() -> {
            while (!stop.get()) {
                String k = "computed";
                map.computeIfAbsent(k, name -> {
                    try {
                        Thread.sleep(2);
                    } catch (InterruptedException e) {
                        Thread.currentThread().interrupt();
                    }
                    return valueFor(name, 1);
                });
                map.remove(k);
            }
        }, "computer");

        Thread markerReader = new Thread(() -> {
            String k = "computed";
            while (!stop.get()) {
                Object v = map.get(k);
                if (v != null && !(v instanceof String)) {
                    errors.incrementAndGet();
                    System.out.println("FAIL reservation marker leaked: " + v.getClass());
                }
            }
        }, "marker-reader");

        for (Thread t : readers) t.start();
        for (Thread t : writers) t.start();
        grower.start();
        computer.start();
        markerReader.start();

        Thread.sleep(seconds * 1000L);
        stop.set(true);
        for (Thread t : readers) t.join();
        for (Thread t : writers) t.join();
        grower.join();
        computer.join();
        markerReader.join();

        // Final consistency: every key must read back its last written value.
        for (String k : KEY) {
            Object v = map.get(k);
            if (v != null && !((String) v).startsWith(k + "#")) {
                errors.incrementAndGet();
                System.out.println("FAIL final value for " + k + ": " + v);
            }
        }

        if (errors.get() != 0) {
            throw new AssertionError("CHM memo stress failures: " + errors.get());
        }
        System.out.println("CHM_MEMO_STRESS_OK reads=" + reads.get());
    }
}
