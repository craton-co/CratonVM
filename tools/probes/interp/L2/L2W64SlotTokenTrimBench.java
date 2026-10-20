// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 64, lane L2 -- bench probe for
// docs/internal/fixed-bugs/interpreter-i63-L2-proposal-frame-home-ranges-skip-memory-token-reads-KEPT-OFF-20261104.md
// and its safety net
// docs/internal/jit-proposals/i63-L2-proposal-the-lowering-refuses-a-token-slot-operand-read-20261027-RETIRED-20261009.md.
//
// A value node reused as the memory chain's producer (a volatile `getstatic`,
// an `instanceof` result, a call result) is named by the next memory op in
// its token slot. `ir_lower::plan_slots` counted that ordering edge as a read
// and held the value's FRAME HOME across the whole inner loop.
// `CRATONVM_JIT_IR_SLOTS_TOKEN_EDGES_NOT_USES=1` (wave 64) drops those reads
// from the home ranges; it implies the allocator's
// `CRATONVM_JIT_IR_LS_TOKEN_EDGES_NOT_USES` rule and the token-read guard
// `CRATONVM_JIT_IR_LOWER_TOKEN_READ_GUARD`, which refuses any compile whose
// lowering reads an operand through a token slot.
//
// Census (stderr), with `CRATONVM_DBG_IR_LS_TOKEN_TRIM_CENSUS=1`, one pair of
// lines per optimizing compile (`CRATONVM_DBG_IR_COMPILES=1` names the method):
//   [ir-ls] token-trim census: values=.. trimmed=.. ... examples=[..]
//           slot-words=A->B ref-words=C->D homes-trimmed=N
//   [ir-ls] token-read guard: nodes=.. refusals=R pinned-hits=..
// (the guard line only while the guard is on). The orchestrator reads:
//   * slot-words A->B: frame words without/with the slot rule (the
//     proposal's measure; B < A on `nested` / `callToken` is the win,
//     A == B everywhere on CratonBench closes the proposal);
//   * refusals R: must be 0 on every compile; any non-zero line is a
//     wrong-code bug in the trimmed model (an arm reads through a token)
//     and is filed as a defect. A refusal also prints
//     `[ir-ls] token-read guard: nN <op> reads nM through token slot K (at file:line)`.
//
// Rows (stderr, min ns per inner iteration over REPS calls; JIT on):
//   nested     `for (i) { if (stop) break; for (j) s += i + j; burnt++; }`,
//              `stop` volatile: the i58-L2 / i63-L2 shape;
//   typeCheck  an `instanceof` whose result orders `hits++` after an inner
//              loop;
//   callToken  a call's result read BEFORE an inner loop, and the call the
//              token of `counter++` after it;
//   control    `nested` with no static field: slot-words must not move and
//              neither must its time.
// A/B: `CRATONVM_JIT_IR_SLOTS_TOKEN_EDGES_NOT_USES` unset vs `=1`, one binary,
// interleaved runs, medians of reps (in-JVM timings swing ~3x on Windows).
//
// stdout carries only checksums, identical on every VM and every switch:
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// nested checksum=2998000000 burnt=40000
// typeCheck checksum=495050000 hits=2000000
// callToken checksum=15494950000 counter=2000000
// control checksum=2998000000
// HOTSPOT_EXPECTED_END
public class L2W64SlotTokenTrimBench {
    static final int REPS = 20;
    static final int OUTER = 2000;
    static final int INNER = 1000;
    static final int CHECKS = 100000;

    static volatile boolean stop;
    static long burnt;
    static int hits;
    static int counter;
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

    static int next(int i) {
        return i * 3 + 1;
    }

    static long callToken(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            int v = next(i);
            s += v;
            for (int j = 0; j < 100; j++) {
                s += j;
            }
            counter++;
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
            sum = callToken(CHECKS);
            best = Math.min(best, System.nanoTime() - t0);
        }
        System.out.println("callToken checksum=" + sum + " counter=" + counter);
        System.err.printf("callToken  %.3f ns/op%n", best / (double) ((long) CHECKS * 100));

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
