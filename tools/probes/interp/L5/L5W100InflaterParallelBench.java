// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 100, lane L5 -- bench for
// `CRATONVM_INFLATER_SHARDED_TABLE` (default off;
// `docs/internal/fixed-bugs/interpreter-i73-L5-proposal-a-sharded-inflater-table-DONE-20261019.md`).
//
// Each thread owns its own `Inflater` and inflates the same 4 KiB class-sized
// raw-deflate stream (the shape of a jar entry) over and over. With one
// process-wide inflater table lock held across the zlib step, the 4-thread row
// costs about 4x the 1-thread row per thread-op; with the sharded table each
// thread's stream is in its own shard and the rows should scale.
//
// Run (interleave, five repetitions, medians; with and without --nojit):
//   javac -d out L5W100InflaterParallelBench.java
//   env CRATONVM_NONE=1                    cratonvm --java-home <jdk25> -cp out L5W100InflaterParallelBench
//   env CRATONVM_INFLATER_SHARDED_TABLE=1  cratonvm --java-home <jdk25> -cp out L5W100InflaterParallelBench
//   java -cp out L5W100InflaterParallelBench          (HotSpot reference)
// Compare the `TIME threads-4` line with `TIME threads-1` (ns per inflate per
// thread). Every other line is deterministic; HotSpot 25 prints:
//   input=4096 compressed-ok=true
//   threads-1 ok=true
//   threads-4 ok=true
//   threads-8 ok=true

import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.zip.Deflater;
import java.util.zip.Inflater;

public class L5W100InflaterParallelBench {
    static final int OPS = 20_000;

    static byte[] input() {
        byte[] b = new byte[4096];
        long x = 0x9E3779B97F4A7C15L;
        for (int i = 0; i < b.length; i++) {
            x ^= x << 13;
            x ^= x >>> 7;
            x ^= x << 17;
            // Class-file-like: mostly small values, some repetition.
            b[i] = (byte) ((i % 64 < 40) ? (x & 0x1F) : (i & 0xFF));
        }
        return b;
    }

    static byte[] deflate(byte[] in) {
        Deflater d = new Deflater(Deflater.DEFAULT_COMPRESSION, true);
        d.setInput(in);
        d.finish();
        byte[] buf = new byte[in.length * 2 + 64];
        int n = d.deflate(buf);
        d.end();
        byte[] out = new byte[n];
        System.arraycopy(buf, 0, out, 0, n);
        return out;
    }

    /** One thread's loop: returns false on any mismatch. */
    static boolean loop(byte[] compressed, byte[] expect, int ops) throws Exception {
        Inflater inf = new Inflater(true);
        byte[] out = new byte[expect.length];
        boolean ok = true;
        try {
            for (int i = 0; i < ops; i++) {
                inf.reset();
                inf.setInput(compressed);
                int n = 0;
                while (n < out.length && !inf.finished()) {
                    int got = inf.inflate(out, n, out.length - n);
                    if (got == 0 && (inf.needsInput() || inf.needsDictionary())) {
                        break;
                    }
                    n += got;
                }
                // A cheap check that does not dominate the inflate.
                if (n != expect.length || out[0] != expect[0] || out[n - 1] != expect[n - 1]
                        || out[n / 2] != expect[n / 2]) {
                    ok = false;
                }
            }
        } finally {
            inf.end();
        }
        return ok;
    }

    static void row(int threads, byte[] compressed, byte[] expect) throws Exception {
        AtomicBoolean ok = new AtomicBoolean(true);
        CountDownLatch start = new CountDownLatch(1);
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            ts[t] = new Thread(() -> {
                try {
                    start.await();
                    if (!loop(compressed, expect, OPS)) {
                        ok.set(false);
                    }
                } catch (Exception e) {
                    ok.set(false);
                }
            });
            ts[t].start();
        }
        long t0 = System.nanoTime();
        start.countDown();
        for (Thread t : ts) {
            t.join();
        }
        long ns = System.nanoTime() - t0;
        System.out.println("threads-" + threads + " ok=" + ok.get());
        System.out.println("TIME threads-" + threads + " ns/op-per-thread=" + (ns / OPS)
                + " total-ms=" + (ns / 1_000_000));
    }

    public static void main(String[] args) throws Exception {
        byte[] in = input();
        byte[] compressed = deflate(in);
        // Warm-up (also proves the stream round-trips).
        boolean warm = loop(compressed, in, 2_000);
        System.out.println("input=" + in.length + " compressed-ok=" + warm);
        row(1, compressed, in);
        row(4, compressed, in);
        row(8, compressed, in);
    }
}
