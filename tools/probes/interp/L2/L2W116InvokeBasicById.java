// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 116, lane L2: `CRATONVM_JIT_MH_COMPILED_LINK_BY_ID`
// (default off; page
// docs/internal/fixed-bugs/interpreter-i115-L2-proposal-compiled-invoke-basic-linked-by-class-id-KEPT-OFF-20261224.md).
//
// Combinator chains whose `LambdaForm`s are SPUN (hidden `LambdaForm$MH/0x...`
// classes, not the CDS holders), each called hot from a compiled caller, so
// their `invokeBasic` / `linkToStatic` steps are the ones the switch links by
// class id. Rows cover a plain return, a primitive return, a reference
// result, a throwable leaving a spun form (it has no handler of its own, so
// the linked call must hand it outward unchanged), a `guardWithTest` with
// both arms, and a `catchException` form (a form WITH a handler: never linked
// by id, it must still be right). Every line must be the same with the switch
// off and on, both modes, JIT and --nojit.
//
// Run:
//   javac -d out tools/probes/interp/L2/L2W116InvokeBasicById.java
//   java -cp out L2W116InvokeBasicById                                   (HotSpot reference)
//   cratonvm --jdk-only -cp out L2W116InvokeBasicById
//   CRATONVM_JIT_MH_COMPILED_LINK_BY_ID=1 cratonvm --jdk-only -cp out L2W116InvokeBasicById
//   (and --compatible, and --nojit once per mode)
// Bench rows: lines starting with "TIME " (ns per chain call; interleave the
// arms, take medians). Expected: `TIME filter-chain`, `TIME insert-chain` and
// `TIME gwt-chain` drop with the switch on; `TIME catch-chain` flat.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// filter-chain 30000000000
// insert-chain 10000500000
// string-chain abc:7:x
// throw-chain caught IllegalStateException odd 7
// throw-count 50000
// gwt-chain -3749875000
// catch-chain 100000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L2W116InvokeBasicById {
    static final int CALLS = 100_000;
    static final int REPS = 5;

    static int add(int a, int b) { return a + b; }
    static int twice(int a) { return a * 2; }
    static int plus3(int a) { return a + 3; }
    static String cat(String s, int i) { return s + ":" + i; }
    static String suffix(String s) { return s + ":x"; }
    static boolean even(int a) { return (a & 1) == 0; }
    static int half(int a) { return a / 2; }
    static int neg(int a) { return -a; }
    static int checkEven(int a) {
        if ((a & 1) != 0) {
            throw new IllegalStateException("odd " + a);
        }
        return a;
    }
    static int boom(int a) {
        throw new IllegalArgumentException("boom");
    }
    static int recover(IllegalArgumentException e, int a) { return 1; }

    static final MethodHandle FILTER;
    static final MethodHandle INSERT;
    static final MethodHandle STRING;
    static final MethodHandle THROW;
    static final MethodHandle GWT;
    static final MethodHandle CATCH;

    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            Class<?> c = L2W116InvokeBasicById.class;
            MethodType ii = MethodType.methodType(int.class, int.class);
            MethodHandle add = l.findStatic(c, "add", MethodType.methodType(int.class, int.class, int.class));
            MethodHandle twice = l.findStatic(c, "twice", ii);
            MethodHandle plus3 = l.findStatic(c, "plus3", ii);
            // (int,int)int -> twice -> plus3, with the second argument filtered too.
            FILTER = MethodHandles.filterReturnValue(
                    MethodHandles.filterReturnValue(MethodHandles.filterArguments(add, 1, twice), twice),
                    plus3);
            INSERT = MethodHandles.filterReturnValue(MethodHandles.insertArguments(add, 1, 3), twice);
            STRING = MethodHandles.filterReturnValue(
                    l.findStatic(c, "cat", MethodType.methodType(String.class, String.class, int.class)),
                    l.findStatic(c, "suffix", MethodType.methodType(String.class, String.class)));
            THROW = MethodHandles.filterReturnValue(l.findStatic(c, "checkEven", ii), plus3);
            GWT = MethodHandles.guardWithTest(
                    l.findStatic(c, "even", MethodType.methodType(boolean.class, int.class)),
                    MethodHandles.filterReturnValue(l.findStatic(c, "half", ii), plus3),
                    MethodHandles.filterReturnValue(l.findStatic(c, "neg", ii), twice));
            CATCH = MethodHandles.catchException(
                    l.findStatic(c, "boom", ii),
                    IllegalArgumentException.class,
                    l.findStatic(c, "recover",
                            MethodType.methodType(int.class, IllegalArgumentException.class, int.class)));
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static long filterChain() throws Throwable {
        long sum = 0;
        for (int i = 0; i < CALLS; i++) {
            sum += (int) FILTER.invokeExact(i, i);
        }
        return sum;
    }

    static long insertChain() throws Throwable {
        long sum = 0;
        for (int i = 0; i < CALLS; i++) {
            sum += (int) INSERT.invokeExact(i);
        }
        return sum;
    }

    static long gwtChain() throws Throwable {
        long sum = 0;
        for (int i = 0; i < CALLS; i++) {
            sum += (int) GWT.invokeExact(i);
        }
        return sum;
    }

    static long catchChain() throws Throwable {
        long sum = 0;
        for (int i = 0; i < CALLS; i++) {
            sum += (int) CATCH.invokeExact(i);
        }
        return sum;
    }

    static String lastThrow = "";

    static int throwChain() throws Throwable {
        int caught = 0;
        for (int i = 0; i < CALLS; i++) {
            try {
                int r = (int) THROW.invokeExact(i);
                if (r != i + 3) {
                    throw new AssertionError("bad " + r);
                }
            } catch (IllegalStateException e) {
                caught++;
                if (i == 7) {
                    lastThrow = e.getClass().getSimpleName() + " " + e.getMessage();
                }
            }
        }
        return caught;
    }

    static long time(String row, java.util.concurrent.Callable<Long> body) throws Exception {
        long[] t = new long[REPS];
        long result = 0;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            result = body.call();
            t[r] = System.nanoTime() - t0;
        }
        java.util.Arrays.sort(t);
        System.out.println("TIME " + row + " " + t[REPS / 2] / CALLS + " ns/call");
        return result;
    }

    public static void main(String[] args) throws Exception {
        long filter = time("filter-chain", () -> {
            try { return filterChain(); } catch (Throwable e) { throw new RuntimeException(e); }
        });
        long insert = time("insert-chain", () -> {
            try { return insertChain(); } catch (Throwable e) { throw new RuntimeException(e); }
        });
        String s = "";
        for (int i = 0; i < CALLS / 10; i++) {
            try {
                s = (String) STRING.invokeExact("abc", 7);
            } catch (Throwable e) {
                throw new RuntimeException(e);
            }
        }
        long thrown = time("throw-chain", () -> {
            try { return (long) throwChain(); } catch (Throwable e) { throw new RuntimeException(e); }
        });
        long gwt = time("gwt-chain", () -> {
            try { return gwtChain(); } catch (Throwable e) { throw new RuntimeException(e); }
        });
        long caught = time("catch-chain", () -> {
            try { return catchChain(); } catch (Throwable e) { throw new RuntimeException(e); }
        });
        System.out.println("filter-chain " + filter);
        System.out.println("insert-chain " + insert);
        System.out.println("string-chain " + s);
        System.out.println("throw-chain caught " + lastThrow);
        System.out.println("throw-count " + thrown);
        System.out.println("gwt-chain " + gwt);
        System.out.println("catch-chain " + caught);
    }
}
