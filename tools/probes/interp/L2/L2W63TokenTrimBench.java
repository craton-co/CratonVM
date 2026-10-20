// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 63, lane L2 -- bench probe for
// docs/internal/jit-proposals/i58-L2-proposal-the-live-model-skips-memory-token-reads-20261022-RETIRED-20261009.md.
//
// The optimizing tier reuses a VALUE node as the memory chain's producer (a
// volatile `getstatic`, an `instanceof`), so the next memory op names it in
// its token slot. With the default liveness model that ordering edge counts
// as a read and holds the value's interval (a register or a spill colour)
// across a whole inner loop. `CRATONVM_JIT_IR_LS_TOKEN_EDGES_NOT_USES=1`
// (JIT round 17) drops those reads; wave 63 adds the census that says how
// many intervals that switch actually moves:
// `CRATONVM_DBG_IR_LS_TOKEN_TRIM_CENSUS=1` prints, per optimizing compile,
//   [ir-ls] token-trim census: values=.. trimmed=.. (value-typed=.. memory=..)
//           positions=.. carried-dropped=.. header-live-in-dropped=..
//           peak-live=A->B examples=[nID Int lo..hi->lo..hi, ...]
// (`CRATONVM_DBG_IR_COMPILES=1` beside it names the method each line
// belongs to: the census line comes right after that method's compile
// starts lowering.)
//
// Rows (stderr, min ns per inner iteration over REPS calls; JIT on):
//   nested     `for (i) { if (stop) break; for (j) s += i + j; burnt++; }`
//              with `stop` volatile: the i58-L2 shape; the census should show
//              a value-typed trim with header-live-in-dropped >= 1;
//   typeCheck  an `instanceof` whose result orders the `hits++` after an
//              inner loop (`TypeCheckBench`'s reuse, inside a loop nest);
//   control    `nested` with no static field at all: census value-typed=0,
//              and its time must not move.
// A/B: `CRATONVM_JIT_IR_LS_TOKEN_EDGES_NOT_USES` unset vs `=1`, interleaved
// binaries, medians of reps (in-JVM timings on the Windows host swing ~3x).
// `CRATONVM_DBG_IR_LINEAR_SCAN=1` shows the OSR-entry refusals the switch
// should remove (the wave-58 excuse, `excused:memory-order-uses`, should stop
// firing for `nested` with the switch on: the value is no longer live there).
//
// stdout carries only checksums, identical on every VM and every switch:
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// nested checksum=2998000000 burnt=40000
// typeCheck checksum=495050000 hits=2000000
// control checksum=2998000000
// HOTSPOT_EXPECTED_END
public class L2W63TokenTrimBench {
    static final int REPS = 20;
    static final int OUTER = 2000;
    static final int INNER = 1000;
    static final int CHECKS = 100000;

    static volatile boolean stop;
    static long burnt;
    static int hits;
    static Object[] objs;

    static long nested(int n, int m) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            if (stop) {
                break;
            }
            for (int j = 0; j < m; j++) {
                s += i + j;
            }
            burnt++;
        }
        return s;
    }

    static long typeCheck(int n) {
        long s = 0;
        Object[] a = objs;
        for (int i = 0; i < n; i++) {
            if (a[i & 7] instanceof String) {
                s++;
            }
            for (int j = 0; j < 100; j++) {
                s += j;
            }
            hits++;
        }
        return s;
    }

    static long control(int n, int m) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            for (int j = 0; j < m; j++) {
                s += i + j;
            }
        }
        return s;
    }

    public static void main(String[] args) {
        objs = new Object[8];
        for (int k = 0; k < objs.length; k++) {
            objs[k] = (k & 1) == 0 ? (Object) ("s" + k) : (Object) Integer.valueOf(k);
        }

        long best = Long.MAX_VALUE;
        long sum = 0;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            sum = nested(OUTER, INNER);
            best = Math.min(best, System.nanoTime() - t0);
        }
        System.out.println("nested checksum=" + sum + " burnt=" + burnt);
        System.err.printf("nested     %.3f ns/op%n", best / (double) ((long) OUTER * INNER));

        best = Long.MAX_VALUE;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            sum = typeCheck(CHECKS);
            best = Math.min(best, System.nanoTime() - t0);
        }
        System.out.println("typeCheck checksum=" + sum + " hits=" + hits);
        System.err.printf("typeCheck  %.3f ns/op%n", best / (double) ((long) CHECKS * 100));

        best = Long.MAX_VALUE;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            sum = control(OUTER, INNER);
            best = Math.min(best, System.nanoTime() - t0);
        }
        System.out.println("control checksum=" + sum);
        System.err.printf("control    %.3f ns/op%n", best / (double) ((long) OUTER * INNER));
    }
}
