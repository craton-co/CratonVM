/**
 * gch h1/jw: r17-gc-compiled-chain-walks-stop-at-4096-activations-silently.
 *
 * One JIT chain entry covers every activation of a compiled recursion, and the
 * collector's RBP-chain walks (coverage proof, band verifier, precise scan,
 * Stage 5 remap, register-image remap) used to stop after 4096 parent steps
 * without saying so. This probe runs a compiled recursion deeper than that, on
 * a thread with a 256 MiB stack, with young collections forced at the bottom
 * and again every 512 activations on the way out.
 *
 * Each activation allocates one Box held in a local of its own frame, and also
 * parks it in a heap array (the heap slot is always updated by a moving
 * collection). On the way out the activation compares its local against the
 * heap slot by identity: a frame slot nothing rewrote keeps the pre-move
 * address and compares unequal ("stale"); a slot that names a reclaimed or
 * reused copy fails the field check ("bad").
 *
 * Usage: GchJwDeepCompiledRecursionProbe [depth] [rounds]
 *   depth  default 6000 (> 4097); 3000 is the below-the-guard control
 *   rounds default 3
 * stdout is deterministic (compared with HotSpot); diagnostics go to stderr.
 */
public class GchJwDeepCompiledRecursionProbe {
    static final class Box {
        final int id;
        final long check;
        byte[] pad;

        Box(int id) {
            this.id = id;
            this.check = mix(id);
            this.pad = new byte[24];
        }
    }

    static long mix(int id) {
        long x = id * 0x9E3779B97F4A7C15L;
        return x ^ (x >>> 29);
    }

    static volatile Object sink;
    static Box[] held;
    static int stale;
    static int bad;

    /** Allocate enough to force several young collections. */
    static int churn(int kib) {
        int n = 0;
        for (int i = 0; i < kib; i++) {
            byte[] b = new byte[1024];
            b[i & 1023] = (byte) i;
            sink = b;
            n += b.length >>> 10;
        }
        return n;
    }

    static int rec(int depth, int churnKib) {
        Box mine = new Box(depth);
        held[depth] = mine;
        if (depth < 0) {
            sink = mine; // never taken; keeps escape analysis from removing `mine`
        }
        int r;
        if (depth == 0) {
            r = churn(churnKib);
        } else {
            r = rec(depth - 1, churnKib);
        }
        if ((depth & 511) == 0 && churnKib > 0) {
            r += churn(churnKib / 8);
        }
        if (mine != held[depth]) {
            stale++;
        }
        if (mine.id != depth || mine.check != mix(depth) || mine.pad == null || mine.pad.length != 24) {
            bad++;
        }
        held[depth] = null;
        return r;
    }

    /**
     * Warm `rec` and `churn` into compiled code on shallow recursions (no
     * churn, so no collection depends on the warm-up). On the deep thread.
     */
    static void warm() {
        for (int i = 0; i < 20_000; i++) {
            rec(32, 0);
        }
        stale = 0;
        bad = 0;
    }

    public static void main(String[] args) throws Exception {
        final int depth = args.length > 0 ? Integer.parseInt(args[0]) : 6000;
        final int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 3;
        held = new Box[Math.max(depth, 64) + 1];

        final int[] totals = new int[2];
        final long[] seen = new long[1];
        // gch h2/jr: the warm-up runs on the deep thread, not here. vh1-jw
        // printed `moving_cycles_under_live_jit=0` with every young cycle
        // falling back as `cross-thread-jit-peer`: the 20 000-iteration loop in
        // `main` was OSR-compiled, so `main` sat in compiled code at
        // `deep.join()` -- a live JIT peer of every collection -- and the
        // moving path never ran under the deep chain. `main` now runs no loop.
        Thread deep = new Thread(null, () -> {
            warm();
            for (int round = 0; round < rounds; round++) {
                seen[0] += rec(depth, 64 * 1024);
            }
            totals[0] = stale;
            totals[1] = bad;
        }, "deep", 256L << 20);
        deep.start();
        deep.join();

        System.err.println("[jw-deep] depth=" + depth + " rounds=" + rounds + " churned_kib=" + seen[0]);
        System.out.println("depth " + depth + " rounds " + rounds);
        System.out.println("stale " + totals[0]);
        System.out.println("bad " + totals[1]);
        System.out.println(totals[0] == 0 && totals[1] == 0 ? "PASS" : "FAIL");
    }
}
