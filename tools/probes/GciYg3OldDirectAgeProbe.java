/**
 * gci i3/yg: does the HotSpot age clock count allocations that go straight to
 * the old generation? (`gci-i2yg-young-trigger-loop-and-age-clock-review-findings`
 * item 4, switch `CRATONVM_GEN_SERIAL_EDEN_AGE_CLOCK_OLD_DIRECT`.)
 *
 * <p>Run at {@code -Xmx256m}. 64 finalizable objects are held across 40 rounds;
 * each round allocates one 34 MiB {@code byte[]} (above the Generational
 * collector's humongous threshold, half its 64 MiB young semi-space, so it is
 * allocated in the old generation; below HotSpot Serial's ~68 MiB eden, so
 * HotSpot allocates it in eden) and 16 MiB of 16 KiB arrays, all dropped at
 * once. HotSpot Serial runs a young collection about every 68 MiB of that
 * (~29 over the hold), so the held objects reach the tenuring threshold (15)
 * and die OLD: dropped, then followed only by young-sized churn, none of them
 * is finalized ({@code finalized=0/64}). An age clock that does not count the
 * 34 MiB arrays ticks about once per 68 MiB of the small churn alone (~9 over
 * the hold), keeps the objects young, and finalizes them at the next young
 * cycle.
 *
 * <p>Stdout is the count only (compared with HotSpot); stderr carries the
 * round progress.
 */
public class GciYg3OldDirectAgeProbe {
    static final int N = 64;
    static final int ROUNDS = 40;
    static final int BIG = 34 * 1024 * 1024;
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
        for (int r = 0; r < ROUNDS; r++) {
            sink = new byte[BIG];
            sink = null;
            churn(16);
            if (r % 10 == 9) {
                System.err.println("[yg3-od] round " + (r + 1) + "/" + ROUNDS);
            }
        }
        java.util.Arrays.fill(fins, null);
        fins = null;
        int got = 0;
        for (int r = 0; r < 4; r++) {
            churn(16);
            System.runFinalization();
            Thread.sleep(20);
            synchronized (LOCK) {
                got = finalized;
            }
        }
        System.out.println("finalized=" + got + "/" + N + " after an old-direct hold");
    }
}
