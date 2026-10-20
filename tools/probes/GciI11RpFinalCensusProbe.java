/**
 * gci i11/rp (finding R1 of
 * docs/known-issues/gc/gci-i11rp-generational-reference-processing-review-findings-20261013.md):
 * one finalizable object dies, then one {@code System.gc()}. Run with
 * {@code -Xlog:gc+ref=debug:stdout:none}: the three {@code gc,ref} lines say
 * which reference-processing phases ran. HotSpot 25 Serial discovers the dead
 * object's {@code FinalReference}, so its phase 1 ({@code SoftWeakFinalRefsPhase})
 * and phase 2 ({@code KeepAliveFinalRefsPhase}, {@code marks_oops_alive: true})
 * both print {@code ReferenceProcessor::execute ...}; this VM printed
 * {@code Skipped KeepAliveFinalRefsPhase of Reference Processing: no references}
 * because its census never counted a resurrected finalizable.
 *
 * <p>stdout: the log lines, then {@code finalized=true} once {@code finalize()}
 * has run (waited for up to 10 s). Nothing else is printed.
 */
public class GciI11RpFinalCensusProbe {
    static volatile int finalized;

    static final class Fin {
        @SuppressWarnings("removal")
        @Override
        protected void finalize() {
            finalized = finalized + 1;
        }
    }

    /** Allocates the finalizable object in a frame that returns at once. */
    static void dropOne() {
        new Fin();
    }

    @SuppressWarnings("removal")
    public static void main(String[] args) throws Exception {
        dropOne();
        System.gc();
        long deadline = System.nanoTime() + 10_000_000_000L;
        while (finalized == 0 && System.nanoTime() < deadline) {
            System.runFinalization();
            Thread.sleep(10);
        }
        System.out.println("finalized=" + (finalized == 1));
    }
}
