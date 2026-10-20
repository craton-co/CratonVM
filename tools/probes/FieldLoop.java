/**
 * The loop that made the tier inversion visible: an instance-field read
 * accumulated across a counted loop, with nothing else in the body.
 *
 * `sum` is called `probe.reps` times so it reaches the JIT through the
 * invocation-count door rather than only through OSR — the two doors compile
 * different bodies, and the comparison is about the body the first-call door
 * emits.
 *
 * `sumWide` is the same loop with four INDEPENDENT accumulators. It is not a
 * second benchmark: the ratio between the two arms separates a latency
 * bottleneck (which independent work overlaps) from extra work (which it does
 * not).
 */
public class FieldLoop {
    int fx = 3;

    int sum(int n) {
        int a = 0;
        for (int i = 0; i < n; i++) {
            a += this.fx;
        }
        return a;
    }

    int sumWide(int n) {
        int a = 0, b = 0, c = 0, d = 0;
        for (int i = 0; i < n; i++) {
            a += this.fx;
            b += this.fx;
            c += this.fx;
            d += this.fx;
        }
        return a + b + c + d;
    }

    public static void main(String[] args) {
        FieldLoop p = new FieldLoop();
        int reps = Integer.getInteger("probe.reps", 2000);
        int n = Integer.getInteger("probe.n", 20000);
        boolean wide = Boolean.getBoolean("probe.wide");
        long acc = 0;
        // Warm both doors before the clock starts.
        for (int r = 0; r < 50; r++) {
            acc += wide ? p.sumWide(1000) : p.sum(1000);
        }
        long t0 = System.nanoTime();
        for (int r = 0; r < reps; r++) {
            acc += wide ? p.sumWide(n) : p.sum(n);
        }
        long t1 = System.nanoTime();
        System.out.println("acc=" + acc + " ms=" + (t1 - t0) / 1000000L);
    }
}
