// gci i9/ds (`docs/internal/gc/gci-i9ds-frozen-peer-deopt-stash-not-rooted-RETIRED-RULING-20261018.md`):
// does an object that only a STASHED deopt frame holds survive a collection
// whose take-over froze its thread in compiled code?
//
// Each worker calls `top(new Box(k), i, d)` in a hot loop. The Box is held
// only by the callee chain top -> mid -> leaf (the caller's operand stack
// gives it up at the invoke). Every 97th call passes `d == 0`, so `leaf`'s
// `idiv` traps: in an optimizing-tier body that is a deopt guard, which stashes
// the reconstructed frame (`LAST_DEOPT`) and unwinds the `i64::MIN` sentinel
// through the compiled callers (`mid`, `top`), whose frames are published as
// `PendingException` stashes (`LAST_EXCEPTIONAL`) on the way to `top`'s
// handler. While the sentinel unwinds, the Box is named by the stash alone;
// a take-over that freezes the worker there used to leave it unrooted.
// Both the normal path (`mid` reads `b` after `leaf` returns) and the
// exceptional path (`top`'s handler reads `b`) re-read the Box, and every
// result is compared with a closed form: a Box swept or remapped wrongly
// reads back zero fields and counts as a mismatch.
//
//   GciDs9DeoptStashProbe [workers=4] [iters=100000] [churn=1]
//
// To reach the optimizing tier's deopt guard (the single-pass body throws
// directly and stashes no frame; see probes/DeoptTrapProbe.java) the rows set
//   CRATONVM_C2_ACCEPT=always CRATONVM_TIER_C1_THRESHOLD=100000
//   CRATONVM_TIER_C2_THRESHOLD=600 CRATONVM_TIER_C2_MIN_INVOCATIONS=500
//
// stdout is deterministic (HotSpot prints the same line); diagnostics go to
// stderr as `[gci-ds9] ...`.
public class GciDs9DeoptStashProbe {
    static final class Box {
        final int v;
        final long w;

        Box(int v) {
            this.v = v;
            this.w = 0x9E3779B97F4A7C15L * (v + 1);
        }
    }

    // No allocation and no handler: `d == 0` traps the compiled body.
    static int leaf(Box b, int i, int d) {
        int x = b.v + i;
        return x / d;
    }

    // Holds `b` across the call and reads it after the call returns.
    static long mid(Box b, int i, int d) {
        long pre = b.w;
        int q = leaf(b, i, d);
        return pre + q + b.v;
    }

    // The handler reads `b` after the throw.
    static long top(Box b, int i, int d) {
        try {
            return mid(b, i, d);
        } catch (ArithmeticException e) {
            return b.w ^ b.v;
        }
    }

    static long expected(int v, int i, int d) {
        long w = 0x9E3779B97F4A7C15L * (v + 1);
        if (d == 0) {
            return w ^ v;
        }
        return w + (v + i) / d + v;
    }

    static int divisor(int i) {
        return (i % 97 == 0) ? 0 : 1 + (i & 7);
    }

    static final class Worker extends Thread {
        final int id;
        final int iters;
        long checksum;
        long mismatches;

        Worker(int id, int iters) {
            this.id = id;
            this.iters = iters;
            setName("ds9-worker-" + id);
        }

        @Override
        public void run() {
            long sum = 0;
            long bad = 0;
            for (int i = 0; i < iters; i++) {
                int k = id * 1000003 + i;
                int d = divisor(i);
                long r = top(new Box(k), i, d);
                if (r != expected(k, i, d)) {
                    bad++;
                }
                sum += r;
            }
            checksum = sum;
            mismatches = bad;
        }
    }

    static volatile boolean done;

    public static void main(String[] args) throws Exception {
        int workers = args.length > 0 ? Integer.parseInt(args[0]) : 4;
        int iters = args.length > 1 ? Integer.parseInt(args[1]) : 100000;
        boolean churn = args.length > 2 ? Integer.parseInt(args[2]) != 0 : true;
        long t0 = System.nanoTime();
        Thread churner = null;
        if (churn) {
            churner = new Thread(() -> {
                Object[] ring = new Object[512];
                int n = 0;
                while (!done) {
                    ring[n & 511] = new long[16 + (n & 63)];
                    n++;
                    if ((n & 4095) == 0) {
                        Thread.yield();
                    }
                }
            }, "ds9-churn");
            churner.setDaemon(true);
            churner.start();
        }
        Worker[] ws = new Worker[workers];
        for (int w = 0; w < workers; w++) {
            ws[w] = new Worker(w, iters);
            ws[w].start();
        }
        long checksum = 0;
        long mismatches = 0;
        for (Worker w : ws) {
            w.join();
            checksum += w.checksum;
            mismatches += w.mismatches;
        }
        done = true;
        if (churner != null) {
            churner.join();
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.err.println("[gci-ds9] workers=" + workers + " iters=" + iters
                + " churn=" + (churn ? 1 : 0) + " mismatches=" + mismatches + " ms=" + ms);
        if (mismatches == 0) {
            System.out.println("ds9-deopt-stash: PASS checksum=" + checksum);
        } else {
            System.out.println("ds9-deopt-stash: FAIL mismatches=" + mismatches);
        }
    }
}
