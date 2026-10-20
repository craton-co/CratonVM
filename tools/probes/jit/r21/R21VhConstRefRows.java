// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 22, lane vhconst: the constant-VarHandle shapes UPDATERS21-5 / -6 left open --
// a reference compareAndExchange in a CAST context (`(String) VO.compareAndExchange(h, e, v)`) and the
// STORE modes of `arrayElementVarHandle(Object[].class)` (set / setRelease / setVolatile / CAS /
// compareAndExchange / getAndSet), with the covariant store check, the NPE and the AIOOBE.
//
// Facts are printed cold, then again through the warmed kernels (each its own method, called hot first
// so a compiled body answers the exception rows too).
//
// Run:
//   javac -d out R21VhConstRefRows.java
//   java -Xmx512m -cp out R21VhConstRefRows                                      # HotSpot 25
//   cratonvm -Xmx512m -cp out R21VhConstRefRows                                  # default (--jdk-only)
//   CRATONVM_JIT_VH_CONST_CAST_CAE=1 cratonvm -Xmx512m -cp out R21VhConstRefRows
//   CRATONVM_JIT_VH_CONST_REF_ARRAY_STORE=1 cratonvm -Xmx512m -cp out R21VhConstRefRows
//   (vh-narr-set stores a subclass value: served through the supers displays, which
//   CRATONVM_JIT_TYPECHECK_DISPLAY_ARM=0 turns off -- it then keeps the call)
//   cratonvm --compatible ... R21VhConstRefRows                                  # every arm again
// Engagement: CRATONVM_DBG_JITC=1 prints `vh-const-inline (ir|sp)` for the cast CAE and
// `vh-const-ref-array-store-inline (ir|sp)` per served array store site.
// -Dr21.rows=conf prints the facts only; -Dr21.rows=bench the timing rows only.
//
// HotSpot 25 expectations (by reading JDK 25 `VarHandleReferences` / `X-VarHandle.java.template`,
// `Invokers` and the JLS; to be confirmed on the host):
// * cast CAE: the exchange happens, THEN the witness is cast. `cae-cast-wrong-hit` (field and expected
//   the same Integer) is `java.lang.ClassCastException field=n1` (the field WAS exchanged);
//   `cae-cast-wrong-miss` is `java.lang.ClassCastException field=7` (unchanged); hits return the witness.
// * Object[] handle stores: `runtimeTypeCheck` checks the NEW value against the ARRAY's own component,
//   `reflectiveTypeCheck` throws `new ArrayStoreException()`: `java.lang.ArrayStoreException msg=null`,
//   array unchanged. `expected` is only compared (componentType Object). Plain `set` is different
//   (JDK 25 src: `array[index] = handle.componentType.cast(value)`, an `aastore`): cold it is
//   `msg=java.lang.Integer` (the class name); warm HotSpot's C2 turns that hot implicit `aastore` /
//   AIOOBE into an OmitStackTraceInFastThrow throw, `msg=null` (measured 25.0.3; wave 25, lane vhase).
// * `set` evaluates `array[index] = runtimeTypeCheck(..)`: the store check (RHS) runs before the index
//   check, so `set-both-bad` is the ArrayStoreException; setVolatile / compareAndSet evaluate
//   `Preconditions.checkIndex` first, so their `*-both-bad` rows are
//   `java.lang.ArrayIndexOutOfBoundsException msg=Index 8 out of bounds for length 8`.
// * A null array is a NullPointerException (class only: the helpful message names JDK locals).
// * Every `fact` line identical on HotSpot, every CratonVM arm and --nojit. Timing lines
//   `time <row> <N> ns/op`, median of 9: HotSpot about 0-2 ns for the sets, 5-10 ns for the locked rows.

import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.function.Supplier;

public class R21VhConstRefRows {
    static class Holder {
        volatile String s;
        volatile Object o;
    }

    static final VarHandle VS;
    static final VarHandle VO;
    static final VarHandle VOA = MethodHandles.arrayElementVarHandle(Object[].class);
    static {
        try {
            MethodHandles.Lookup lk = MethodHandles.lookup();
            VS = lk.findVarHandle(Holder.class, "s", String.class);
            VO = lk.findVarHandle(Holder.class, "o", Object.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static final String[] VALS = {"v0", "v1", "v2", "v3"};
    static final int N = 200_000;
    static final int WARM = 20;
    static final int REPS = 9;
    static long sink;

    static void fact(String row, Object v) {
        System.out.println("fact " + row + " " + v);
    }

    static String exc(Throwable t) {
        return t.getClass().getName() + " msg=" + t.getMessage();
    }

    static String attempt(Supplier<Object> s) {
        try {
            return "ok " + s.get();
        } catch (NullPointerException | ClassCastException e) {
            return e.getClass().getName();
        } catch (Throwable t) {
            return exc(t);
        }
    }

    // ── the fact kernels (warmed before the warm facts) ───────────────────

    static String castCae(Holder h, Object e, Object v) { return (String) VO.compareAndExchange(h, e, v); }
    static String castCaeAcq(Holder h, Object e, Object v) { return (String) VO.compareAndExchangeAcquire(h, e, v); }
    static String castCaeStr(Holder h, String e, String v) { return (String) VS.compareAndExchange(h, e, v); }
    static void oaSet(Object[] a, int i, Object v) { VOA.set(a, i, v); }
    static void oaSetRelease(Object[] a, int i, Object v) { VOA.setRelease(a, i, v); }
    static void oaSetVolatile(Object[] a, int i, Object v) { VOA.setVolatile(a, i, v); }
    static boolean oaCas(Object[] a, int i, Object e, Object v) { return VOA.compareAndSet(a, i, e, v); }
    static Object oaCae(Object[] a, int i, Object e, Object v) { return VOA.compareAndExchange(a, i, e, v); }
    static Object oaGas(Object[] a, int i, Object v) { return VOA.getAndSet(a, i, v); }
    static void oaCasVoid(Object[] a, int i, Object e, Object v) { VOA.compareAndSet(a, i, e, v); }

    static void warmFactKernels() {
        Holder h = new Holder();
        Object[] os = new Object[8];
        String[] ss = new String[8];
        for (int k = 0; k < N; k++) {
            String a = VALS[k & 3], b = VALS[(k + 1) & 3];
            h.o = a;
            h.s = a;
            sink += castCae(h, a, b).length() + castCaeAcq(h, b, a).length() + castCaeStr(h, a, b).length();
            oaSet(os, k & 7, a);
            oaSetRelease(ss, k & 7, b);
            oaSetVolatile(os, k & 7, b);
            sink += oaCas(ss, k & 7, b, a) ? 1 : 0;
            sink += oaCae(os, k & 7, b, a) == a ? 1 : 0;
            sink += oaGas(ss, k & 7, b) == a ? 1 : 0;
            oaCasVoid(os, k & 7, a, b);
        }
    }

    static String arr(Object[] a) {
        return a.getClass().getSimpleName() + java.util.Arrays.toString(a);
    }

    static void facts(String phase) {
        String p = phase + " ";
        // UPDATERS21-6: the cast compareAndExchange.
        Holder h = new Holder();
        h.o = "o0";
        fact(p + "cae-cast-hit", attempt(() -> castCae(h, h.o, "n1")) + " field=" + h.o);
        fact(p + "cae-cast-miss", attempt(() -> castCae(h, "zz", "n2")) + " field=" + h.o);
        h.o = null;
        fact(p + "cae-cast-null-hit", attempt(() -> castCae(h, null, "n3")) + " field=" + h.o);
        h.o = null;
        fact(p + "cae-cast-null-miss", attempt(() -> castCae(h, "zz", "n4")) + " field=" + h.o);
        Integer seven = Integer.valueOf(7);
        h.o = seven;
        fact(p + "cae-cast-wrong-hit", attempt(() -> castCae(h, seven, "n1")) + " field=" + h.o);
        h.o = seven;
        fact(p + "cae-cast-wrong-miss", attempt(() -> castCae(h, "zz", "n1")) + " field=" + h.o);
        h.o = seven;
        fact(p + "cae-acq-cast-wrong-hit", attempt(() -> castCaeAcq(h, seven, "n5")) + " field=" + h.o);
        h.s = "s0";
        fact(p + "cae-str-hit", attempt(() -> castCaeStr(h, h.s, "s1")) + " field=" + h.s);
        fact(p + "cae-str-miss", attempt(() -> castCaeStr(h, "zz", "s2")) + " field=" + h.s);
        fact(p + "cae-null-holder", attempt(() -> castCae(null, null, "n")));

        // UPDATERS21-5: Object[] element stores.
        Object[] os = new Object[8];
        fact(p + "oarr-set", attempt(() -> { oaSet(os, 1, "a"); oaSet(os, 2, new int[0].length); return arr(os); }));
        fact(p + "oarr-set-array-value", attempt(() -> { oaSet(os, 3, new String[] {"x"}); return os[3].getClass().getName(); }));
        String[] ss = new String[8];
        fact(p + "sarr-set", attempt(() -> { oaSetRelease(ss, 0, "s"); oaSet(ss, 1, null); return arr(ss); }));
        fact(p + "sarr-set-integer", attempt(() -> { oaSet(ss, 2, seven); return arr(ss); }) + " " + arr(ss));
        fact(p + "sarr-setvolatile-integer", attempt(() -> { oaSetVolatile(ss, 2, seven); return arr(ss); }));
        fact(p + "sarr-setrelease-integer", attempt(() -> { oaSetRelease(ss, 2, seven); return arr(ss); }));
        fact(p + "sarr-cas-integer", attempt(() -> oaCas(ss, 0, "s", seven)) + " " + ss[0]);
        fact(p + "sarr-cas-integer-miss", attempt(() -> oaCas(ss, 0, "zz", seven)) + " " + ss[0]);
        fact(p + "sarr-cas-expected-integer", attempt(() -> oaCas(ss, 0, seven, "t")) + " " + ss[0]);
        fact(p + "sarr-cae-integer", attempt(() -> oaCae(ss, 0, "s", seven)) + " " + ss[0]);
        fact(p + "sarr-gas-integer", attempt(() -> oaGas(ss, 0, seven)) + " " + ss[0]);
        fact(p + "sarr-cas", attempt(() -> oaCas(ss, 0, "s", "t")) + " " + ss[0]);
        fact(p + "sarr-cae", attempt(() -> oaCae(ss, 0, "zz", "u")) + " " + ss[0]);
        fact(p + "sarr-gas", attempt(() -> oaGas(ss, 0, null)) + " " + ss[0]);
        Number[] ns = new Number[4];
        fact(p + "narr-set-integer", attempt(() -> { oaSet(ns, 0, seven); oaSet(ns, 1, 2L); return arr(ns); }));
        fact(p + "narr-set-string", attempt(() -> { oaSet(ns, 2, "x"); return arr(ns); }));
        CharSequence[] cs = new CharSequence[2];
        fact(p + "csarr-set-string", attempt(() -> { oaSet(cs, 0, "x"); return arr(cs); }));
        fact(p + "csarr-set-array-value", attempt(() -> { oaSet(cs, 1, new String[0]); return arr(cs); }));
        Object[][] nested = new String[2][];
        fact(p + "nested-set-string-array", attempt(() -> { oaSet(nested, 0, new String[] {"q"}); return nested[0].length; }));
        fact(p + "nested-set-object-array", attempt(() -> { oaSet(nested, 1, new Object[0]); return nested[1]; }));
        // The index and the array.
        fact(p + "oarr-set-oob", attempt(() -> { oaSet(os, 8, "a"); return "stored"; }));
        fact(p + "oarr-set-negative", attempt(() -> { oaSet(os, -1, "a"); return "stored"; }));
        fact(p + "oarr-cas-oob", attempt(() -> oaCas(os, 8, null, "a")));
        fact(p + "oarr-gas-oob", attempt(() -> oaGas(os, 9, "a")));
        fact(p + "sarr-set-both-bad", attempt(() -> { oaSet(ss, 8, seven); return "stored"; }));
        fact(p + "sarr-setvolatile-both-bad", attempt(() -> { oaSetVolatile(ss, 8, seven); return "stored"; }));
        fact(p + "sarr-cas-both-bad", attempt(() -> oaCas(ss, 8, null, seven)));
        fact(p + "null-set", attempt(() -> { oaSet(null, 0, "a"); return "stored"; }));
        fact(p + "null-cas", attempt(() -> oaCas(null, 0, null, "a")));
        fact(p + "null-gas", attempt(() -> oaGas(null, 0, "a")));
        fact(p + "oarr-casvoid", attempt(() -> { os[4] = "e"; oaCasVoid(os, 4, "e", "f"); return os[4]; }));
    }

    // ── timing kernels ───────────────────────────────────────────────────

    static final Object[] OA = new Object[8];
    static final String[] SA = new String[8];
    static final Number[] NA = new Number[8];
    static final Integer[] INTS = {1, 2, 3, 4};

    static long vhRefCastCae(Holder h, int n) { long s = 0; for (int k = 0; k < n; k++) s += ((String) VS.compareAndExchange(h, VALS[k & 3], VALS[(k + 1) & 3])).length(); return s; }
    static long vhObjCastCae(Holder h, int n) { long s = 0; for (int k = 0; k < n; k++) s += ((String) VO.compareAndExchange(h, VALS[k & 3], VALS[(k + 1) & 3])).length(); return s; }
    static long vhOarrSet(Holder h, int n) { for (int k = 0; k < n; k++) VOA.set(OA, k & 7, VALS[k & 3]); return OA[3].hashCode() & 1; }
    static long vhOarrSetRelease(Holder h, int n) { for (int k = 0; k < n; k++) VOA.setRelease(OA, k & 7, VALS[k & 3]); return OA[3].hashCode() & 1; }
    static long vhOarrSetVolatile(Holder h, int n) { for (int k = 0; k < n; k++) VOA.setVolatile(OA, k & 7, VALS[k & 3]); return OA[3].hashCode() & 1; }
    static long vhOarrCas(Holder h, int n) { long s = 0; for (int k = 0; k < n; k++) s += VOA.compareAndSet(OA, k & 7, VALS[k & 3], VALS[(k + 1) & 3]) ? 1 : 0; return s; }
    static long vhOarrCae(Holder h, int n) { long s = 0; for (int k = 0; k < n; k++) s += VOA.compareAndExchange(OA, k & 7, VALS[k & 3], VALS[(k + 1) & 3]) == null ? 1 : 0; return s; }
    static long vhOarrGetAndSet(Holder h, int n) { long s = 0; for (int k = 0; k < n; k++) s += VOA.getAndSet(OA, k & 7, VALS[k & 3]) == null ? 1 : 0; return s; }
    static long vhSarrSet(Holder h, int n) { for (int k = 0; k < n; k++) VOA.set(SA, k & 7, VALS[k & 3]); return SA[3].length(); }
    static long vhNarrSet(Holder h, int n) { for (int k = 0; k < n; k++) VOA.set(NA, k & 7, INTS[k & 3]); return NA[3].intValue(); }

    static final String[] VH_ROWS = {
        "vh-ref-cast-cae", "vh-obj-cast-cae", "vh-oarr-set", "vh-oarr-setrelease", "vh-oarr-setvolatile",
        "vh-oarr-cas", "vh-oarr-cae", "vh-oarr-getandset", "vh-sarr-set", "vh-narr-set",
    };

    static Holder fresh() {
        Holder h = new Holder();
        h.s = "v0";
        h.o = "v0";
        java.util.Arrays.fill(OA, null);
        java.util.Arrays.fill(SA, null);
        java.util.Arrays.fill(NA, null);
        return h;
    }

    static long callVh(int row, Holder h, int n) {
        switch (row) {
            case 0: return vhRefCastCae(h, n);
            case 1: return vhObjCastCae(h, n);
            case 2: return vhOarrSet(h, n);
            case 3: return vhOarrSetRelease(h, n);
            case 4: return vhOarrSetVolatile(h, n);
            case 5: return vhOarrCas(h, n);
            case 6: return vhOarrCae(h, n);
            case 7: return vhOarrGetAndSet(h, n);
            case 8: return vhSarrSet(h, n);
            default: return vhNarrSet(h, n);
        }
    }

    static void warm() {
        for (int w = 0; w < WARM; w++) {
            for (int row = 0; row < VH_ROWS.length; row++) {
                sink += callVh(row, fresh(), N / 10);
            }
        }
    }

    static void bench() {
        for (int row = 0; row < VH_ROWS.length; row++) {
            List<Long> times = new ArrayList<>();
            for (int r = 0; r < REPS; r++) {
                Holder h = fresh();
                long t0 = System.nanoTime();
                sink += callVh(row, h, N);
                long t1 = System.nanoTime();
                times.add(t1 - t0);
            }
            Collections.sort(times);
            System.out.println("time " + VH_ROWS[row] + " " + (times.get(REPS / 2) / N) + " ns/op");
        }
    }

    public static void main(String[] args) {
        String rows = System.getProperty("r21.rows", "all");
        boolean conf = !rows.equals("bench");
        boolean timing = !rows.equals("conf");
        if (conf) {
            facts("cold");
        }
        warmFactKernels();
        warm();
        if (conf) {
            facts("warm");
            // The timing kernels' own answers on fixed inputs, after warm-up.
            for (int row = 0; row < VH_ROWS.length; row++) {
                fact("kernel " + VH_ROWS[row], callVh(row, fresh(), 1000));
            }
        }
        if (timing) {
            bench();
        }
        if (sink == 42) {
            System.out.println("unlikely");
        }
    }
}
