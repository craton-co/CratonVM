// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 16, lane L5: the back-edge counter's steady state
// (docs/known-issues/interpreter/i1-L7-proposal-per-method-backedge-counters-20260923.md,
// "Progress (wave 16)").
//
// Sixteen methods, each one warm loop of a different shape (the fused
// `if_icmp` back edge, `goto`-closed `while`, `do`/`while` on `ifne`, a
// `long` counter, nested loops with two headers in one frame, a loop over an
// array, ...), called over and over.
//
//   * "long" phase: 5 000 iterations per call. Every activation crosses the
//     1 000-back-edge OSR floor. Before wave 16 each back edge past the floor
//     made the out-of-line `try_osr_with_backoff` call, which under --nojit
//     (and for any loop header whose OSR offers were refused) only ever
//     answered "no"; wave 16 removes that under --nojit and spaces it to one
//     call per 256 back edges otherwise (`Frame::osr_poll_at`). This phase
//     should get faster, most visibly under --nojit.
//   * "short" phase: 200 iterations per call, below the floor. Nothing
//     changed there; it is the control and should stay flat.
//
// Wave 20 (lane L5): the first OSR offer of every activation and loop header
// consults the VM's refused-loop budget (`osr_loop_offer_allowed`). Once any
// loop in the VM had a whole activation refused, that consult SipHashed the
// method's name and descriptor and took the budget map's mutex, for every
// header of every method; it now reads the method's memoised key and a
// 256-bit filter, and only a header with a record takes the lock. Expect the
// JIT-on "long" phase to be flat to slightly faster, and only when
// `CRATONVM_DBG_OSR=1` shows an `all-offers-refused` line (no record, no
// change: the one-load fast path was already there); --nojit never reaches
// the consult.
//
// Run: cratonvm --compatible --nojit BackEdgeCounterBench [rounds]
//      (and without --nojit); interleave with the previous build, 5 runs,
//      medians (the microbenchmark-noise note).
// stdout is deterministic: one "checksum=" line, identical on HotSpot 25.
// stderr: per round, ns per back edge for each phase.
public class BackEdgeCounterBench {
    static final int LONG = 5_000;
    static final int SHORT = 200;
    static final int LONG_CALLS = 40;
    static final int SHORT_CALLS = LONG_CALLS * (LONG / SHORT);
    static final int[] DATA = new int[LONG];

    static int l0(int n, int s) {
        for (int i = 0; i < n; i++) {
            s = s * 31 + i;
        }
        return s;
    }

    static int l1(int n, int s) {
        int i = 0;
        while (i < n) {
            s ^= i << 1;
            i++;
        }
        return s;
    }

    static int l2(int n, int s) {
        int i = n;
        do {
            s += i ^ 0x5a5a;
            i--;
        } while (i != 0);
        return s;
    }

    static int l3(int n, int s) {
        long acc = s;
        for (long i = 0; i < n; i++) {
            acc = acc * 17 + i;
        }
        return (int) (acc ^ (acc >>> 32));
    }

    static int l4(int n, int s) {
        int outer = n / 50;
        for (int i = 0; i < outer; i++) {
            for (int j = 0; j < 50; j++) {
                s += i * j;
            }
        }
        return s;
    }

    static int l5(int n, int s) {
        int[] d = DATA;
        int lim = Math.min(n, d.length);
        for (int i = 0; i < lim; i++) {
            s += d[i];
        }
        return s;
    }

    static int l6(int n, int s) {
        for (int i = n - 1; i >= 0; i--) {
            s = (s << 3) - s + i;
        }
        return s;
    }

    static int l7(int n, int s) {
        int i = 0;
        while (true) {
            if (i >= n) {
                break;
            }
            s = s + (i & 7);
            i += 1;
        }
        return s;
    }

    static int l8(int n, int s) {
        for (int i = 0; i < n; i += 2) {
            s ^= i * 0x9e37;
        }
        for (int i = 1; i < n; i += 2) {
            s += i;
        }
        return s;
    }

    static int l9(int n, int s) {
        int a = 1;
        int b = 1;
        for (int i = 0; i < n; i++) {
            int t = a + b;
            a = b;
            b = t;
        }
        return s + b;
    }

    static int l10(int n, int s) {
        int i = 0;
        do {
            if ((i & 1) == 0) {
                s += i;
            } else {
                s -= i >> 1;
            }
            i++;
        } while (i < n);
        return s;
    }

    static int l11(int n, int s) {
        for (int i = 0; i < n; i++) {
            s = Integer.rotateLeft(s, 1) ^ i;
        }
        return s;
    }

    static int l12(int n, int s) {
        int k = n;
        while (k > 0) {
            s += k % 7;
            k--;
        }
        return s;
    }

    static int l13(int n, int s) {
        for (int i = 0; i < n; i++) {
            s = s * 7 + (i < 100 ? 1 : 2);
        }
        return s;
    }

    static int l14(int n, int s) {
        int outer = n / 100;
        int i = 0;
        while (i < outer) {
            int j = 0;
            do {
                s ^= j + i;
                j++;
            } while (j < 100);
            i++;
        }
        return s;
    }

    static int l15(int n, int s) {
        short c = 0;
        for (int i = 0; i < n; i++) {
            c += (short) i;
            s += c;
        }
        return s;
    }

    static int all(int n, int s) {
        s = l0(n, s);
        s = l1(n, s);
        s = l2(n, s);
        s = l3(n, s);
        s = l4(n, s);
        s = l5(n, s);
        s = l6(n, s);
        s = l7(n, s);
        s = l8(n, s);
        s = l9(n, s);
        s = l10(n, s);
        s = l11(n, s);
        s = l12(n, s);
        s = l13(n, s);
        s = l14(n, s);
        s = l15(n, s);
        return s;
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 3;
        for (int i = 0; i < DATA.length; i++) {
            DATA[i] = i * 2654435 + 7;
        }
        // Back edges per call of `all`, roughly: sixteen loops of n iterations.
        double longEdges = (double) LONG * 16 * LONG_CALLS;
        double shortEdges = (double) SHORT * 16 * SHORT_CALLS;
        long check = 0;
        for (int r = 0; r < rounds; r++) {
            int s = r;
            long t0 = System.nanoTime();
            for (int c = 0; c < LONG_CALLS; c++) {
                s = all(LONG, s);
            }
            long t1 = System.nanoTime();
            for (int c = 0; c < SHORT_CALLS; c++) {
                s = all(SHORT, s);
            }
            long t2 = System.nanoTime();
            check = check * 31 + s;
            System.err.printf(
                    "round %d: long %.2f ns/backedge  short %.2f ns/backedge%n",
                    r, (t1 - t0) / longEdges, (t2 - t1) / shortEdges);
        }
        System.out.println("checksum=" + check);
    }
}
