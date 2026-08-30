import java.util.Random;

/**
 * The regression test for identity-hash side-table EVICTION: a `Random` that is
 * still reachable must keep its generator state while millions of other hashed
 * objects die around it.
 *
 * <p>The hazard the eviction hook introduces is precise. The collector reports
 * the identity hashes of objects it reclaimed, and `securerandom.rs` removes
 * those keys from `SEED_TABLE`/`GAUSSIAN_TABLE`. If it ever reported a LIVE
 * object's hash -- a dead base that a survivor had already slid onto, say --
 * that live `Random`'s seed entry would vanish, and the next `nextInt()` would
 * lazily re-seed it from OS entropy. The draw would still succeed, which is why
 * this has to be checked by VALUE and not by liveness.
 *
 * <p>So every number below is a pure function of the fixed seeds, and the whole
 * run reduces to one checksum. Run it on real HotSpot and on CratonVM and diff:
 * identical output means every live generator kept its state across every
 * cycle; a re-seeded generator diverges immediately and cannot be made to agree
 * twice, because the replacement seed is drawn from entropy.
 */
public class RandomLiveAcrossGc {
    public static void main(String[] args) throws Exception {
        int live = Integer.getInteger("live", 200);
        int churn = Integer.getInteger("churn", 200_000);
        int rounds = Integer.getInteger("rounds", 6);

        // Held for the whole run: these are the generators whose state must
        // survive. Seeded, so their sequences are fixed.
        Random[] keep = new Random[live];
        for (int i = 0; i < live; i++) {
            keep[i] = new Random(i);
        }

        long sum = 0;
        for (int i = 0; i < live; i++) {
            sum += keep[i].nextInt();
        }

        long junkSink = 0;
        for (int r = 0; r < rounds; r++) {
            // GARBAGE THAT IS HASHED. Every one of these takes an identity hash
            // (that is how its seed is keyed), so every one of them is a hash
            // the sweep will report as dead -- which is exactly the batch the
            // live generators above have to survive.
            for (int i = 0; i < churn; i++) {
                junkSink += new Random(i).nextInt();
            }
            System.gc();
            Thread.sleep(20);

            // Interleaved with the churn, not just after it: a live entry that
            // is evicted mid-run has to show up in the checksum.
            for (int i = 0; i < live; i++) {
                sum += keep[i].nextInt();
            }
        }

        // `haveNextNextGaussian` lives in a SECOND table keyed the same way, so
        // it needs its own witness: a gaussian pair half-consumed before a GC
        // and completed after it.
        double gauss = 0;
        for (int i = 0; i < live; i++) {
            gauss += keep[i].nextGaussian();
        }
        System.gc();
        Thread.sleep(20);
        for (int i = 0; i < live; i++) {
            gauss += keep[i].nextGaussian();
        }

        System.out.println("CK live-across-gc sum=" + sum);
        System.out.printf("CK live-across-gc gaussian=%.12f%n", gauss);
        System.out.println("CK churn-nonzero=" + (junkSink != 0));
    }
}
