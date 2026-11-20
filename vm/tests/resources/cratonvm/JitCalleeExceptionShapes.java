package cratonvm;

/**
 * Two shapes a compiled callee's escaping exception has to survive, both taken
 * from bc-java.
 *
 * <p><b>1. A callee that declares a handler but cannot catch THIS throw.</b>
 * {@code outsideTryStep} latches a flag, then calls a method that throws from
 * OUTSIDE every protected range. The JVM answer is to propagate to the caller.
 * Re-running the callee from its entry instead — which is what the JIT's
 * dispatch helper used to do whenever it could not resume the callee's own
 * handler — executes the prefix a second time, and for a prefix that is not
 * idempotent that does not merely duplicate work: the re-run takes the early
 * exit and the exception is LOST. BouncyCastle's
 * {@code CipherInputStream.nextChunk} is exactly this shape —
 * {@code finaliseCipher()} sets {@code finalized = true} and THEN throws on a
 * bad AEAD tag, so the re-run returned a clean EOF over tampered ciphertext.
 *
 * <p><b>2. Two disjoint protected ranges catching the SAME type.</b> With the
 * throw pc unknown, a handler search that matches typed rows by exception class
 * alone picks the FIRST row whichever range actually threw.
 * {@code ProvRevocationChecker.check} is that shape, and running the wrong
 * handler made a PKIX revocation check re-issue the query that had just failed
 * instead of falling back.
 *
 * <p>Both checksums are computed by construction, not measured: shape 1 is
 * "every iteration threw exactly once and ran its prefix exactly once", shape 2
 * is "no iteration ran the wrong handler". Real HotSpot agrees with both.
 */
public class JitCalleeExceptionShapes {

    static boolean latched;
    static int prefixRuns;

    static int work(int x) {
        return x + 1;
    }

    /** Latches, then throws — the non-idempotent prefix's partner. */
    static void finish() {
        throw new IllegalStateException("finish");
    }

    /**
     * Declares a handler, but the throw comes from before the {@code try}, so
     * nothing in this method can catch it.
     */
    static int outsideTryStep(int x) {
        if (latched) {
            return -1;
        }
        latched = true;
        prefixRuns++;
        finish();
        try {
            return work(x);
        } catch (RuntimeException e) {
            return -2;
        }
    }

    /**
     * 20000 iterations; each must throw out of {@code outsideTryStep} exactly
     * once, having run its prefix exactly once. Golden value 20000.
     */
    public static int outsideTryChecksum() {
        int caught = 0;
        int wrong = 0;
        for (int i = 0; i < 20000; i++) {
            latched = false;
            prefixRuns = 0;
            try {
                int r = outsideTryStep(i);
                // Reaching here at all means the exception was swallowed.
                wrong += (r == -1) ? 1 : 2;
            } catch (IllegalStateException e) {
                caught++;
            }
            if (prefixRuns != 1) {
                wrong += 1000;
            }
        }
        return caught - wrong;
    }

    static int branchA(int x) {
        throw new IllegalStateException("A");
    }

    static int branchB(int x) {
        throw new IllegalStateException("B");
    }

    /** Two disjoint ranges, one catch type, different handlers. */
    static int twoRangesStep(int x, boolean first) {
        if (first) {
            try {
                return branchA(x);
            } catch (IllegalStateException e) {
                return 1;
            }
        } else {
            try {
                return branchB(x);
            } catch (IllegalStateException e) {
                return 2;
            }
        }
    }

    /** 0 when every throw reached the handler of ITS OWN range. */
    public static int twoRangesChecksum() {
        int bad = 0;
        for (int i = 0; i < 20000; i++) {
            if (twoRangesStep(i, true) != 1) {
                bad++;
            }
            if (twoRangesStep(i, false) != 2) {
                bad++;
            }
        }
        return bad;
    }

    public static void main(String[] args) {
        System.out.println("outsideTryChecksum=" + outsideTryChecksum());
        System.out.println("twoRangesChecksum=" + twoRangesChecksum());
    }
}
