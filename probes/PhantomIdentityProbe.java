import java.lang.ref.PhantomReference;
import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.util.Collections;
import java.util.HashSet;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;

/**
 * Does a `Reference` that comes back out of a `ReferenceQueue` still have the
 * identity it was registered with?
 *
 * The write-up this probe belongs to reports that on 8 threads and ~3200
 * phantom references, 2-5 per run arrive as a DIFFERENT, half-constructed
 * instance of the SAME class: a `final int` assigned in the constructor reads
 * back `0`, and the registry has never seen the object being handed back. That
 * is the case a class-shape guard cannot see, because the shape is right.
 *
 * Reported per run:
 *   registered  - references created
 *   delivered   - references polled out of the queue
 *   distinct    - distinct identities among them
 *   duplicates  - the same identity delivered twice
 *   unknown     - delivered an object the registry never saw   <-- the defect
 *   zeroid      - delivered an object whose final int reads 0  <-- the defect
 *
 * `unknown` and `zeroid` must both be 0. HotSpot answers 0/0.
 */
public class PhantomIdentityProbe {
    static final class Watch extends PhantomReference<Object> {
        final int id;

        Watch(Object referent, ReferenceQueue<Object> q, int id) {
            super(referent, q);
            this.id = id;
        }
    }

    public static void main(String[] args) throws Exception {
        int threads = Integer.getInteger("threads", 8);
        int perThread = Integer.getInteger("per", 400);
        int rounds = Integer.getInteger("rounds", 1);

        for (int round = 1; round <= rounds; round++) {
            ReferenceQueue<Object> queue = new ReferenceQueue<>();
            // A plain synchronized registry, NOT Collections.synchronizedSet:
            // this probe is about reference identity, and must not depend on
            // the wrapper it is often run beside.
            // `-Dsyncset=true` swaps the registry for the wrapper the original
            // write-up's probe used, which is the variable that decides whether
            // "the enclosing Set has never seen the object handed back" is a
            // reference-processor finding or a `Collections.synchronizedSet`
            // finding.
            Set<Watch> registry = Boolean.getBoolean("syncset")
                    ? Collections.synchronizedSet(new HashSet<>())
                    : Collections.newSetFromMap(new ConcurrentHashMap<>());
            Set<Integer> ids = Collections.newSetFromMap(new ConcurrentHashMap<>());
            Thread[] ts = new Thread[threads];
            final int[] registered = {0};
            for (int t = 0; t < threads; t++) {
                final int base = t * perThread;
                ts[t] = new Thread(() -> {
                    for (int i = 0; i < perThread; i++) {
                        Object referent = new byte[64];
                        Watch w = new Watch(referent, queue, base + i + 1);
                        registry.add(w);
                        ids.add(w.id);
                        synchronized (registered) {
                            registered[0]++;
                        }
                        referent = null;
                        if ((i & 15) == 0) {
                            // Churn, so the referents die and their slots are
                            // reused while the references are still tracked.
                            byte[] junk = new byte[8192];
                            junk[0] = 1;
                        }
                    }
                }, "reg-" + t);
                ts[t].start();
            }
            for (Thread t : ts) t.join();

            for (int i = 0; i < 40; i++) {
                System.gc();
                Thread.sleep(20);
            }

            int delivered = 0, duplicates = 0, unknown = 0, zeroid = 0;
            Set<Integer> seen = new HashSet<>();
            Reference<?> r;
            while ((r = queue.poll()) != null) {
                delivered++;
                if (!(r instanceof Watch)) {
                    unknown++;
                    continue;
                }
                Watch w = (Watch) r;
                if (!registry.contains(w)) {
                    unknown++;
                }
                if (w.id == 0 || !ids.contains(w.id)) {
                    zeroid++;
                    continue;
                }
                if (!seen.add(w.id)) {
                    duplicates++;
                }
            }
            System.out.println("round=" + round
                    + " registered=" + registered[0]
                    + " delivered=" + delivered
                    + " distinct=" + seen.size()
                    + " duplicates=" + duplicates
                    + " unknown=" + unknown
                    + " zeroid=" + zeroid);
        }
    }
}
