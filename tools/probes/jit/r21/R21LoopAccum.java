// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 1, lane loops: HOIST13-2, scalar promotion of a loop's
// accumulator cell (a static, a field of `this`, a field of an invariant box,
// an array element), with the controls that must NOT change any answer:
//
//   * an exception thrown mid-loop (explicit `throw` and an AIOOBE), after
//     which the cell must hold exactly the sum of the iterations that ran;
//   * a volatile accumulator (never promoted: every store is a release);
//   * a second thread spinning on the plain cell while the loop runs, joined
//     before the final value is printed (the join orders the exit write-back
//     before the read; the reader's intermediate sightings are not printed).
//
// Run:
//   javac -d out R21LoopAccum.java
//   java -Xmx256m -cp out R21LoopAccum                                   # HotSpot 25
//   cratonvm -Xmx256m -cp out R21LoopAccum                               # default (--jdk-only)
//   CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_METHOD_ENTRY=1 cratonvm -Xmx256m -cp out R21LoopAccum
//   CRATONVM_JIT_IR_LOOP_ARRAY_ELEMENT_PROMOTE=1 cratonvm -Xmx256m -cp out R21LoopAccum  # array-elem
//   CRATONVM_JIT_IR_LOOP_PROMOTE_SPLICED=1 cratonvm -Xmx256m -cp out R21LoopAccum   # spliced kernels
//     (the timed kernels are spliced into the drivers' lambdas; this arm promotes their loops:
//      `[ir] loop field promote: header bci N: 1 cell(s), stores sunk to the exit` with a
//      spliced pc for fieldThisInt/Long, `[ir] loop static promote: ...` for staticInt/Long)
//   cratonvm --compatible -Xmx256m -cp out R21LoopAccum                  # every arm again
//   cratonvm --nojit -Xmx256m -cp out R21LoopAccum                       # facts only
// Engagement (arm `=1`): CRATONVM_DBG_JITC=1 prints, for fieldThisInt / fieldThisLong,
//   `[ir] loop field promote: header bci N: 1 cell(s), stores sunk to the exit`, and
//   CRATONVM_DBG_LICM=1 prints `[ir] loop field promote (method entry): ...`.
//   With CRATONVM_JIT_IR_LOOP_ARRAY_ELEMENT_PROMOTE=1, arrayElem prints
//   `[ir] loop array element promote: header bci N: 1 element(s), stores sunk to the exit`.
//   staticInt is promoted on the default arm already
//   (`[ir] loop static promote: header bci N: 1 static cell(s), ...`).
//
// HotSpot expectation: every `fact` line below is identical on HotSpot 25, on
// every CratonVM arm and under --nojit (they are pure arithmetic on fixed
// inputs). Timing lines (`time <row> <N> ns/op`, one op = one call over
// N_ITER = 1000 iterations, median of 9 reps after warm-up): HotSpot keeps
// every non-volatile cell in a register (static / this / box rows ~0-400
// ns/op, often vectorised); the array-element row is HotSpot's too
// (it promotes `acc[1]` after a loop-entry range check); the volatile row
// pays a store per iteration on both VMs.

public class R21LoopAccum {
    static final int N_ITER = 1000;
    static final int WARM = 30000;
    static final int REPS = 9;
    static final int CALLS = 20000;

    static int sCount;
    static long sLong;
    static volatile int vCount;

    int fCount;
    long fLong;
    final Box box = new Box();
    final int[] acc = new int[4];

    static final class Box {
        int sum;
    }

    // ── kernels ──────────────────────────────────────────────────────────

    /** A static accumulator (promoted by CRATONVM_JIT_IR_LOOP_STATIC_PROMOTE). */
    static int staticInt(int n, int k) {
        for (int i = 0; i < n; i++) {
            sCount += i ^ k;
        }
        return sCount;
    }

    static long staticLong(int n, int k) {
        for (int i = 0; i < n; i++) {
            sLong += (long) i * k;
        }
        return sLong;
    }

    /** A field of the receiver (the new method-entry promotion). */
    int fieldThisInt(int n, int k) {
        for (int i = 0; i < n; i++) {
            fCount += i ^ k;
        }
        return fCount;
    }

    long fieldThisLong(int n, int k) {
        for (int i = 0; i < n; i++) {
            fLong += (long) i * k;
        }
        return fLong;
    }

    /** A field of an invariant object read from a final field (base not known non-null to the IR). */
    int fieldBox(int n, int k) {
        Box b = box;
        for (int i = 0; i < n; i++) {
            b.sum += i ^ k;
        }
        return b.sum;
    }

    /** An array element with an invariant index (LOOPS21-1, CRATONVM_JIT_IR_LOOP_ARRAY_ELEMENT_PROMOTE). */
    int arrayElem(int n, int k) {
        int[] a = acc;
        for (int i = 0; i < n; i++) {
            a[1] += i ^ k;
        }
        return a[1];
    }

    /** Volatile control: every store is a release; never promoted. */
    static int volatileInt(int n, int k) {
        for (int i = 0; i < n; i++) {
            vCount += i ^ k;
        }
        return vCount;
    }

    /** An explicit throw mid-loop: the cell must hold exactly what ran. */
    int throwMid(int n, int stop) {
        for (int i = 0; i < n; i++) {
            fCount += i + 1;
            if (i == stop) {
                throw new IllegalStateException("stop " + i);
            }
        }
        return fCount;
    }

    static int staticThrowMid(int n, int stop) {
        for (int i = 0; i < n; i++) {
            sCount += i + 1;
            if (i == stop) {
                throw new IllegalStateException("stop " + i);
            }
        }
        return sCount;
    }

    /** An implicit AIOOBE mid-loop: the add before the failing read ran, the one after did not. */
    int aioobeMid(int[] data, int n) {
        for (int i = 0; i < n; i++) {
            fCount += 1;
            fCount += data[i];
        }
        return fCount;
    }

    // ── driver ───────────────────────────────────────────────────────────

    interface Op {
        long run();
    }

    static long sink;

    static void time(String row, Op op) {
        for (int w = 0; w < WARM; w++) {
            sink += op.run();
        }
        long[] reps = new long[REPS];
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            for (int c = 0; c < CALLS; c++) {
                sink += op.run();
            }
            reps[r] = (System.nanoTime() - t0) / CALLS;
        }
        java.util.Arrays.sort(reps);
        System.out.println("time " + row + " " + reps[REPS / 2] + " ns/op");
    }

    static void fact(String row, Object v) {
        System.out.println("fact " + row + " " + v);
    }

    public static void main(String[] args) throws Exception {
        R21LoopAccum o = new R21LoopAccum();

        // Facts first, cold, then again after warm-up (the compiled bodies).
        for (int pass = 0; pass < 2; pass++) {
            String p = pass == 0 ? "cold-" : "warm-";
            if (pass == 1) {
                for (int w = 0; w < WARM; w++) {
                    sCount = 0;
                    sLong = 0;
                    vCount = 0;
                    o.fCount = 0;
                    o.fLong = 0;
                    o.box.sum = 0;
                    o.acc[1] = 0;
                    staticInt(64, w);
                    staticLong(64, w);
                    volatileInt(64, w);
                    o.fieldThisInt(64, w);
                    o.fieldThisLong(64, w);
                    o.fieldBox(64, w);
                    o.arrayElem(64, w);
                    try {
                        o.throwMid(64, 70);
                        staticThrowMid(64, 70);
                        o.aioobeMid(new int[64], 64);
                    } catch (RuntimeException e) {
                        throw new AssertionError(e);
                    }
                }
            }
            sCount = 0;
            sLong = 0;
            vCount = 0;
            o.fCount = 0;
            o.fLong = 0;
            o.box.sum = 0;
            o.acc[1] = 0;
            fact(p + "static-int", staticInt(N_ITER, 0x55));
            fact(p + "static-long", staticLong(N_ITER, 7));
            fact(p + "field-this-int", o.fieldThisInt(N_ITER, 0x55));
            fact(p + "field-this-long", o.fieldThisLong(N_ITER, 7));
            fact(p + "field-box", o.fieldBox(N_ITER, 0x55));
            fact(p + "array-elem", o.arrayElem(N_ITER, 0x55));
            fact(p + "volatile-int", volatileInt(N_ITER, 0x55));
            // Zero-trip: the cells keep what they held (no write-back of a stale value).
            fact(p + "zero-trip-this", o.fieldThisInt(0, 1) + " " + o.fCount);
            fact(p + "zero-trip-static", staticInt(0, 1) + " " + sCount);

            o.fCount = 0;
            try {
                o.throwMid(N_ITER, 499);
                fact(p + "throw-mid", "no-throw");
            } catch (IllegalStateException e) {
                fact(p + "throw-mid", e.getMessage() + " fCount=" + o.fCount);
            }
            sCount = 0;
            try {
                staticThrowMid(N_ITER, 333);
                fact(p + "static-throw-mid", "no-throw");
            } catch (IllegalStateException e) {
                fact(p + "static-throw-mid", e.getMessage() + " sCount=" + sCount);
            }
            o.fCount = 0;
            int[] data = new int[100];
            for (int i = 0; i < data.length; i++) {
                data[i] = i * 3;
            }
            try {
                o.aioobeMid(data, 150);
                fact(p + "aioobe-mid", "no-throw");
            } catch (ArrayIndexOutOfBoundsException e) {
                fact(p + "aioobe-mid", e.getMessage() + " fCount=" + o.fCount);
            }
        }

        // A second thread spins on the plain cell while the loop runs; the
        // join orders the exit write-back before the final read.
        final R21LoopAccum shared = new R21LoopAccum();
        final boolean[] stop = new boolean[1];
        Thread reader = new Thread(() -> {
            long seen = 0;
            while (!readStop(stop)) {
                seen += shared.fCount;
            }
            sink += seen;
        });
        reader.setDaemon(true);
        reader.start();
        long expect = 0;
        for (int r = 0; r < 2000; r++) {
            shared.fieldThisInt(N_ITER, r);
            for (int i = 0; i < N_ITER; i++) {
                expect += i ^ r;
            }
        }
        setStop(stop);
        reader.join();
        fact("reader-final", shared.fCount + " expect=" + (int) expect);

        // Timing.
        time("static-int", () -> staticInt(N_ITER, 0x55));
        time("static-long", () -> staticLong(N_ITER, 7));
        time("field-this-int", () -> o.fieldThisInt(N_ITER, 0x55));
        time("field-this-long", () -> o.fieldThisLong(N_ITER, 7));
        time("field-box", () -> o.fieldBox(N_ITER, 0x55));
        time("array-elem", () -> o.arrayElem(N_ITER, 0x55));
        time("volatile-int", () -> volatileInt(N_ITER, 0x55));
        System.out.println("done");
    }

    static synchronized boolean readStop(boolean[] s) {
        return s[0];
    }

    static synchronized void setStop(boolean[] s) {
        s[0] = true;
    }
}
