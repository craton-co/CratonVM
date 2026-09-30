// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 29, lane L7: stage 1 of the contiguous
// interpreter stack (docs/known-issues/interpreter/
// i1-L4-proposal-contiguous-interpreter-stack-20260923.md). A frame built by
// the cached install paths (the fast doors and the general dispatchers) takes
// its locals and operand stack from ONE window of its FrameStack's slot slab
// (vm/src/runtime/slot_slab.rs) instead of four pooled Vecs.
// CRATONVM_JIT_NO_LOCALS_SLAB=1 restores the pooled buffers in the same
// binary, so the A/B is one build run twice.
//
// Rows (ns per interpreted call, stderr), --nojit:
//
//   rec-d1000   f(n) = n == 0 ? 0 : 1 + f(n - 1), depth 1000, 10^4 reps: the
//               proposal's workload (a). Every call below the first rep
//               rebuilds a retired slot.
//   rec-d10     the same at depth 10, 10^6 reps (depth sensitivity).
//   leaf-g      g(a, b) = a + b, 10^7 calls: workload (b), tiny locals.
//   mixed       10^7 calls cycling h(long, double, int), k(double, long),
//               z() and w(Object, int): category-2 argument slots and
//               arities 0-4.
//   virtual     an instance method v(int), 10^7 calls (the virtual door).
//   deep-d6000  f at depth 6000, 1700 reps, on a thread with a 256 MB stack:
//               the slab crosses several chunks (a window is 1 + 24 slots).
//
// Expected with the slab on vs CRATONVM_JIT_NO_LOCALS_SLAB=1: flat to a few
// percent down on every row (a retired slot's rebuild writes one window
// instead of clearing, reserving and resizing two Vecs, and skips the
// ValueStack's take/put of its two buffers); deep-d6000 and rec-d1000 the
// same shape. Anything slower beyond the host floor is a regression of the
// per-call path and must be reported.
//
// Positive control: CRATONVM_DBG_INVOKE_PHASES=1 prints at exit
//   [invoke-phases] slot slab: windowed=W of retired=R (...)
// with W close to R (every cached frame; R also counts the few by-value
// frames) when the slab is on, and W=0 with CRATONVM_JIT_NO_LOCALS_SLAB=1; and
//   [invoke-phases] frame lifecycle: installs=I install_cyc=.. pops=P pop_cyc=..
// the per-install and per-pop cycles of EVERY cached frame (the fast doors
// included), for the A/B.
//
// Run: cratonvm --nojit -cp <dir> L7W29ContiguousStackBench
//      CRATONVM_JIT_NO_LOCALS_SLAB=1 cratonvm --nojit -cp <dir> L7W29ContiguousStackBench
//      on fat-LTO builds, interleaved, pinned, 5 rounds, medians.
//
// stdout is deterministic, identical on HotSpot 25 (25.0.3), with and without
// -Xint. For scale, HotSpot 25 -Xint on the i7-8550U box (ns/call): rec-d1000
// 65.8, rec-d10 39.0, leaf-g 45.4, mixed 132.1, virtual 120.2, deep-d6000
// 40.7.
//   rec-d1000 10000000
//   rec-d10 10000000
//   leaf-g 50000005000000
//   mixed 59375001250000
//   virtual 50000005000000
//   deep-d6000 10200000
public class L7W29ContiguousStackBench {
    static final int WARMUP_DIV = 20;

    static int f(int n) {
        return n == 0 ? 0 : 1 + f(n - 1);
    }

    static int g(int a, int b) {
        return a + b;
    }

    static long h(long a, double b, int c) {
        return a + (long) b + c;
    }

    static long k(double a, long b) {
        return (long) a + b;
    }

    static int z() {
        return 1;
    }

    static int w(Object o, int x) {
        return o == null ? x : x + 1;
    }

    int bias;

    int v(int a) {
        return a + bias;
    }

    interface Row {
        long run(int reps);
    }

    static long recurse(int depth, int reps) {
        long s = 0;
        for (int r = 0; r < reps; r++) {
            s += f(depth);
        }
        return s;
    }

    static long leaf(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += g(i, 1);
        }
        return s;
    }

    static long mixed(int n) {
        long s = 0;
        Object o = new Object();
        for (int i = 0; i < n; i++) {
            switch (i & 3) {
                case 0 -> s += h(i, 0.5 * i, 3);
                case 1 -> s += k(1.25 * i, i);
                case 2 -> s += z();
                default -> s += w(o, i);
            }
        }
        return s;
    }

    static long virtualCalls(int n) {
        L7W29ContiguousStackBench b = new L7W29ContiguousStackBench();
        b.bias = 1;
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += b.v(i);
        }
        return s;
    }

    static void row(String name, long calls, int reps, Row r) {
        r.run(Math.max(1, reps / WARMUP_DIV));
        long t0 = System.nanoTime();
        long sum = r.run(reps);
        long ns = System.nanoTime() - t0;
        System.out.println(name + " " + sum);
        System.err.printf(java.util.Locale.ROOT, "%-12s %8.1f ns/call%n", name, (double) ns / calls);
    }

    public static void main(String[] args) throws Exception {
        row("rec-d1000", 10_000L * 1001, 10_000, reps -> recurse(1000, reps));
        row("rec-d10", 1_000_000L * 11, 1_000_000, reps -> recurse(10, reps));
        row("leaf-g", 10_000_000L, 10_000_000, L7W29ContiguousStackBench::leaf);
        row("mixed", 10_000_000L, 10_000_000, L7W29ContiguousStackBench::mixed);
        row("virtual", 10_000_000L, 10_000_000, L7W29ContiguousStackBench::virtualCalls);
        Thread deep = new Thread(null,
                () -> row("deep-d6000", 1_700L * 6001, 1_700, reps -> recurse(6000, reps)),
                "deep", 256L << 20);
        deep.start();
        deep.join();
    }
}
