import java.lang.ref.PhantomReference;
import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.WeakReference;

/**
 * gci i15/rf (finding G-9 of
 * docs/internal/gc/gci-i15rf-g1-remark-loses-a-reference-only-a-dead-finalizable-reaches-RETIRED-UNFIXED-20261018.md):
 * a finalizable object X holds a WeakReference W and a PhantomReference P that
 * nothing else reaches; W's referent Y and P's referent Z are reachable only
 * through them. When X dies, HotSpot's keep-alive phase marks X with reference
 * discovery disabled, so W and P are ordinary objects in that collection: Y and
 * Z are kept, W still names Y after {@code finalize()} resurrects X. The NEXT
 * collection discovers W and P: W is cleared, P is enqueued.
 *
 * <p>{@code old}: X, W, P, Y and Z are first aged by 20 {@code System.gc()}
 * calls while statics hold them, so on this VM's G1 they sit in Old regions and
 * only the concurrent cycle's remark finds X dead. {@code young}: dropped at
 * once (evidence for the stop-the-world pauses, which clear W in the pause that
 * resurrects X). HotSpot 25 (Serial, G1): every line {@code true}.
 *
 * <p>stdout only the verdict lines below; diagnostics on stderr.
 */
public class GciI15RfUndiscoveredRefProbe {
    static final int LEN = 512;
    static volatile int finalized;
    static Fin holder;
    static volatile Fin resurrected;
    static long[] keepY;
    static Object[] keepZ;
    static final ReferenceQueue<Object[]> QUEUE = new ReferenceQueue<>();

    static final class Fin {
        WeakReference<long[]> weak;
        PhantomReference<Object[]> phantom;

        @SuppressWarnings("removal")
        @Override
        protected void finalize() {
            resurrected = this;
            finalized = finalized + 1;
        }
    }

    static long sum(long[] a) {
        long s = 0;
        for (int i = 0; i < a.length; i++) {
            s = s * 31 + a[i];
        }
        return s;
    }

    static long expected() {
        long[] a = new long[LEN];
        fill(a);
        return sum(a);
    }

    static void fill(long[] a) {
        for (int i = 0; i < a.length; i++) {
            a[i] = i * 31L + 7;
        }
    }

    static void build() {
        long[] y = new long[LEN];
        fill(y);
        Object[] z = new Object[] {"z", Integer.valueOf(3)};
        keepY = y;
        keepZ = z;
        Fin x = new Fin();
        x.weak = new WeakReference<>(y);
        x.phantom = new PhantomReference<>(z, QUEUE);
        holder = x;
    }

    @SuppressWarnings("removal")
    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "old";
        boolean old = mode.equals("old");
        System.out.println("mode " + (old ? "old" : "young"));
        long want = expected();
        build();
        if (old) {
            for (int i = 0; i < 20; i++) {
                System.gc();
            }
        }
        keepY = null;
        keepZ = null;
        holder = null;
        System.gc();
        long deadline = System.nanoTime() + 10_000_000_000L;
        while (finalized == 0 && System.nanoTime() < deadline) {
            System.runFinalization();
            Thread.sleep(10);
        }
        System.out.println("finalized-once: " + (finalized == 1));
        Fin back = resurrected;
        long[] got = back == null ? null : back.weak.get();
        boolean kept = got != null;
        boolean intact = got != null && got.length == LEN && sum(got) == want;
        got = null;
        System.out.println("weak-from-finalizable-kept: " + kept);
        System.out.println("payload-intact: " + intact);
        System.gc();
        boolean cleared = back != null && back.weak.get() == null;
        System.out.println("weak-cleared-by-next-collection: " + cleared);
        Reference<? extends Object[]> polled = QUEUE.remove(10_000L);
        boolean enqueued = back != null && polled == back.phantom;
        System.out.println("phantom-enqueued-by-next-collection: " + enqueued);
        System.err.println("[rf15] finalized=" + finalized + " kept=" + kept + " intact=" + intact
                + " cleared=" + cleared + " polled=" + (polled != null));
    }
}
