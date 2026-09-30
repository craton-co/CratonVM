// Interpreter round i1, lane L7 — cost of re-offering a refused OSR compile on
// every activation.
//
// `loop` has an `athrow` inside a `try` with a handler, which the single-pass
// OSR door refuses (RBC.6). The loop runs 3 000 iterations per call — past the
// 1 000 back-edge OSR threshold — so before this round every INTERPRETED call
// (all of them until method-entry compilation takes over; all 4 000 under
// `CRATONVM_JIT_THRESHOLD=100000`) paid two refused OSR compile attempts
// (admit + jit_scan + exception-table copy) at 1 000 and 2 000 back edges. The
// refusal is now remembered method-wide (`mark_osr_denied`).
//
// Expected stdout (HotSpot 25, any flags), exactly:
//   sum=<same number on every VM>
//   caught=4000
// What to measure: the `elapsed_ms` line on stderr, CratonVM default flags,
// before vs after this round's binary (interleave runs, take medians). With
// `CRATONVM_DBG_JITC=1`, count `osr-DENY (RBC.6 athrow` lines: previously
// roughly one or two per call, now one per install epoch.
public class L7OsrAthrowRetryCost {
    static long loop(int n, int throwAt) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            try {
                if (i == throwAt) {
                    throw new IllegalStateException("x");
                }
                acc += (i ^ (acc >>> 3)) & 0xFFFF;
            } catch (IllegalStateException e) {
                acc -= 7;
                caught++;
            }
        }
        return acc;
    }

    static int caught;

    public static void main(String[] args) {
        long t0 = System.nanoTime();
        long sum = 0;
        for (int call = 0; call < 4_000; call++) {
            sum += loop(3_000, 1_500 + (call & 7));
        }
        long ms = (System.nanoTime() - t0) / 1_000_000;
        System.out.println("sum=" + sum);
        System.out.println("caught=" + caught);
        System.err.println("elapsed_ms=" + ms);
    }
}
