// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 92, lane L4: a failed VarHandle.compareAndExchange
// never answers its `expected` as the witness (HotSpot: one cmpxchg, whose
// result is the witness). Native `varhandle_compare_and_exchange_raw` ->
// `vh_compare_and_exchange_slot` (native-builtins/src/lang_invoke.rs).
//
// Protocol per row: SETTERS threads run cae(E -> N) and count the calls whose
// witness == E (a claimed exchange); RESETTERS threads run cae(N -> E) and
// count witness == N. Every claimed exchange is a real transition of a variable
// that only ever holds E or N, so on HotSpot
//     claimedSet - claimedReset == (final == N ? 1 : 0)
// exactly. A witness of `expected` for a FAILED exchange (the re-read after a
// failed CAS seeing the other side's restore) is a claim without a store, and
// the balance is off. The output is the verdict only, so it is deterministic.
//
// Rows: long instance field, Integer-reference instance field (two distinct
// boxes, compared by identity), static int field, int[] element, Object[]
// element. Racy: the orchestrator runs it several times, `--nojit` with
// CRATONVM_VH_INTERP_CAE_FAST=0 (the native on every call) and default, both
// modes; HotSpot never prints `false`.
//
// Run: javac -d . L4W92CaeWitnessContention.java
//      java -cp . L4W92CaeWitnessContention
//      CRATONVM_VH_INTERP_CAE_FAST=0 cratonvm --nojit --jdk-only -cp . L4W92CaeWitnessContention
//      CRATONVM_VH_INTERP_CAE_FAST=0 cratonvm --nojit --compatible -cp . L4W92CaeWitnessContention
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint)
// row long-field balanced true
// row ref-field balanced true
// row static-int balanced true
// row int-array balanced true
// row ref-array balanced true
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicLong;

public class L4W92CaeWitnessContention {
    static final int THREADS_PER_SIDE = 2;

    volatile long l;
    volatile Object r;
    static volatile int s;

    static final VarHandle L;
    static final VarHandle R;
    static final VarHandle S;
    static final VarHandle IA = MethodHandles.arrayElementVarHandle(int[].class);
    static final VarHandle OA = MethodHandles.arrayElementVarHandle(Object[].class);

    static {
        try {
            MethodHandles.Lookup lk = MethodHandles.lookup();
            L = lk.findVarHandle(L4W92CaeWitnessContention.class, "l", long.class);
            R = lk.findVarHandle(L4W92CaeWitnessContention.class, "r", Object.class);
            S = lk.findStaticVarHandle(L4W92CaeWitnessContention.class, "s", int.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    interface Cae {
        /** true when the witness equals {@code from}: a claimed exchange */
        boolean claim(boolean forward);
    }

    static boolean run(int n, Cae cae, java.util.function.BooleanSupplier finalIsN)
            throws InterruptedException {
        AtomicLong set = new AtomicLong();
        AtomicLong reset = new AtomicLong();
        CountDownLatch go = new CountDownLatch(1);
        Thread[] ts = new Thread[THREADS_PER_SIDE * 2];
        for (int t = 0; t < ts.length; t++) {
            final boolean forward = (t % 2) == 0;
            ts[t] = new Thread(() -> {
                try {
                    go.await();
                } catch (InterruptedException e) {
                    return;
                }
                long mine = 0;
                for (int i = 0; i < n; i++) {
                    if (cae.claim(forward)) {
                        mine++;
                    }
                }
                (forward ? set : reset).addAndGet(mine);
            });
            ts[t].start();
        }
        go.countDown();
        for (Thread t : ts) {
            t.join();
        }
        return set.get() - reset.get() == (finalIsN.getAsBoolean() ? 1 : 0);
    }

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        L4W92CaeWitnessContention h = new L4W92CaeWitnessContention();

        h.l = 1L;
        boolean ok = run(n, fwd -> fwd
                ? (long) L.compareAndExchange(h, 1L, 2L) == 1L
                : (long) L.compareAndExchange(h, 2L, 1L) == 2L, () -> h.l == 2L);
        System.out.println("row long-field balanced " + ok);

        final Object e = new Object();
        final Object nn = new Object();
        h.r = e;
        ok = run(n, fwd -> fwd
                ? R.compareAndExchange(h, e, nn) == e
                : R.compareAndExchange(h, nn, e) == nn, () -> h.r == nn);
        System.out.println("row ref-field balanced " + ok);

        s = 1;
        ok = run(n, fwd -> fwd
                ? (int) S.compareAndExchange(1, 2) == 1
                : (int) S.compareAndExchange(2, 1) == 2, () -> s == 2);
        System.out.println("row static-int balanced " + ok);

        final int[] ia = {0, 1, 0};
        ok = run(n, fwd -> fwd
                ? (int) IA.compareAndExchange(ia, 1, 1, 2) == 1
                : (int) IA.compareAndExchange(ia, 1, 2, 1) == 2, () -> ia[1] == 2);
        System.out.println("row int-array balanced " + ok);

        final Object[] oa = {null, e, null};
        ok = run(n, fwd -> fwd
                ? OA.compareAndExchange(oa, 1, e, nn) == e
                : OA.compareAndExchange(oa, 1, nn, e) == nn, () -> oa[1] == nn);
        System.out.println("row ref-array balanced " + ok);
    }
}
