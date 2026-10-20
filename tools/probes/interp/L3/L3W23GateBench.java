// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 23, lane L3: timing probe for the dispatch loop's
// fast-path gate, which since wave 23 also compares the frame stack's
// `code_moves` word on every bytecode (so a frame moved onto a replaced
// body's copy at a freed callee copy's address does not keep the callee's
// verdict) and marks a moved frame's `ReplacedBody` at every gate refresh.
// Nothing here is redefined: it prices the cost every program pays.
//
// Rows to compare, interleaved builds (dev 0c410b878 vs this branch), run
// with --nojit so the interpreter executes every bytecode, medians of 3:
//     arith ms   -- straight-line arithmetic in one frame (the per-bytecode
//                   compare; expected within noise, at most ~1-2% slower)
//     calls ms   -- a small static callee called in a loop (a gate refresh
//                   per call and per return; expected within noise)
//     recurse ms -- self-recursion (same code, a new frame each call: no
//                   refresh before or after; expected within noise)
// The `sum=` line (stdout) is deterministic and must match HotSpot 25's:
//     sum.arith=2135650843 sum.calls=200000010000000 sum.recurse=12060000
// The timings go to stderr.
//
//     cratonvm --java-home <jdk25> --nojit -cp <dir> L3W23GateBench
public class L3W23GateBench {
    static int arith(int n) {
        int a = 1, b = 2, c = 3;
        for (int i = 0; i < n; i++) {
            a = a * 31 + b;
            b = b ^ (a >>> 3);
            c = c + a - b;
        }
        return a + b + c;
    }

    static long leaf(long x) {
        return x + 1;
    }

    static long calls(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += leaf(i);
        }
        return s;
    }

    static int depth(int d) {
        return d == 0 ? 1 : 1 + depth(d - 1);
    }

    static long recurse(int rounds) {
        long s = 0;
        for (int i = 0; i < rounds; i++) {
            s += depth(200);
        }
        return s;
    }

    public static void main(String[] args) {
        int scale = args.length > 0 ? Integer.parseInt(args[0]) : 1;
        long t0 = System.nanoTime();
        int a = arith(20_000_000 * scale);
        long t1 = System.nanoTime();
        long c = calls(20_000_000 * scale);
        long t2 = System.nanoTime();
        long r = recurse(60_000 * scale);
        long t3 = System.nanoTime();
        if (scale == 1) {
            System.out.println("sum.arith=" + a + " sum.calls=" + c + " sum.recurse=" + r);
        }
        System.err.println("arith ms " + (t1 - t0) / 1_000_000);
        System.err.println("calls ms " + (t2 - t1) / 1_000_000);
        System.err.println("recurse ms " + (t3 - t2) / 1_000_000);
    }
}
