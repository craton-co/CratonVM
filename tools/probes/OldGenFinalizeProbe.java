/**
 * A finalizable object that dies OLD is finalized, as on HotSpot. gc-common
 * round 2026-09-24 (w6-d),
 * `docs/internal/gc-common-round-20260923/common-d-generational-old-gen-finalizables-are-never-finalized-RETIRED-20260928.md`.
 *
 * <p>The objects are aged by allocation-driven collections only (40 x 16 MB
 * of churn while they are strongly held), so the Generational backend has
 * promoted them before they are dropped. `RefCheckOld` never showed this: its
 * `System.gc()` drives keep its finalizables young on that backend.
 *
 * <p>Pass any argument to add a `System.gc()` per round. Prints
 * `finalized=N/64` and `PROBE-OK` when all 64 ran. Measured on the gc-common
 * w5 binary, `-Xmx256m`: HotSpot 64/64 both arms; G1 and ZGC 64/64 both arms;
 * Generational 0/64 both arms.
 */
public class OldGenFinalizeProbe {
    static final int N = 64;
    static final Object LOCK = new Object();
    static int finalized = 0;

    static final class Fin {
        final byte[] pad = new byte[256];

        @SuppressWarnings("removal")
        @Override
        protected void finalize() {
            synchronized (LOCK) {
                finalized++;
            }
        }
    }

    static Object sink;

    static void churn(int mb) {
        for (int i = 0; i < mb * 64; i++) {
            sink = new byte[16 * 1024];
        }
    }

    @SuppressWarnings("removal")
    public static void main(String[] args) throws Exception {
        Fin[] fins = new Fin[N];
        for (int i = 0; i < N; i++) {
            fins[i] = new Fin();
        }
        for (int r = 0; r < 40; r++) {
            churn(16);
        }
        java.util.Arrays.fill(fins, null);
        fins = null;
        int got = 0;
        for (int r = 0; r < 20; r++) {
            churn(16);
            if (args.length > 0) {
                System.gc();
            }
            System.runFinalization();
            Thread.sleep(20);
            synchronized (LOCK) {
                got = finalized;
            }
            if (got >= N) {
                break;
            }
        }
        System.out.println("finalized=" + got + "/" + N
                + (args.length > 0 ? " (with System.gc)" : " (allocation-driven)")
                + (got == N ? " PROBE-OK" : " PROBE-FAIL"));
    }
}
