import java.lang.ref.WeakReference;
import java.util.Random;
import java.util.concurrent.atomic.AtomicLong;

/**
 * Isolates the mechanism behind
 * the retired bug-h2-mvstore-readpagefromcache-classid0-nonmoving-sweep write-up
 * without needing H2.
 *
 * The failing read there is `FileStore.readPageFromCache`:
 *
 *     return (Page<K,V>) cache.get(pos);
 *
 * and `CacheLongKeyLIRS.Entry.getValue()` is
 *
 *     return value == null ? reference.get() : value;
 *
 * i.e. the cast that fails is over a `WeakReference.get()` — generics erasure
 * puts a `checkcast Page` immediately after `invokevirtual get()Ljava/lang/Object;`.
 * H2's `evictBlock()` performs exactly this transition on every eviction:
 *
 *     e.reference = new WeakReference<>(e.value);
 *     e.value = null;
 *
 * and `access()` resurrects it with `e.value = e.getValue()`, which LATCHES a
 * bad `get()` into the strong field — which is why the doc records "exactly 4
 * `cannot be cast` lines (one per reader thread that trips over the same
 * corrupted page)".
 *
 * This probe reproduces that shape: a long-lived (old-gen) Entry array whose
 * entries oscillate between strong and weak, read concurrently, with the same
 * erased-generic cast on the read path.
 *
 * args: [threads] [seconds] [entries]
 */
public class WeakRefLirsStress {

    static final int MAGIC = 0x5AFE5AFE;

    static final class Payload {
        final int magic = MAGIC;
        final long id;
        final byte[] data;
        Payload(long id, int size) {
            this.id = id;
            this.data = new byte[size];
        }
    }

    static final class Entry {
        final long key;
        volatile Payload value;
        volatile WeakReference<Payload> reference;
        Entry(long key) { this.key = key; }

        // Erased exactly like CacheLongKeyLIRS.Entry.getValue(): the
        // `reference.get()` arm compiles to invokevirtual + checkcast Payload.
        Payload getValue() {
            Payload v = value;
            if (v != null) {
                return v;
            }
            WeakReference<Payload> r = reference;
            return r == null ? null : r.get();
        }
    }

    static volatile boolean stop;
    static final AtomicLong reads = new AtomicLong();
    static final AtomicLong resurrections = new AtomicLong();
    static final AtomicLong evictions = new AtomicLong();
    static final AtomicLong misses = new AtomicLong();
    static final AtomicLong failures = new AtomicLong();

    public static void main(String... a) throws Exception {
        int threads = a.length > 0 ? Integer.parseInt(a[0]) : 10;
        int seconds = a.length > 1 ? Integer.parseInt(a[1]) : 600;
        int count = a.length > 2 ? Integer.parseInt(a[2]) : 20000;
        int payload = a.length > 3 ? Integer.parseInt(a[3]) : 8 * 1024;

        final Entry[] entries = new Entry[count];
        for (int i = 0; i < count; i++) {
            entries[i] = new Entry(i);
            entries[i].value = new Payload(i, payload);
        }
        // Give the long-lived array and its entries time to be promoted.
        for (int i = 0; i < 20; i++) {
            byte[] churn = new byte[1024 * 1024];
            if (churn.length == 0) {
                throw new IllegalStateException();
            }
        }
        System.out.println("populated " + count + " entries of " + payload + " bytes");
        System.out.flush();

        final long deadline = System.currentTimeMillis() + seconds * 1000L;
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int seed = t;
            ts[t] = new Thread(new Runnable() {
                @Override public void run() {
                    Random r = new Random(seed);
                    try {
                        while (!stop && System.currentTimeMillis() < deadline) {
                            for (int k = 0; k < 200; k++) {
                                Entry e = entries[r.nextInt(entries.length)];
                                Payload p = e.getValue();
                                if (p == null) {
                                    misses.incrementAndGet();
                                    p = new Payload(e.key, payload);
                                    e.value = p;
                                    e.reference = null;
                                } else {
                                    if (p.magic != MAGIC || p.data == null) {
                                        report(e, p, "field readback");
                                        return;
                                    }
                                    if (e.value == null) {
                                        // resurrect, exactly as LIRS access() does
                                        e.value = p;
                                        e.reference = null;
                                        resurrections.incrementAndGet();
                                    }
                                }
                                reads.incrementAndGet();
                                // evict a random OTHER entry, as evictBlock() does
                                Entry v = entries[r.nextInt(entries.length)];
                                Payload pv = v.value;
                                if (pv != null) {
                                    v.reference = new WeakReference<>(pv);
                                    v.value = null;
                                    evictions.incrementAndGet();
                                }
                            }
                        }
                    } catch (Throwable ex) {
                        failures.incrementAndGet();
                        System.out.println("READER FAILED: " + ex);
                        ex.printStackTrace(System.out);
                        System.out.flush();
                        stop = true;
                    }
                }
            });
            ts[t].setDaemon(true);
            ts[t].start();
        }

        long last = System.currentTimeMillis();
        while (!stop && System.currentTimeMillis() < deadline) {
            Thread.sleep(500);
            long now = System.currentTimeMillis();
            if (now - last >= 30000) {
                last = now;
                System.out.println("reads=" + reads.get() + " evict=" + evictions.get()
                        + " resurrect=" + resurrections.get() + " miss=" + misses.get());
                System.out.flush();
            }
        }
        System.out.println("done reads=" + reads.get() + " failures=" + failures.get());
        System.out.flush();
        if (failures.get() != 0) {
            Runtime.getRuntime().halt(3);
        }
    }

    static void report(Entry e, Payload p, String where) {
        failures.incrementAndGet();
        System.out.println("CORRUPT (" + where + ") key=" + e.key
                + " magic=" + Integer.toHexString(p.magic)
                + " id=" + p.id + " data=" + (p.data == null ? "null" : p.data.length));
        System.out.flush();
        stop = true;
    }
}
