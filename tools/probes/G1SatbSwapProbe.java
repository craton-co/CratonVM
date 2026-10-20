// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
// g1put (2026-10-05): does a JIT-compiled reference OVERWRITE keep the SATB
// snapshot under G1's concurrent mark?
//
// Holders each own one Token; a hot, compiled loop swaps tokens between random
// holders (`t = a.f; a.f = b.f; b.f = t`), so every store overwrites a non-null
// reference: the classic snapshot-at-the-beginning hazard. A retained ~90 MB
// ballast keeps the heap above G1's initiating occupancy (run with
// -Xmx256m -XX:InitiatingHeapOccupancyPercent=20), and a ballast slice is
// replaced every round so old garbage keeps accumulating and cycles keep
// starting. A store that skips its pre-barrier while a mark is running can
// leave a token unmarked; the next mixed collection then frees it and the
// final check finds a corrupt token or a wrong id sum.
//
// Usage: G1SatbSwapProbe [ballastMiB] [holders] [rounds] [itersPerRound]
public final class G1SatbSwapProbe {
    static final class Token {
        final int id;
        final int magic;
        final long[] body;

        Token(int id) {
            this.id = id;
            this.magic = id * 0x9E3779B1 ^ 0x5A5A5A5A;
            this.body = new long[4];
            this.body[id & 3] = id;
        }

        boolean valid() {
            return magic == (id * 0x9E3779B1 ^ 0x5A5A5A5A) && body != null && body.length == 4
                    && body[id & 3] == id;
        }
    }

    static final class Holder {
        Token f;
    }

    static Object[][] ballast;
    static long[][] junk = new long[1024][];

    static int arg(String[] a, int i, int d) {
        return a.length > i ? Integer.parseInt(a[i]) : d;
    }

    static Object[] slice(int seed) {
        Object[] s = new Object[256];
        for (int i = 0; i < s.length; i++) {
            long[] x = new long[46];
            x[0] = seed + i;
            s[i] = x;
        }
        return s;
    }

    // The compiled kernel: swaps, with young churn so collections keep coming.
    static long swap(Holder[] hs, int iters, int seed) {
        int n = hs.length;
        int x = seed;
        long s = 0;
        for (int i = 0; i < iters; i++) {
            x = x * 1103515245 + 12345;
            int a = (x >>> 8) % n;
            x = x * 1103515245 + 12345;
            int b = (x >>> 8) % n;
            Holder ha = hs[a];
            Holder hb = hs[b];
            Token t = ha.f;
            ha.f = hb.f;
            hb.f = t;
            s += t.id;
            if ((i & 15) == 0) {
                junk[(i >>> 4) & 1023] = new long[8];
            }
        }
        return s;
    }

    public static void main(String[] args) {
        int ballastMiB = arg(args, 0, 90);
        int holders = arg(args, 1, 4096);
        int rounds = arg(args, 2, 60);
        int iters = arg(args, 3, 1_000_000);
        int slices = ballastMiB * 1024 * 1024 / (256 * 400);
        ballast = new Object[slices][];
        for (int i = 0; i < slices; i++) {
            ballast[i] = slice(i);
        }
        Holder[] hs = new Holder[holders];
        for (int i = 0; i < holders; i++) {
            hs[i] = new Holder();
            hs[i].f = new Token(i);
        }
        long acc = 0;
        for (int r = 0; r < rounds; r++) {
            acc += swap(hs, iters, r * 7919 + 1);
            ballast[(r * 131) % slices] = slice(r);
        }
        long corrupt = 0;
        long sum = 0;
        boolean[] seen = new boolean[holders];
        for (Holder h : hs) {
            if (h.f == null || !h.f.valid() || h.f.id < 0 || h.f.id >= holders || seen[h.f.id]) {
                corrupt++;
                continue;
            }
            seen[h.f.id] = true;
            sum += h.f.id;
        }
        long want = (long) holders * (holders - 1) / 2;
        System.out.println("g1-satb-swap holders=" + holders + " rounds=" + rounds + " corrupt=" + corrupt
                + " ids_ok=" + (sum == want) + " acc_nonzero=" + (acc != 0));
    }
}
