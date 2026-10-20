// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Adversarial cover for the VM-side offload runtime.
//
// `vm/src/runtime/offload.rs` has 22 unit tests and every one of them is
// a no-device or error path — skips without a driver, unknown stream
// handles, unregistered submission handles. Not one dispatches a kernel,
// because none of it can run without a device. The happy path —
// marshalling, the input-residency cache, the read-only-input write
// suppression, the chunked writeback, the bounds deopt, the write-back
// into the Java heap — is covered by five end-to-end checks in
// `bench-gpu/ci-gate.sh` and nothing else.
//
// These are the cases those five do not reach. Each scenario prints one
// checksum; the runner compares `--gpu` against `--nojit` on the same
// binary (the control) and against HotSpot (the oracle).
//
// Every array is at least 1<<16 elements so the dispatch clears
// `--gpu-min-work` and is really offloaded rather than quietly falling
// back to the interpreter and comparing the interpreter with itself.
//
// Usage: java GpuRuntimeStress [scenario] [n]
//   0 all   1 concurrent   2 cache-coherence   3 readonly-inputs
//   4 aliasing   5 repeat-submit   6 deopt-after-success
//   7 bulk-writes (System.arraycopy / Arrays.fill)
public class GpuRuntimeStress {

    // ── the offloaded kernels ────────────────────────────────────────
    // Void, static, primitive arrays, one canonical counted loop: the
    // shape `analyzer::analyze` admits and `try_dispatch` will take.

    static void scale(int[] in, int[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = in[i] * 3 + 7;
        }
    }

    static void combine(int[] a, int[] b, int[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = a[i] * 2 + b[i];
        }
    }

    // Reads `src` and writes `dst`; used to check that a read-only input
    // is not written back over.
    static void copyShift(int[] src, int[] dst) {
        for (int i = 0; i < dst.length; i++) {
            dst[i] = src[i] + 11;
        }
    }

    static long mix(long h, long v) {
        return h * 1000003L + v;
    }

    static long sum(int[] v) {
        long h = 0;
        for (int x : v) h = mix(h, x);
        return h;
    }

    // ── 1. concurrent dispatch from several Java threads ─────────────
    //
    // `OffloadCacheRegistry` hands one `OffloadCache` — one CUDA
    // context, one kernel map, one input-residency cache, one dispatch
    // memo, one submission table, one chunk-stream pool — to every
    // dispatching thread. Nothing in the unit tests puts two threads
    // through it at once.
    static long concurrent(int n, int threads) throws Exception {
        final long[] results = new long[threads];
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                int[] in = new int[n];
                int[] out = new int[n];
                for (int i = 0; i < n; i++) in[i] = id * 31 + (i % 997);
                long h = 0;
                for (int round = 0; round < 8; round++) {
                    scale(in, out);
                    h = mix(h, sum(out));
                    // Mutate so each round is distinguishable and the
                    // residency cache has to keep up.
                    for (int i = 0; i < n; i += 4096) in[i] += 1;
                }
                results[id] = h;
            });
        }
        for (Thread t : ts) t.start();
        for (Thread t : ts) t.join();
        long h = 0;
        for (long r : results) h = mix(h, r);
        return h;
    }

    // ── 2. residency-cache coherence across a host write ─────────────
    //
    // The input-residency cache mirrors a Java array in device memory
    // across submits. Every host write must evict it — the interpreter's
    // `*astore` arms call `input_cache::invalidate`. If one is missed,
    // the second submit computes from the stale device copy.
    static long cacheCoherence(int n) {
        int[] in = new int[n];
        int[] out = new int[n];
        for (int i = 0; i < n; i++) in[i] = i % 1013;
        long h = 0;
        for (int round = 0; round < 12; round++) {
            scale(in, out);
            h = mix(h, sum(out));
            // Host writes BETWEEN submits, of every shape the cache
            // must notice: a single element, a strided sweep, and a
            // whole-array rewrite.
            in[round] = 999_000 + round;
            for (int i = round; i < n; i += 1024) in[i] = i ^ round;
            if (round == 6) {
                for (int i = 0; i < n; i++) in[i] = (i * 7 + round) % 4099;
            }
        }
        return h;
    }

    // ── 3. read-only inputs must not be written back over ────────────
    //
    // `writes_param_mask` suppresses the post-launch D->H copy for an
    // array the kernel never stores to. If the mask is wrong in the
    // permissive direction a read-only input gets clobbered by whatever
    // the device buffer held; in the strict direction the output is not
    // written back at all.
    static long readonlyInputs(int n) {
        int[] a = new int[n];
        int[] b = new int[n];
        int[] out = new int[n];
        for (int i = 0; i < n; i++) {
            a[i] = i % 251;
            b[i] = (i * 3) % 509;
        }
        long h = 0;
        for (int round = 0; round < 6; round++) {
            combine(a, b, out);
            // Both inputs must be untouched, every round.
            h = mix(h, sum(a));
            h = mix(h, sum(b));
            h = mix(h, sum(out));
        }
        return h;
    }

    // ── 4. the same array as two arguments ───────────────────────────
    //
    // `combine(a, a, out)` passes one Java array in two parameter slots.
    // The marshaller keys its residency cache by `ObjectRef`, so both
    // slots resolve to the same device buffer — and `writes_param_mask`
    // is per-parameter. A kernel that reads a slot it also writes is the
    // case the chunked writeback explicitly refuses; this is the
    // read-only twin of it.
    static long aliasing(int n) {
        int[] a = new int[n];
        int[] out = new int[n];
        for (int i = 0; i < n; i++) a[i] = i % 373;
        long h = 0;
        for (int round = 0; round < 6; round++) {
            combine(a, a, out);
            h = mix(h, sum(out));
            h = mix(h, sum(a));
            a[round * 3] += 5;
        }
        return h;
    }

    // ── 5. many submits of one unchanged array ───────────────────────
    //
    // The residency cache's whole reason to exist. Nothing is mutated,
    // so every submit after the first should reuse the device buffer —
    // and every submit must still produce the same answer.
    static long repeatSubmit(int n) {
        int[] in = new int[n];
        int[] out = new int[n];
        for (int i = 0; i < n; i++) in[i] = i % 617;
        long h = 0;
        for (int round = 0; round < 32; round++) {
            copyShift(in, out);
            h = mix(h, sum(out));
        }
        return h;
    }

    // ── 6. a deopt after successful submits, then more submits ───────
    //
    // A bounds failure makes the device raise the failure flag and the
    // host re-run on the CPU. The interesting part is what happens to
    // the caches afterwards: the kernel must not be blacklisted for a
    // DATA failure, and the residency entries must still be coherent.
    //
    // `out` is deliberately SHORTER than `in`, so the loop over
    // `in.length` runs past the end of `out` and every element from
    // `out.length` on is out of bounds.
    static void overrun(int[] in, int[] out) {
        for (int i = 0; i < in.length; i++) {
            out[i] = in[i] + 1;
        }
    }

    static long deoptThenContinue(int n) {
        int[] in = new int[n];
        int[] out = new int[n];
        for (int i = 0; i < n; i++) in[i] = i % 811;
        long h = 0;

        // Two clean submits first, so the kernel is compiled and cached.
        scale(in, out);
        h = mix(h, sum(out));
        scale(in, out);
        h = mix(h, sum(out));

        // Now one that must throw.
        int[] shortOut = new int[n / 2];
        boolean threw = false;
        try {
            overrun(in, shortOut);
        } catch (ArrayIndexOutOfBoundsException e) {
            threw = true;
        }
        h = mix(h, threw ? 1 : 0);
        // Whatever the device wrote before failing, the elements that
        // WERE in bounds must hold the right answer after the CPU re-run.
        h = mix(h, sum(shortOut));

        // And the cache must still be usable afterwards.
        for (int round = 0; round < 4; round++) {
            scale(in, out);
            h = mix(h, sum(out));
            in[round] += 3;
        }
        return h;
    }

    // ── 7. bulk host writes: System.arraycopy and Arrays.fill ────────
    //
    // A plain `iastore` is not the only way the host writes an array.
    // `System.arraycopy` is a native intrinsic in another crate and
    // `Arrays.fill` may be intrinsified too; neither goes through an
    // interpreter astore arm. If the residency cache is not told, the
    // next submit computes from the copy it still holds.
    static long bulkWrites(int n) {
        int[] in = new int[n];
        int[] src = new int[n];
        int[] out = new int[n];
        for (int i = 0; i < n; i++) {
            in[i] = i % 271;
            src[i] = (i * 5) % 683;
        }
        long h = 0;
        for (int round = 0; round < 8; round++) {
            scale(in, out);
            h = mix(h, sum(out));
            // Bulk rewrite of the input between submits, with no astore
            // anywhere in sight.
            if ((round & 1) == 0) {
                System.arraycopy(src, 0, in, 0, n);
            } else {
                java.util.Arrays.fill(in, 40_000 + round);
            }
        }
        return h;
    }

    public static void main(String[] args) throws Exception {
        int which = args.length > 0 ? Integer.parseInt(args[0]) : 0;
        int n = args.length > 1 ? Integer.parseInt(args[1]) : (1 << 16);

        if (which == 0 || which == 1) {
            System.out.println("concurrent=" + concurrent(n, 4));
        }
        if (which == 0 || which == 2) {
            System.out.println("cache_coherence=" + cacheCoherence(n));
        }
        if (which == 0 || which == 3) {
            System.out.println("readonly_inputs=" + readonlyInputs(n));
        }
        if (which == 0 || which == 4) {
            System.out.println("aliasing=" + aliasing(n));
        }
        if (which == 0 || which == 5) {
            System.out.println("repeat_submit=" + repeatSubmit(n));
        }
        if (which == 0 || which == 6) {
            System.out.println("deopt_then_continue=" + deoptThenContinue(n));
        }
        if (which == 0 || which == 7) {
            System.out.println("bulk_writes=" + bulkWrites(n));
        }
        System.out.println("STRESS_DONE n=" + n);
    }
}
