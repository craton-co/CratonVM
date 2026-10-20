import java.lang.ref.WeakReference;

/**
 * gc-common w29-a: a DEAD ThreadLocal's value must stop being rooted by the
 * (still live) thread that set it, once that thread keeps using ThreadLocals.
 *
 * CratonVM serves ThreadLocal get/set/remove from natives whose per-thread
 * rows name their ThreadLocal through a JNI weak global; a row whose owner a
 * collection cleared is expunged when its bucket is touched and by a full
 * expunge an insert runs once the map has doubled. This probe drops N
 * ThreadLocals that each hold a value, collects, then sets enough fresh
 * ThreadLocals to cross the expunge threshold, collects again, and counts the
 * dead ThreadLocals' values that are still reachable.
 *
 * Expected: CratonVM (all three backends) `dead-values-live=0`. HotSpot's
 * ThreadLocalMap expunges lazily too (cleanSomeSlots / rehash), so its count
 * may be non-zero; the verdict line judges only what must hold everywhere:
 * the live ThreadLocals keep their own values. Ends on its own (bounded).
 */
public class DeadThreadLocalExpungeProbe {
    public static void main(String[] args) throws Exception {
        final int n = 2000;
        @SuppressWarnings("unchecked")
        WeakReference<Object>[] dead = new WeakReference[n];
        for (int i = 0; i < n; i++) {
            ThreadLocal<Object> tl = new ThreadLocal<>();
            Object value = new byte[256];
            tl.set(value);
            dead[i] = new WeakReference<>(value);
        }
        for (int r = 0; r < 5; r++) {
            System.gc();
            Thread.sleep(20);
        }
        @SuppressWarnings("unchecked")
        ThreadLocal<Integer>[] keep = new ThreadLocal[400];
        for (int i = 0; i < keep.length; i++) {
            keep[i] = new ThreadLocal<>();
            keep[i].set(i);
        }
        int live = n;
        long start = System.nanoTime();
        while (live > 0 && System.nanoTime() - start < 10_000_000_000L) {
            System.gc();
            Thread.sleep(50);
            live = 0;
            for (WeakReference<Object> w : dead) {
                if (w.get() != null) {
                    live++;
                }
            }
        }
        boolean kept = true;
        for (int i = 0; i < keep.length; i++) {
            Integer v = keep[i].get();
            kept &= v != null && v == i;
        }
        System.out.println("dead-values-live=" + live + " of " + n);
        System.out.println("kept-values-ok=" + kept);
        System.out.println(kept ? "PROBE-OK" : "PROBE-FAIL");
    }
}
