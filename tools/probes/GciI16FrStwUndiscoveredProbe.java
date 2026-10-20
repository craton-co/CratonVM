import java.lang.ref.PhantomReference;
import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.WeakReference;

/**
 * gci i16/fr (the stop-the-world half of G-9,
 * docs/internal/gc/gci-i15rf-g1-remark-loses-a-reference-only-a-dead-finalizable-reaches-RETIRED-UNFIXED-20261018.md):
 * HotSpot's reference order in a stop-the-world collection that finds a
 * finalizable object X dead. Discovery happens only during the strong mark;
 * {@code process_discovered_references} disables it before the FinalReference
 * keep-alive phase, so a Reference first reached through X is an ordinary
 * object there and its referent is kept.
 *
 * <ul>
 *   <li>{@code nested}: X -> weak W -> Inner Y, and Y -> weak W2 -> payload.
 *       After {@code finalize()}: both kept; the next collection clears W.</li>
 *   <li>{@code twin}: X -> weak W -> payload Y, and a static (discovered) weak
 *       V -> Y. V is cleared in the collection that finalizes X (before the
 *       keep-alive phase), W keeps Y; the next collection clears W.</li>
 *   <li>{@code live}: X is ALIVE (static) and holds a weak W -> payload and a
 *       phantom P -> Z; nothing else reaches the payload or Z. Both are
 *       discovered: W cleared and P enqueued by the first collection.</li>
 * </ul>
 *
 * <p>HotSpot 25 (Serial, G1): every verdict line {@code true}. stdout only the
 * verdict lines; diagnostics on stderr.
 */
public class GciI16FrStwUndiscoveredProbe {
    static final int LEN = 256;
    static volatile int finalized;
    static volatile Fin resurrected;
    static Fin holder;
    static WeakReference<long[]> twin;
    static final ReferenceQueue<Object> QUEUE = new ReferenceQueue<>();

    static final class Inner {
        WeakReference<long[]> weak;
    }

    static final class Fin {
        WeakReference<Object> weak;
        PhantomReference<Object> phantom;

        @SuppressWarnings("removal")
        @Override
        protected void finalize() {
            resurrected = this;
            finalized = finalized + 1;
        }
    }

    static long[] payload() {
        long[] a = new long[LEN];
        for (int i = 0; i < LEN; i++) {
            a[i] = i * 31L + 7;
        }
        return a;
    }

    static boolean intact(Object o) {
        if (!(o instanceof long[])) {
            return false;
        }
        long[] a = (long[]) o;
        if (a.length != LEN) {
            return false;
        }
        for (int i = 0; i < LEN; i++) {
            if (a[i] != i * 31L + 7) {
                return false;
            }
        }
        return true;
    }

    @SuppressWarnings("removal")
    static Fin awaitFinalized() throws InterruptedException {
        System.gc();
        long deadline = System.nanoTime() + 10_000_000_000L;
        while (finalized == 0 && System.nanoTime() < deadline) {
            System.runFinalization();
            Thread.sleep(10);
        }
        return resurrected;
    }

    static void buildNested() {
        Inner y = new Inner();
        y.weak = new WeakReference<>(payload());
        Fin x = new Fin();
        x.weak = new WeakReference<>(y);
    }

    static void nested() throws Exception {
        buildNested();
        Fin back = awaitFinalized();
        System.out.println("finalized-once: " + (finalized == 1));
        Object o = back == null ? null : back.weak.get();
        boolean outer = o instanceof Inner;
        boolean inner = outer && intact(((Inner) o).weak.get());
        o = null;
        System.out.println("outer-kept: " + outer);
        System.out.println("inner-kept: " + inner);
        System.gc();
        boolean cleared = back != null && back.weak.get() == null;
        System.out.println("outer-cleared-by-next-collection: " + cleared);
        System.err.println("[fr16] nested finalized=" + finalized + " outer=" + outer + " inner=" + inner
                + " cleared=" + cleared);
    }

    static void buildTwin() {
        long[] y = payload();
        twin = new WeakReference<>(y);
        Fin x = new Fin();
        x.weak = new WeakReference<>(y);
    }

    static void twin() throws Exception {
        buildTwin();
        Fin back = awaitFinalized();
        System.out.println("finalized-once: " + (finalized == 1));
        boolean twinCleared = twin.get() == null;
        System.out.println("discovered-twin-cleared: " + twinCleared);
        boolean kept = back != null && intact(back.weak.get());
        System.out.println("undiscovered-kept: " + kept);
        System.gc();
        boolean cleared = back != null && back.weak.get() == null;
        System.out.println("undiscovered-cleared-by-next-collection: " + cleared);
        System.err.println("[fr16] twin finalized=" + finalized + " twinCleared=" + twinCleared + " kept=" + kept
                + " cleared=" + cleared);
    }

    static void buildLive() {
        Fin x = new Fin();
        x.weak = new WeakReference<>(payload());
        x.phantom = new PhantomReference<>(new Object[] {"z", Integer.valueOf(3)}, QUEUE);
        holder = x;
    }

    static void live() throws Exception {
        buildLive();
        System.gc();
        boolean weakCleared = holder.weak.get() == null;
        System.out.println("live-weak-cleared: " + weakCleared);
        Reference<?> polled = QUEUE.remove(10_000L);
        boolean enqueued = polled == holder.phantom;
        System.out.println("live-phantom-enqueued: " + enqueued);
        System.out.println("live-not-finalized: " + (finalized == 0));
        System.err.println("[fr16] live weakCleared=" + weakCleared + " enqueued=" + enqueued + " finalized="
                + finalized);
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "nested";
        System.out.println("mode " + mode);
        if (mode.equals("nested")) {
            nested();
        } else if (mode.equals("twin")) {
            twin();
        } else if (mode.equals("live")) {
            live();
        } else {
            System.out.println("unknown mode");
        }
    }
}
