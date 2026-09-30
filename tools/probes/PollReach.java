/**
 * The frame-traffic kernel: a counted loop whose ENTIRE live state is two
 * `long`s and an `int`, so every frame word the optimizing tier writes in the
 * body is an intermediate rather than a spill.
 *
 * `hotLoop`'s body is one `Add` over two single-use operands — an `I2L` of the
 * induction variable and a `UShr` of the accumulator. That is the smallest
 * shape in which the single-use carry can want BOTH of a consumer's operands
 * at once, which is what
 * `c2-one-carry-slot-is-the-frame-traffic-ceiling-FIXED-20260910.md`
 * measures. Nothing in the body allocates, calls, loads or can trap, so the
 * disassembly is the arithmetic and the frame traffic and nothing else.
 *
 * `pairLoop` is the same idea with THREE binary consumers stacked, so a run
 * that only ever pairs one consumer per block is distinguishable from one that
 * pairs every consumer it can. `wideLoop` is four independent chains: it
 * separates "the carry removed work" from "the carry removed a latency
 * bottleneck the machine was hiding anyway".
 *
 * `hotLoop` is called `probe.reps` times so it reaches the optimizing tier
 * through the invocation-count door as well as through OSR.
 */
public class PollReach {

    long hotLoop(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += i ^ (s >>> 3);
        }
        return s;
    }

    long pairLoop(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += (i ^ (s >>> 3)) + ((s >>> 5) & i);
        }
        return s;
    }

    long wideLoop(int n) {
        long a = 0, b = 1, c = 2, d = 3;
        for (int i = 0; i < n; i++) {
            a += i ^ (a >>> 3);
            b += i ^ (b >>> 3);
            c += i ^ (c >>> 3);
            d += i ^ (d >>> 3);
        }
        return a + b + c + d;
    }

    public static void main(String[] args) {
        PollReach p = new PollReach();
        int reps = Integer.getInteger("probe.reps", 2000);
        int n = Integer.getInteger("probe.n", 20000);
        String kind = System.getProperty("probe.kind", "hot");
        long acc = 0;
        // Warm both doors before the clock starts.
        for (int r = 0; r < 50; r++) {
            acc += run(p, kind, 1000);
        }
        acc = 0;
        long t0 = System.nanoTime();
        for (int r = 0; r < reps; r++) {
            acc += run(p, kind, n);
        }
        long t1 = System.nanoTime();
        System.out.println("acc=" + acc + " ms=" + (t1 - t0) / 1000000L);
    }

    private static long run(PollReach p, String kind, int n) {
        if (kind.equals("pair")) {
            return p.pairLoop(n);
        }
        if (kind.equals("wide")) {
            return p.wideLoop(n);
        }
        return p.hotLoop(n);
    }
}
