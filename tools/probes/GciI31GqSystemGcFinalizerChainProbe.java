import java.lang.ref.WeakReference;

/**
 * gci i31/gq (G-2 of docs/known-issues/gc/gci-i14gq-g1-reference-processing-and-finalization-review-findings-20261013.md):
 * many rounds of "a young finalizable {@code Fin} that alone holds private
 * children, some of them aged into the old generation first, dies; one
 * {@code System.gc()}". HotSpot 25 (one full collection per call): every weak
 * reference to a private child reads null right after the call, before
 * {@code finalize()}; {@code finalize()} then finds every child intact.
 *
 * <p>The soundness half (the point of the rows under the whole-heap verifier):
 * {@code finalize()} checks every child's payload, so a child the collection
 * freed or scrubbed while the finalizable was queued shows as a mismatch (or a
 * crash). The round body runs hot (JIT-compiled after a few rounds) and a churn
 * thread allocates throughout, so pauses meet compiled frames and a busy peer.
 *
 * <p>stdout (deterministic, HotSpot): {@code rounds N}, {@code
 * weak-to-private-children-cleared-every-round true}, {@code
 * finalized-every-round true}, {@code children-intact true}. Diagnostics on
 * stderr ({@code [gq31c] ...}).
 *
 * <p>Args: {@code [rounds (default 60)] [aged children per round (default 6)]
 * [young children per round (default 6)]}.
 */
public class GciI31GqSystemGcFinalizerChainProbe {
    static volatile boolean stop;
    static volatile Object sinkObj;
    static volatile int finalizedThisRound;
    static volatile int badChildren;
    static volatile Fin back;
    static Child[] agedPool;

    static final class Child {
        final int[] payload;
        Child next;

        Child(int seed) {
            payload = new int[] {seed, seed * 3, seed * 7};
        }

        boolean intact() {
            return payload.length == 3
                    && payload[1] == payload[0] * 3
                    && payload[2] == payload[0] * 7;
        }
    }

    static final class Fin {
        final Child[] children;

        Fin(Child[] children) {
            this.children = children;
        }

        @Override
        @SuppressWarnings({"deprecation", "removal"})
        protected void finalize() {
            int bad = 0;
            for (Child c : children) {
                if (c == null || !c.intact() || (c.next != null && !c.next.intact())) {
                    bad++;
                }
            }
            badChildren = badChildren + bad;
            back = this;
            finalizedThisRound = finalizedThisRound + 1;
        }
    }

    /**
     * One round: the finalizable takes {@code aged} children from the aged pool
     * (each with a young tail) and {@code young} fresh ones; only the weak
     * references leave this frame. Returns how many of them still read set
     * right after {@code System.gc()}.
     */
    @SuppressWarnings("removal")
    static int round(int r, int aged, int young) {
        WeakReference<?>[] weak = makeAndDrop(r, aged, young);
        System.gc();
        int kept = 0;
        for (WeakReference<?> w : weak) {
            if (w.get() != null) {
                kept++;
            }
        }
        return kept;
    }

    /** Builds the round's graph; the finalizable dies when this returns. */
    static WeakReference<?>[] makeAndDrop(int r, int aged, int young) {
        Child[] kids = new Child[aged + young];
        for (int i = 0; i < aged; i++) {
            int slot = r * aged + i;
            Child c = agedPool[slot];
            agedPool[slot] = null;
            c.next = new Child(1000 + slot);
            kids[i] = c;
        }
        for (int i = 0; i < young; i++) {
            kids[aged + i] = new Child(-1 - i);
        }
        WeakReference<?>[] weak = new WeakReference<?>[kids.length];
        for (int i = 0; i < kids.length; i++) {
            weak[i] = new WeakReference<>(kids[i]);
        }
        new Fin(kids);
        return weak;
    }

    @SuppressWarnings("removal")
    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 60;
        int aged = args.length > 1 ? Integer.parseInt(args[1]) : 6;
        int young = args.length > 2 ? Integer.parseInt(args[2]) : 6;
        agedPool = new Child[rounds * aged];
        for (int i = 0; i < agedPool.length; i++) {
            agedPool[i] = new Child(1 + i);
        }
        for (int i = 0; i < 20; i++) {
            System.gc();
        }
        Thread churn = new Thread(() -> {
            Object[] ring = new Object[64];
            int n = 0;
            while (!stop) {
                ring[n++ & 63] = new byte[1024];
            }
            sinkObj = ring;
        }, "churn");
        churn.setDaemon(true);
        churn.start();
        int roundsCleared = 0;
        int roundsFinalized = 0;
        int keptTotal = 0;
        for (int r = 0; r < rounds; r++) {
            finalizedThisRound = 0;
            int kept = round(r, aged, young);
            keptTotal += kept;
            if (kept == 0) {
                roundsCleared++;
            }
            int waits = 0;
            while (finalizedThisRound == 0 && waits < 500) {
                System.runFinalization();
                Thread.sleep(2);
                waits++;
            }
            if (finalizedThisRound == 1) {
                roundsFinalized++;
            }
            back = null;
        }
        stop = true;
        churn.join();
        System.out.println("rounds " + rounds);
        System.out.println("weak-to-private-children-cleared-every-round " + (roundsCleared == rounds));
        System.out.println("finalized-every-round " + (roundsFinalized == rounds));
        System.out.println("children-intact " + (badChildren == 0));
        System.err.println("[gq31c] rounds=" + rounds + " cleared_rounds=" + roundsCleared
                + " kept_weak_total=" + keptTotal + " finalized_rounds=" + roundsFinalized
                + " bad_children=" + badChildren);
    }
}
