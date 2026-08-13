import org.h2.message.Trace;
import org.h2.message.TraceSystem;

/**
 * Per-call cost of the H2 debug-trace gate that {@code TraceObject.debugCodeCall}
 * runs on every JDBC API call: {@code Trace.isDebugEnabled()} -> private
 * {@code isEnabled(3)} -> an {@code invokeinterface} to
 * {@code TraceSystem.isEnabled} when {@code traceLevel} is {@code PARENT}.
 *
 * Written to answer the open question on the retired
 * `bug-h2-hang-cluster-lirs-trace-mvstore-compact` page: a single stack dump on
 * `org.h2.test.synth.TestBtreeIndex` landed in `Trace.isEnabled`, and the page
 * asked whether that gate is unexpectedly costly under CratonVM.
 *
 * Measured 2026-08-10 on one Azure host:
 *
 *   HotSpot C2          1.16 ns per call
 *   HotSpot -Xint       57 - 72 ns
 *   cratonvm --nojit    2560 - 2990 ns
 *   cratonvm JIT        2060 - 3180 ns
 *
 * ~42x HotSpot's own interpreter -- but {@link CallShapeBench}, a synthetic
 * chain of the same shape, costs the same, so this gate is NOT special: it is
 * the generic per-call rung. The JIT arm is within noise of --nojit, so it is
 * also not a JIT dispatch pathology.
 *
 * Two anti-measurement-bug precautions:
 *
 *  - TWO RECEIVERS WITH DIFFERENT ANSWERS, summed into a volatile sink. A first
 *    version used one always-false Trace and HotSpot C2 measured 0.00 ns/call:
 *    it had proved the loop had no effect and deleted it. Any "CratonVM is
 *    Nx HotSpot" number taken that way is measuring dead-code elimination.
 *  - THE LOOP IS NOT INLINE IN main(). An OSR-only loop is refused tier-up.
 *
 *   javac -cp "$H2CP" -d . TraceBench.java
 *   java -Xint -cp "$H2CP:." TraceBench
 *   <cratonvm> --java-home $JDK25 --nojit --Xmx 2g -c "$H2CP:." TraceBench
 */
public class TraceBench {
    static volatile int sink;

    static long bench(Trace[] ts, int n) {
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

    public static void main(String[] a) throws Exception {
        TraceSystem quiet = new TraceSystem(null);
        quiet.setLevelSystemOut(TraceSystem.OFF);
        TraceSystem loud = new TraceSystem(null);
        loud.setLevelSystemOut(TraceSystem.DEBUG);
        Trace[] ts = { quiet.getTrace("q"), loud.getTrace("l") };
        System.out.println("answers: " + ts[0].isDebugEnabled() + " " + ts[1].isDebugEnabled());
        int n = 20_000_000;
        for (int rep = 0; rep < 4; rep++) {
            long ns = bench(ts, n);
            System.out.printf("rep %d: %d calls in %.3f ms -> %.2f ns/call%n",
                    rep, n, ns / 1e6, (double) ns / n);
        }
        System.out.println("sink=" + sink);
    }
}
