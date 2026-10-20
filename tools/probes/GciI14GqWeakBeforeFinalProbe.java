import java.lang.ref.PhantomReference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.WeakReference;

/**
 * gci i14/gq (finding G-1 of
 * docs/known-issues/gc/gci-i14gq-g1-reference-processing-and-finalization-review-findings-20261013.md):
 * a finalizable object and a private child die together; weak references to
 * both must read null right after the collection that finds them dead, before
 * {@code finalize()} resurrects the object (HotSpot clears soft and weak
 * references before finalization). A weak reference to an object the
 * finalizable shares with a static root stays set; a phantom reference to the
 * finalizable is not enqueued (phantom after finalization); the resurrected
 * object's child is intact afterwards.
 *
 * <p>{@code old}: the graph is first aged by 20 {@code System.gc()} calls while
 * a static holds it, so on this VM's G1 it sits in Old regions and only the
 * concurrent cycle's remark can find it dead (a young pause never looks at an
 * Old region). {@code young}: dropped at once (the control: young pauses
 * already clear both). {@code split}: only the CHILD is aged first, the
 * finalizable is young (finding G-2: this VM's G1 {@code System.gc()} is a
 * young pause plus a marking cycle, and the young pause's round treats the Old
 * child as live). HotSpot 25 (Serial, G1): every line {@code true}.
 *
 * <p>stdout only the verdict lines below; diagnostics on stderr.
 */
public class GciI14GqWeakBeforeFinalProbe {
    static volatile int finalized;
    static final Object SHARED = new Object();
    static Fin holder;
    static volatile Fin resurrected;

    static final class Child {
        final int[] payload = new int[] {7, 11, 13};
    }

    static Child agedChild;

    static final class Fin {
        Child child;
        Object shared = SHARED;

        Fin(Child child) {
            this.child = child;
        }

        @SuppressWarnings("removal")
        @Override
        protected void finalize() {
            resurrected = this;
            finalized = finalized + 1;
        }
    }

    @SuppressWarnings("removal")
    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "young";
        boolean old = mode.equals("old");
        boolean split = mode.equals("split");
        System.out.println("mode " + (old ? "old" : split ? "split" : "young"));
        if (split) {
            agedChild = new Child();
            for (int i = 0; i < 20; i++) {
                System.gc();
            }
            holder = new Fin(agedChild);
            agedChild = null;
        } else {
            holder = new Fin(new Child());
        }
        WeakReference<Fin> weakFin = new WeakReference<>(holder);
        WeakReference<Child> weakChild = new WeakReference<>(holder.child);
        WeakReference<Object> weakShared = new WeakReference<>(SHARED);
        ReferenceQueue<Fin> pq = new ReferenceQueue<>();
        PhantomReference<Fin> phantom = new PhantomReference<>(holder, pq);
        if (old) {
            for (int i = 0; i < 20; i++) {
                System.gc();
            }
        }
        holder = null;
        System.gc();
        // Read before waiting for finalize(): the verdict of the collection
        // that found the object dead.
        boolean finCleared = weakFin.get() == null;
        boolean childCleared = weakChild.get() == null;
        System.out.println("weak-to-finalizable-cleared: " + finCleared);
        System.out.println("weak-to-private-child-cleared: " + childCleared);
        System.out.println("weak-to-shared-kept: " + (weakShared.get() == SHARED));
        long deadline = System.nanoTime() + 10_000_000_000L;
        while (finalized == 0 && System.nanoTime() < deadline) {
            System.runFinalization();
            Thread.sleep(10);
        }
        System.out.println("finalized-once: " + (finalized == 1));
        System.gc();
        System.out.println("phantom-held-back: " + (pq.poll() == null));
        Fin back = resurrected;
        boolean intact = back != null
                && back.child != null
                && back.child.payload.length == 3
                && back.child.payload[0] + back.child.payload[1] + back.child.payload[2] == 31
                && back.shared == SHARED;
        System.out.println("resurrected-child-intact: " + intact);
        System.out.println("weak-still-cleared-after-resurrection: "
                + (weakFin.get() == null && weakChild.get() == null));
        System.err.println("[gq14] finalized=" + finalized + " finCleared=" + finCleared
                + " childCleared=" + childCleared);
        if (phantom.refersTo(null)) {
            System.err.println("[gq14] phantom reads cleared");
        }
    }
}
