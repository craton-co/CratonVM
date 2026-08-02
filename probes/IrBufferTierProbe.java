/**
 * Throughput probe for the IR code-buffer estimate change (2026-08-01).
 *
 * The estimate decides which methods the optimizing (IR/C2) tier gets to
 * produce a body for: too small and the compile is discarded to the single-pass
 * backend. Raising it therefore MOVES A POPULATION OF METHODS BETWEEN TIERS,
 * and the optimizing tier is not uniformly faster — see
 * `reference_c2_tier_slower_because_fields_take_the_helper`.
 *
 * Neither obvious measurement can see that:
 *
 *   - `bench/CratonBench` produces exactly TWO IR compiles in the whole suite,
 *     identical in both arms; its timings differ only by host load.
 *   - a Spring Boot class produces ~600, but its wall clock is dominated by
 *     context boot and I/O and swings 2x run to run on a shared box.
 *
 * So this reproduces the shape that actually changes arms.
 *
 * **The calls must be VIRTUAL, and megamorphic.** The first draft of this probe
 * used `invokestatic` to monomorphic helpers and never overflowed anything:
 * those lower to direct calls (~60 bytes), while the census's expensive call
 * nodes are MIC + 4-way-PIC dual-ABI inline-cache sites (~360+ bytes each).
 * That draft measured 1383 ms vs 1727 ms between the arms and the difference
 * was pure host noise, because the two arms compiled it identically — a
 * textbook false null.
 *
 * Run both arms of ONE binary:
 *     <exe> -cp . IrBufferTierProbe
 *     CRATONVM_JIT_IR_LEGACY_BUFFER_ESTIMATE=1 <exe> -cp . IrBufferTierProbe
 *
 * ALWAYS confirm engagement before reading a timing:
 *     CRATONVM_DBG_IR_BUFSIZE=1 … 2>&1 | grep -c overflow=true
 * must be non-zero in the OLD arm and zero in the NEW one.
 */
public class IrBufferTierProbe {

    interface Op { int apply(int a, int b); }

    static final class Mix     implements Op { public int apply(int a,int b){ return (a*31) ^ (b+7); } }
    static final class Fold    implements Op { public int apply(int a,int b){ return (a ^ (b<<3)) - (a>>>2); } }
    static final class Blend   implements Op { public int apply(int a,int b){ return (a+b) * 0x9E3779B1; } }
    static final class Scatter implements Op { public int apply(int a,int b){ return (a ^ 0x5bd1e995) + (b*17); } }
    static final class Gather  implements Op { public int apply(int a,int b){ return (a|b) ^ ((a&b)<<1); } }
    static final class Rotate  implements Op { public int apply(int a,int b){ return Integer.rotateLeft(a, b & 31) ^ b; } }

    /** Six distinct receiver classes at every site => megamorphic => PIC. */
    static final Op[] OPS = { new Mix(), new Fold(), new Blend(), new Scatter(), new Gather(), new Rotate() };

    /** Five inline-cache sites in one modest graph. */
    static int step(Op[] ops, int x, int y) {
        int a = ops[0].apply(x, y);
        int b = ops[1].apply(a, y);
        int c = ops[2].apply(b, x);
        int d = ops[3].apply(c, a);
        return ops[4].apply(d, b);
    }

    /** Thirteen of them — the census's call-heavy tail. */
    static int wide(Op[] ops, int x, int y) {
        int a = ops[0].apply(x, y);
        int b = ops[1].apply(a, y);
        int c = ops[2].apply(b, x);
        int d = ops[3].apply(c, a);
        int e = ops[4].apply(d, b);
        int f = ops[5].apply(e, c);
        int g = ops[0].apply(f, d);
        int h = ops[1].apply(g, e);
        int i = ops[2].apply(h, f);
        int j = ops[3].apply(i, g);
        int k = ops[4].apply(j, h);
        int l = ops[5].apply(k, i);
        return ops[0].apply(l, j);
    }

    public static void main(String[] args) {
        long iters = (args.length > 0) ? Long.parseLong(args[0]) : 20_000_000L;

        // Rotate the array between warm-up passes so every site sees every
        // receiver class and the caches go megamorphic rather than settling
        // monomorphic on the first one they meet.
        int warm = 0;
        for (int pass = 0; pass < 6; pass++) {
            Op[] rotated = new Op[OPS.length];
            for (int i = 0; i < OPS.length; i++) {
                rotated[i] = OPS[(i + pass) % OPS.length];
            }
            for (int i = 0; i < 60_000; i++) {
                warm ^= step(rotated, i, i + 1) ^ wide(rotated, i, i + 3);
            }
        }

        long t0 = System.nanoTime();
        int acc = warm;
        for (long i = 0; i < iters; i++) {
            int x = (int) i;
            acc ^= step(OPS, x, x + 1);
            acc += wide(OPS, x, x + 3);
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;

        // Checksum first: a faster wrong answer is a bug, not a result.
        System.out.println("IR_BUFFER_TIER_PROBE ms=" + ms + " checksum=" + acc + " iters=" + iters);
    }
}
