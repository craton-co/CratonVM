import java.lang.ref.PhantomReference;
import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.WeakReference;

/**
 * gci i17/fz (i16/fr's residual, docs/internal/gc-round-gci-20261011/i16-fr-report.md section 8):
 * a phantom reference to an object reachable only THROUGH the referent of a
 * discovered weak reference that a LIVE finalizable object holds.
 *
 * <p>With {@code CRATONVM_STW_UNDISCOVERED_REFS_STRONG=1} the stop-the-world
 * probe hands that referent Y to the collection as a resurrection candidate;
 * the round treated Y as unmarked but everything Y reaches as marked, so the
 * phantom to Z (reachable only through Y) was enqueued one collection late.
 * HotSpot (Serial, G1): Y is only weakly reachable, so W is cleared and Z is
 * phantom reachable in the same collection.
 *
 * <ul>
 *   <li>{@code deep}: live F -> weak W -> Y, Y.child -> Z; a static phantom
 *       P -> Z. (P is deliberately NOT reachable from F: the probe's walk
 *       from the registered finalizable objects would otherwise name Z as a
 *       referent of its own, which i16/fr already handled.)</li>
 *   <li>{@code chain}: as deep, but Y -> Z1 -> Z2 and P -> Z2.</li>
 *   <li>{@code shared}: as deep, but a static field also holds Z strongly: P
 *       must NOT be enqueued (the guard: an object a strong path reaches is
 *       never treated as dead).</li>
 * </ul>
 *
 * <p>HotSpot 25: every verdict line {@code true}. One {@code System.gc()} per
 * mode; the queue is then polled with a timeout, no further collection is
 * requested. stdout is the verdict lines only; diagnostics on stderr.
 */
public class GciFz17DeepCandidateProbe {
    static final ReferenceQueue<Object> QUEUE = new ReferenceQueue<>();
    static volatile int finalized;
    static Fin holder;
    static PhantomReference<Object> phantom;
    static Object strongZ;

    static final class Node {
        Object child;
        final long[] pad = new long[16];
    }

    static final class Fin {
        WeakReference<Object> weak;

        @SuppressWarnings("removal")
        @Override
        protected void finalize() {
            finalized = finalized + 1;
        }
    }

    static void build(String mode) {
        Fin f = new Fin();
        Node y = new Node();
        Object z;
        if (mode.equals("chain")) {
            Node z1 = new Node();
            z = new Object[] {"z2", Integer.valueOf(2)};
            z1.child = z;
            y.child = z1;
        } else {
            z = new Object[] {"z", Integer.valueOf(1)};
            y.child = z;
        }
        if (mode.equals("shared")) {
            strongZ = z;
        }
        f.weak = new WeakReference<>(y);
        phantom = new PhantomReference<>(z, QUEUE);
        holder = f;
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "deep";
        System.out.println("mode " + mode);
        if (!mode.equals("deep") && !mode.equals("chain") && !mode.equals("shared")) {
            System.out.println("unknown mode");
            return;
        }
        build(mode);
        long t0 = System.nanoTime();
        System.gc();
        boolean weakCleared = holder.weak.get() == null;
        System.out.println(mode + "-weak-cleared: " + weakCleared);
        if (mode.equals("shared")) {
            Reference<?> polled = QUEUE.remove(2_000L);
            boolean notEnqueued = polled == null;
            System.out.println(mode + "-phantom-not-enqueued: " + notEnqueued);
            System.out.println(mode + "-strong-intact: " + (strongZ instanceof Object[]
                    && "z".equals(((Object[]) strongZ)[0])));
            System.err.println("[fz17] mode=" + mode + " weakCleared=" + weakCleared + " notEnqueued="
                    + notEnqueued + " finalized=" + finalized);
        } else {
            Reference<?> polled = QUEUE.remove(5_000L);
            boolean enqueued = polled == phantom;
            long ms = (System.nanoTime() - t0) / 1_000_000L;
            System.out.println(mode + "-phantom-enqueued-by-first-collection: " + enqueued);
            System.err.println("[fz17] mode=" + mode + " weakCleared=" + weakCleared + " enqueued=" + enqueued
                    + " ms=" + ms + " finalized=" + finalized);
        }
        System.out.println(mode + "-live-not-finalized: " + (finalized == 0));
    }
}
