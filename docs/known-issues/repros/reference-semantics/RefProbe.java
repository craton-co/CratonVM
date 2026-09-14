import java.lang.ref.PhantomReference;
import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.SoftReference;
import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;

/**
 * Reference-semantics probe for the ZGC weak-referent skip-set residual.
 *
 * One PROBE line per check so the arms diff on stdout alone.
 *   WEAK     - a WeakReference to an otherwise-unreachable object must clear.
 *   PHANTOM  - a PhantomReference must enqueue.
 *   SOFTKEEP - a SoftReference must NOT clear while the heap is roomy.
 *   SOFTLRU  - a SoftReference NOT read since it was created must clear once
 *              the heap is tight. This is the one the referent skip set
 *              decides: if the marker traces the soft referent as a strong
 *              edge, `process_soft_refs` sees is_marked(referent)==true and
 *              returns before the LRU policy is even consulted.
 *              `get()` is deliberately NOT called inside the allocation loop --
 *              it re-stamps the LRU clock, which keeps idle time at ~0 and
 *              would test nothing.
 *   SOFTOOME - a SoftReference read continuously must still be cleared rather
 *              than let the VM throw OutOfMemoryError. That is the JVM spec's
 *              last-ditch guarantee, and it is a different mechanism from the
 *              LRU policy above.
 */
public final class RefProbe {
    private static void gcTwice() throws Exception {
        for (int i = 0; i < 3; i++) {
            System.gc();
            Thread.sleep(120);
        }
    }

    public static void main(String[] args) throws Exception {
        // ---- WEAK -------------------------------------------------------
        Object weakTarget = new byte[4096];
        WeakReference<Object> weak = new WeakReference<>(weakTarget);
        weakTarget = null;
        gcTwice();
        System.out.println("PROBE WEAK cleared=" + (weak.get() == null));

        // ---- PHANTOM ----------------------------------------------------
        ReferenceQueue<Object> q = new ReferenceQueue<>();
        Object phantomTarget = new byte[4096];
        PhantomReference<Object> phantom = new PhantomReference<>(phantomTarget, q);
        phantomTarget = null;
        gcTwice();
        Reference<?> polled = q.poll();
        System.out.println("PROBE PHANTOM enqueued=" + (polled == phantom));

        // ---- SOFTKEEP ---------------------------------------------------
        Object softLive = new byte[4096];
        SoftReference<Object> softKeep = new SoftReference<>(softLive);
        gcTwice();
        System.out.println("PROBE SOFTKEEP retained=" + (softKeep.get() != null));
        if (softLive.hashCode() == 0x7fffffff) {
            System.out.println("unreachable " + softLive);
        }

        // ---- SOFTLRU ----------------------------------------------------
        System.out.println("PROBE SOFTLRU " + softArm(false));

        // ---- SOFTOOME ---------------------------------------------------
        System.out.println("PROBE SOFTOOME " + softArm(true));

        System.out.println("PROBE DONE");
    }

    /**
     * @param touch re-read the soft reference on every allocation, which keeps
     *              its LRU clock at "now" and leaves the last-ditch rule as the
     *              only thing that can clear it.
     */
    private static String softArm(boolean touch) throws Exception {
        SoftReference<byte[]> soft = new SoftReference<>(new byte[1024 * 1024]);
        Thread.sleep(1500); // age the LRU clock past a tight-heap threshold
        boolean oome = false;
        List<byte[]> ballast = new ArrayList<>();
        try {
            for (int i = 0; i < 4096; i++) {
                ballast.add(new byte[1024 * 1024]);
                if (touch && soft.get() == null) {
                    break;
                }
            }
        } catch (OutOfMemoryError e) {
            oome = true;
        }
        int held = ballast.size();
        ballast.clear();
        boolean cleared = soft.get() == null;
        System.gc();
        return "cleared=" + cleared + " oome=" + oome + " mib_allocated=" + held;
    }
}
