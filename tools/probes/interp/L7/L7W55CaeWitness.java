// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 55, lane L7: the witness of a failed
// `VarHandle.compareAndExchange` is a value the variable held, never
// `expected` unless the exchange happened
// (docs/internal/fixed-bugs/interpreter-L7-a-failed-varhandle-compare-and-exchange-re-reads-its-witness-FIXED-20261019.md).
//
// Thread B exchanges E -> N ITER times and counts the calls whose witness is
// E (claimed exchanges). Thread A turns every N it finds into X (counting
// it) and every X back into E, until B is done, then drains a last N. Each
// real exchange writes one N, which only A removes, so the two counts agree
// exactly. The native used to answer a failed CAS with a SEPARATE re-read:
// if A stored E between the two, B saw E although it wrote nothing, and
// `claims` exceeds `observed` (a lost update in a `while (cae != e)` loop).
//
// Rows: a reference field and an `Object[]` element (always the native), a
// `short` field (the native), an `int` field (the interpreter's route-cache
// exchange arm `CRATONVM_VH_INTERP_CAE_FAST`; with `=0`, the native). Each
// row runs cold (r0) and again (r1, r2). Only verdicts are printed.
//
// RACY: a pass on one run proves little. The orchestrator runs it 10 times,
// `--nojit` with `CRATONVM_VH_INTERP_CAE_FAST=0` (every row on the native)
// and default, on the base and the fix: the base is expected to print
// `false` for at least one row in some runs, the fix never.
//
// Expected (HotSpot 25.0.3, `java L7W55CaeWitness`, and `-Xint`):
//   r0 ref true true
//   r0 array true true
//   r0 short true true
//   r0 int true true
//   r1 ref true true
//   r1 array true true
//   r1 short true true
//   r1 int true true
//   r2 ref true true
//   r2 array true true
//   r2 short true true
//   r2 int true true
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

public class L7W55CaeWitness {
    static final class H {
        Object r;
        short s;
        int i;
    }

    static final VarHandle REF;
    static final VarHandle SHORT;
    static final VarHandle INT;
    static final VarHandle ELEM = MethodHandles.arrayElementVarHandle(Object[].class);

    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            REF = l.findVarHandle(H.class, "r", Object.class);
            SHORT = l.findVarHandle(H.class, "s", short.class);
            INT = l.findVarHandle(H.class, "i", int.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static final int ITER = 100_000;
    static final Object E = new Object();
    static final Object X = new Object();
    static final Object N = new Object();

    static volatile boolean done;

    interface Step {
        /** One consumer step: N -> X counted, X -> E. Returns 1 if an N was taken. */
        long take();
    }

    interface Exchange {
        /** One exchange E -> N; true when the witness is E. */
        boolean claim();
    }

    static String verdict(Step a, Exchange b) throws InterruptedException {
        done = false;
        long[] observed = new long[1];
        Thread consumer = new Thread(() -> {
            long n = 0;
            while (!done) {
                n += a.take();
            }
            n += a.take();
            observed[0] = n;
        });
        consumer.start();
        long claims = 0;
        for (int k = 0; k < ITER; k++) {
            if (b.claim()) {
                claims++;
            }
        }
        done = true;
        consumer.join();
        return (claims == observed[0]) + " " + (claims > 0);
    }

    static String refRow() throws InterruptedException {
        H h = new H();
        h.r = E;
        return verdict(
                () -> {
                    long n = REF.compareAndSet(h, N, X) ? 1 : 0;
                    REF.compareAndSet(h, X, E);
                    return n;
                },
                () -> REF.compareAndExchange(h, E, N) == E);
    }

    static String arrayRow() throws InterruptedException {
        Object[] a = {E};
        return verdict(
                () -> {
                    long n = ELEM.compareAndSet(a, 0, N, X) ? 1 : 0;
                    ELEM.compareAndSet(a, 0, X, E);
                    return n;
                },
                () -> ELEM.compareAndExchange(a, 0, E, N) == E);
    }

    static String shortRow() throws InterruptedException {
        H h = new H();
        h.s = 1;
        return verdict(
                () -> {
                    long n = SHORT.compareAndSet(h, (short) 3, (short) 2) ? 1 : 0;
                    SHORT.compareAndSet(h, (short) 2, (short) 1);
                    return n;
                },
                () -> (short) SHORT.compareAndExchange(h, (short) 1, (short) 3) == 1);
    }

    static String intRow() throws InterruptedException {
        H h = new H();
        h.i = 1;
        return verdict(
                () -> {
                    long n = INT.compareAndSet(h, 3, 2) ? 1 : 0;
                    INT.compareAndSet(h, 2, 1);
                    return n;
                },
                () -> (int) INT.compareAndExchange(h, 1, 3) == 1);
    }

    public static void main(String[] args) throws InterruptedException {
        for (int r = 0; r < 3; r++) {
            System.out.println("r" + r + " ref " + refRow());
            System.out.println("r" + r + " array " + arrayRow());
            System.out.println("r" + r + " short " + shortRow());
            System.out.println("r" + r + " int " + intRow());
        }
    }
}
