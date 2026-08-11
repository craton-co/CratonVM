/**
 * The per-call rung: what one interpreted Java call level costs.
 *
 * Control for {@link TraceBench}. It rebuilds H2 {@code Trace.isDebugEnabled()}'s
 * exact shape out of synthetic classes -- bimorphic receiver, invokevirtual ->
 * invokevirtual -> invokeinterface, four field loads, no allocation -- and
 * subtracts an otherwise-identical loop that makes NO calls. If the chain costs
 * here what it costs on H2's own Trace, the number is the generic per-call cost
 * and H2's tracing is not a discrete inefficiency.
 *
 * Measured 2026-08-10, interleaved arms on one Azure host at load 11-12:
 *
 *   HotSpot -Xint      46 - 49 ns net per 3-call chain,  6 - 11 ns/iter loop-only
 *   cratonvm --nojit   1756 - 2231 ns                  125 - 153 ns/iter
 *
 * i.e. ~40-48x on the calls and ~13-21x on the call-free loop: a call level is
 * ~700 ns interpreted against ~15 ns. See
 * docs/known-issues/h2/h2-update-path-throughput-20260802.md.
 *
 * Two anti-measurement-bug precautions, both learned the hard way here:
 *
 *  - TWO RECEIVERS WITH DIFFERENT ANSWERS, summed into a volatile sink. A first
 *    version used one always-false receiver and HotSpot C2 measured 0.00
 *    ns/call: it had proved the loop had no effect and deleted it.
 *  - THE LOOP IS NOT INLINE IN main(). An OSR-only loop is refused tier-up and
 *    measures something else entirely.
 *
 * Compare against `java -Xint`, never against C2 -- C2 is measuring its own
 * inliner, not a call.
 *
 *   javac -d . CallShapeBench.java
 *   java -Xint -cp . CallShapeBench
 *   <cratonvm> --java-home $JDK25 --nojit --Xmx 2g -c . CallShapeBench
 */
public class CallShapeBench {
    interface W {
        boolean isEnabled(int level);
    }

    static final class Sys implements W {
        final int levelMax;

        Sys(int levelMax) {
            this.levelMax = levelMax;
        }

        @Override
        public boolean isEnabled(int level) {
            if (levelMax == 4) {
                return true;
            }
            return level <= levelMax;
        }
    }

    static class T {
        final int traceLevel;
        final W writer;

        T(int traceLevel, W writer) {
            this.traceLevel = traceLevel;
            this.writer = writer;
        }

        boolean isEnabled(int level) {
            if (traceLevel == -1) {
                return writer.isEnabled(level);
            }
            return level <= traceLevel;
        }

        boolean isDebugEnabled() {
            return isEnabled(3);
        }
    }

    static volatile int sink;

    static long bench(T[] ts, int n) {
        long t0 = System.nanoTime();
        int c = 0;
        for (int i = 0; i < n; i++) {
            if (ts[i & 1].isDebugEnabled()) {
                c++;
            }
        }
        sink += c;
        return System.nanoTime() - t0;
    }

    /** Same loop, same bimorphic array access, no calls. */
    static long benchEmpty(T[] ts, int n) {
        long t0 = System.nanoTime();
        int c = 0;
        for (int i = 0; i < n; i++) {
            if (ts[i & 1] != null) {
                c++;
            }
        }
        sink += c;
        return System.nanoTime() - t0;
    }

    public static void main(String[] a) {
        T[] ts = { new T(-1, new Sys(0)), new T(-1, new Sys(4)) };
        System.out.println("answers: " + ts[0].isDebugEnabled() + " " + ts[1].isDebugEnabled());
        int n = 20_000_000;
        for (int rep = 0; rep < 3; rep++) {
            long ns = bench(ts, n);
            long e = benchEmpty(ts, n);
            System.out.printf("rep %d: chain %.2f ns/call, loop-only %.2f ns/iter -> chain-net %.2f ns%n",
                    rep, (double) ns / n, (double) e / n, (double) (ns - e) / n);
        }
        System.out.println("sink=" + sink);
    }
}
