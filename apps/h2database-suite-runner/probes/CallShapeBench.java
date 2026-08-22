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
 * Re-measured 2026-08-21, interleaved arms, MIN of 5 on one Azure host at
 * load 4.4-6.6 (the minimum is the least-contaminated sample on a shared box):
 *
 *   HotSpot -Xint      31.85 ns net per 3-call chain,   5.46 ns/iter loop-only
 *   cratonvm --nojit  871.76 ns                        80.98 ns/iter
 *
 * i.e. ~27x on the calls and ~15x on the call-free loop: A CALL LEVEL IS
 * ~291 ns INTERPRETED against ~10.6 ns. The 2026-08-10 reading of this same
 * probe was ~700 ns and 40-48x; the call-free loop ratio did not move
 * (13-21x then, 14.8x now), so the whole improvement is in the CALL path.
 *
 * READ THE INTERNAL RATIO, NOT THE ABSOLUTE. Per rep the chain/loop ratio was
 * 10.8, 10.2, 9.9 for cratonvm and 5.6, 4.6, 5.0 for HotSpot while the
 * absolutes moved 40%: host speed cancels, the shape does not.
 *
 * See performance/h2-update-path-throughput-RETIRED-20260821.md.
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
