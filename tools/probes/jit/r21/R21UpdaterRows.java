// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 1, lane updaters: AtomicIntegerFieldUpdater /
// AtomicLongFieldUpdater / AtomicReferenceFieldUpdater, conformance and cost.
//
// Facts (cold, then again after every kernel is warm, so a compiled body and
// the interpreter must both answer them): get / set / lazySet / CAS hit and
// miss / getAndAdd / getAndIncrement / getAndSet / incrementAndGet /
// addAndGet / getAndUpdate / accumulateAndGet on each updater; a SUBCLASS
// receiver; a wrong-class receiver through a raw updater (HotSpot: a
// java.lang.ClassCastException with a null message, `throwAccessCheckException`);
// a null receiver (the same ClassCastException: `isInstance(null)` is false);
// a wrong-typed value through a raw reference updater (a message-less
// ClassCastException from `valueCheck`), and a null value (allowed).
//
// Run:
//   javac -d out R21UpdaterRows.java
//   java -Xmx512m -cp out R21UpdaterRows                                        # HotSpot 25
//   cratonvm -Xmx512m -cp out R21UpdaterRows                                    # default (--jdk-only)
//   CRATONVM_JIT_UNSAFE_FIELD_CELLS_BYTE_OFFSETS=1 CRATONVM_JIT_UNSAFE_VOLATILE_PUBLISHES_FIELD_CELL=1 \
//     cratonvm -Xmx512m -cp out R21UpdaterRows                                  # the r21 arm
//   CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS=0 cratonvm -Xmx512m -cp out R21UpdaterRows   # slot offsets (the
//                                                                               # field arm as in w11d)
//   cratonvm --compatible -Xmx512m -cp out R21UpdaterRows                       # every arm again
//   CRATONVM_ARFU_VALUE_CHECK=0 cratonvm --compatible ... R21UpdaterRows         # kill: the rfu
//                                                                               # value-check facts DIFF
//   cratonvm --nojit -Xmx512m -cp out R21UpdaterRows                            # facts only
//   CRATONVM_FIELD_UPDATER_JDK_RULES=0 cratonvm --compatible ... R21UpdaterRows  # kill: the `factory`
//                                                                               # facts DIFF in --compatible
//   Wave 2 arms (opt-in): CRATONVM_JIT_UNSAFE_REF_VOLATILE_FIELD=1 (rfu-get / rfu-set / rfu-lazyset rows,
//   with the two arm-2 switches), CRATONVM_JIT_UNSAFE_RMW_VETO_NEEDS_FIELD_ARM=1 (default arm only: ifu-getandadd),
//   CRATONVM_JIT_IR_SPLICE_NEVER_RUN_PRIVATE_EXIT=1 (every row; CRATONVM_DBG_JITC=1 prints
//   `splice-never-run-private-exit ...throwAccessCheckException`).
//   Wave 3 (constant VarHandle, `time vh-*` rows): CRATONVM_JIT_VH_CONST_CAST_RETURN=1 (vh-ref-cast-get,
//   vh-ref-getacquire: a `(String)` cast gives the site a narrower return) and CRATONVM_JIT_VH_CONST_VOID_RMW=1
//   (vh-int-getandadd-void, vh-int-cas-void: a discarded result gives a `V` return); CRATONVM_DBG_JITC=1 prints
//   `vh-const-inline (ir)` per served site. HotSpot: every vh row 0-5 ns.
//   Wave 4: CRATONVM_JIT_VH_CONST_REF_ARRAY_GET=1 (vh-oarr-getacquire, `vh-const-ref-array-inline`) and
//   CRATONVM_JIT_VH_CONST_CAST_RMW=1 (vh-ref-getandset); without them those two rows stay calls.
//   Facts `vh-cast-get-wrong-class` (java.lang.ClassCastException) / `vh-cast-get-null` (null) pin the cast.
// -Dr21.rows=conf prints the facts only; -Dr21.rows=bench the timing rows only.
//
// Engagement: CRATONVM_DBG_JITC=1 prints `ir-unsafe-inline ... volatile field
// arm` / `ir-unsafe-rmw-inline ... field_arm=true` / `ir-unsafe-ref-cas-inline`
// per site; with the default arm under byte offsets `field_arm=false`.
//
// HotSpot expectation: every `fact` line identical on HotSpot 25, every
// CratonVM arm and --nojit. Timing lines `time <row> <N> ns/op` (one op = one
// updater call, median of 9 reps after warm-up): HotSpot 0-1 ns for get / set
// / lazySet (the loop is hoisted or one MOV), about 5 ns for the locked RMW
// rows (CAS, getAndAdd, getAndIncrement, getAndSet).

import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.atomic.AtomicIntegerFieldUpdater;
import java.util.concurrent.atomic.AtomicLongFieldUpdater;
import java.util.concurrent.atomic.AtomicReferenceFieldUpdater;

public class R21UpdaterRows {
    static class Holder {
        volatile int i;
        volatile long l;
        volatile String s;
        volatile Object o;
    }

    static final class SubHolder extends Holder {
        int pad;
    }

    static final class Other {
        volatile int i;
    }

    static class WithShapes {
        volatile Object obj;
        volatile boolean flag;
        int plain;
        static volatile int stat;
    }

    // ── factory facts (JDK 25 `Atomic*FieldUpdaterImpl` constructors) ─────

    static String newUpdaterFact(java.util.function.Supplier<Object> make) {
        try {
            Object u = make.get();
            return "ok " + (u != null);
        } catch (Throwable t) {
            Throwable c = t.getCause();
            return exc(t) + " cause=" + (c == null ? "null" : exc(c));
        }
    }

    static void factoryFacts() {
        fact("factory int-on-inherited-field",
                newUpdaterFact(() -> AtomicIntegerFieldUpdater.newUpdater(SubHolder.class, "i")));
        fact("factory int-on-own-field",
                newUpdaterFact(() -> AtomicIntegerFieldUpdater.newUpdater(Holder.class, "i")));
        fact("factory int-on-boolean-field",
                newUpdaterFact(() -> AtomicIntegerFieldUpdater.newUpdater(WithShapes.class, "flag")));
        fact("factory int-on-plain-field",
                newUpdaterFact(() -> AtomicIntegerFieldUpdater.newUpdater(WithShapes.class, "plain")));
        fact("factory int-on-static-field",
                newUpdaterFact(() -> AtomicIntegerFieldUpdater.newUpdater(WithShapes.class, "stat")));
        fact("factory int-missing-field",
                newUpdaterFact(() -> AtomicIntegerFieldUpdater.newUpdater(Holder.class, "nope")));
        fact("factory long-on-int-field",
                newUpdaterFact(() -> AtomicLongFieldUpdater.newUpdater(Holder.class, "i")));
        fact("factory ref-object-field-string-vclass",
                newUpdaterFact(() -> AtomicReferenceFieldUpdater.newUpdater(WithShapes.class, String.class, "obj")));
        fact("factory ref-object-field-object-vclass",
                newUpdaterFact(() -> AtomicReferenceFieldUpdater.newUpdater(WithShapes.class, Object.class, "obj")));
        fact("factory ref-string-field-object-vclass",
                newUpdaterFact(() -> AtomicReferenceFieldUpdater.newUpdater(Holder.class, Object.class, "s")));
        // Not a nestmate: a private field of another top-level class.
        fact("factory int-private-field-of-another-class",
                newUpdaterFact(() -> AtomicIntegerFieldUpdater.newUpdater(R21UpdaterOther.class, "p")));
        fact("factory int-package-field-of-another-class",
                newUpdaterFact(() -> AtomicIntegerFieldUpdater.newUpdater(R21UpdaterOther.class, "q")));
        // A nestmate's private field is accessible.
        fact("factory int-private-field-of-a-nestmate",
                newUpdaterFact(() -> AtomicIntegerFieldUpdater.newUpdater(Nest.class, "p")));
        // r21 review item 4: a primitive vclass on a reference field (CCE), the
        // primitive vclass that is a primitive field's type (IAE "Must be
        // reference type"), and another primitive vclass on it (CCE).
        fact("factory ref-object-field-int-vclass",
                newUpdaterFact(() -> AtomicReferenceFieldUpdater.newUpdater(WithShapes.class, int.class, "obj")));
        fact("factory ref-int-field-int-vclass",
                newUpdaterFact(() -> AtomicReferenceFieldUpdater.newUpdater(Holder.class, int.class, "i")));
        fact("factory ref-int-field-long-vclass",
                newUpdaterFact(() -> AtomicReferenceFieldUpdater.newUpdater(Holder.class, long.class, "i")));
        // r21 wave 7, lane updaters (r21-review-w45-vm-findings item 2): JDK 25 runs
        // `getDeclaredField(fieldName)` (whose `Objects.requireNonNull(name)` throws a
        // message-less NPE) inside `catch (Exception ex) { throw new RuntimeException(ex); }`
        // and compares `vclass != fieldClass` only after that `try`. HotSpot 25:
        //   null-vclass / null-vclass-on-int-field  java.lang.ClassCastException msg=null cause=null
        //   *null-fieldname (all three factories, and with a null vclass too)
        //     java.lang.RuntimeException msg=java.lang.NullPointerException
        //       cause=java.lang.NullPointerException msg=null
        //   null-vclass-missing-field  java.lang.RuntimeException msg=java.lang.NoSuchFieldException: nope
        //       cause=java.lang.NoSuchFieldException msg=nope
        fact("factory null-vclass",
                newUpdaterFact(() -> AtomicReferenceFieldUpdater.newUpdater(Holder.class, null, "s")));
        fact("factory null-vclass-on-int-field",
                newUpdaterFact(() -> AtomicReferenceFieldUpdater.newUpdater(Holder.class, null, "i")));
        fact("factory null-vclass-missing-field",
                newUpdaterFact(() -> AtomicReferenceFieldUpdater.newUpdater(Holder.class, null, "nope")));
        fact("factory null-fieldname",
                newUpdaterFact(() -> AtomicReferenceFieldUpdater.newUpdater(Holder.class, String.class, null)));
        fact("factory null-vclass-null-fieldname",
                newUpdaterFact(() -> AtomicReferenceFieldUpdater.newUpdater(Holder.class, null, null)));
        fact("factory int-null-fieldname",
                newUpdaterFact(() -> AtomicIntegerFieldUpdater.newUpdater(Holder.class, null)));
        fact("factory long-null-fieldname",
                newUpdaterFact(() -> AtomicLongFieldUpdater.newUpdater(Holder.class, null)));
        // r21 review (rev-native's fix): a java.lang.reflect.Proxy stored into an
        // interface-typed field through the updater is legal.
        try {
            AtomicReferenceFieldUpdater<RHolder, Runnable> u =
                    AtomicReferenceFieldUpdater.newUpdater(RHolder.class, Runnable.class, "r");
            Runnable p = (Runnable) java.lang.reflect.Proxy.newProxyInstance(
                    R21UpdaterRows.class.getClassLoader(), new Class<?>[] {Runnable.class}, (px, m, a) -> null);
            RHolder h = new RHolder();
            u.set(h, p);
            boolean same = u.get(h) == p;
            boolean cas = u.compareAndSet(h, p, p);
            Runnable prev = u.getAndSet(h, null);
            fact("rfu-proxy-into-interface-field", "ok " + same + " " + cas + " " + (prev == p) + " " + (h.r == null));
        } catch (Throwable t) {
            fact("rfu-proxy-into-interface-field", exc(t));
        }
    }

    static class RHolder {
        volatile Runnable r;
    }

    static class Nest {
        private volatile int p;
    }

    static final AtomicIntegerFieldUpdater<Holder> IFU =
            AtomicIntegerFieldUpdater.newUpdater(Holder.class, "i");
    static final AtomicLongFieldUpdater<Holder> LFU =
            AtomicLongFieldUpdater.newUpdater(Holder.class, "l");
    static final AtomicReferenceFieldUpdater<Holder, String> RFU =
            AtomicReferenceFieldUpdater.newUpdater(Holder.class, String.class, "s");

    static final int N = 200_000;
    static final int WARM = 20;
    static final int REPS = 9;

    static void fact(String row, Object v) {
        System.out.println("fact " + row + " " + v);
    }

    static String exc(Throwable t) {
        return t.getClass().getName() + " msg=" + t.getMessage();
    }

    // ── facts ────────────────────────────────────────────────────────────

    @SuppressWarnings({"unchecked", "rawtypes"})
    static void facts(String phase) {
        Holder h = new Holder();
        SubHolder sh = new SubHolder();
        String p = phase + " ";

        fact(p + "ifu-get0", IFU.get(h));
        IFU.set(h, 5);
        fact(p + "ifu-set", IFU.get(h) + " " + h.i);
        IFU.lazySet(h, 6);
        fact(p + "ifu-lazyset", IFU.get(h));
        fact(p + "ifu-cas-hit", IFU.compareAndSet(h, 6, 7) + " " + h.i);
        fact(p + "ifu-cas-miss", IFU.compareAndSet(h, 6, 8) + " " + h.i);
        fact(p + "ifu-wcas", IFU.weakCompareAndSet(h, 7, 9) + " " + h.i);
        IFU.set(h, 10);
        fact(p + "ifu-getandadd", IFU.getAndAdd(h, 5) + " " + h.i);
        fact(p + "ifu-getandinc", IFU.getAndIncrement(h) + " " + h.i);
        fact(p + "ifu-getanddec", IFU.getAndDecrement(h) + " " + h.i);
        fact(p + "ifu-getandset", IFU.getAndSet(h, -3) + " " + h.i);
        fact(p + "ifu-incandget", IFU.incrementAndGet(h));
        fact(p + "ifu-addandget", IFU.addAndGet(h, Integer.MAX_VALUE));
        fact(p + "ifu-getandupdate", IFU.getAndUpdate(h, x -> x * 2) + " " + h.i);
        fact(p + "ifu-accumulate", IFU.accumulateAndGet(h, 3, Integer::sum));
        IFU.set(sh, 11);
        fact(p + "ifu-subclass", IFU.getAndAdd(sh, 1) + " " + IFU.get(sh) + " " + sh.i);

        fact(p + "lfu-get0", LFU.get(h));
        LFU.set(h, 1L << 40);
        fact(p + "lfu-set", LFU.get(h) + " " + h.l);
        LFU.lazySet(h, -1L);
        fact(p + "lfu-lazyset", LFU.get(h));
        fact(p + "lfu-cas-hit", LFU.compareAndSet(h, -1L, 1L << 33) + " " + h.l);
        fact(p + "lfu-cas-miss", LFU.compareAndSet(h, -1L, 0L) + " " + h.l);
        fact(p + "lfu-getandadd", LFU.getAndAdd(h, 1L << 32) + " " + h.l);
        fact(p + "lfu-getandinc", LFU.getAndIncrement(h) + " " + h.l);
        fact(p + "lfu-getandset", LFU.getAndSet(h, Long.MIN_VALUE) + " " + h.l);
        fact(p + "lfu-incandget", LFU.incrementAndGet(h));
        fact(p + "lfu-addandget", LFU.addAndGet(h, -2L));
        LFU.set(sh, 77L);
        fact(p + "lfu-subclass", LFU.getAndIncrement(sh) + " " + LFU.get(sh) + " " + sh.l);

        fact(p + "rfu-get0", RFU.get(h));
        RFU.set(h, "a");
        fact(p + "rfu-set", RFU.get(h) + " " + h.s);
        RFU.lazySet(h, "b");
        fact(p + "rfu-lazyset", RFU.get(h));
        String b = h.s;
        fact(p + "rfu-cas-hit", RFU.compareAndSet(h, b, "c") + " " + h.s);
        fact(p + "rfu-cas-miss", RFU.compareAndSet(h, "zz", "d") + " " + h.s);
        fact(p + "rfu-getandset", RFU.getAndSet(h, "e") + " " + h.s);
        fact(p + "rfu-getandupdate", RFU.getAndUpdate(h, x -> x + "f") + " " + h.s);
        RFU.set(h, null);
        fact(p + "rfu-set-null", RFU.get(h));
        RFU.set(sh, "sub");
        fact(p + "rfu-subclass", RFU.getAndSet(sh, "sub2") + " " + RFU.get(sh));

        // A wrong-class receiver through a raw updater, a null receiver.
        AtomicIntegerFieldUpdater rawI = IFU;
        AtomicLongFieldUpdater rawL = LFU;
        AtomicReferenceFieldUpdater rawR = RFU;
        Object other = new Other();
        String[] names = {"get", "set", "cas", "getandadd", "getandinc", "getandset"};
        for (int k = 0; k < names.length; k++) {
            for (Object recv : new Object[] {other, null}) {
                String who = recv == null ? "null" : "other";
                fact(p + "ifu-" + names[k] + "-" + who, tryInt(rawI, k, recv));
                fact(p + "lfu-" + names[k] + "-" + who, tryLong(rawL, k, recv));
                fact(p + "rfu-" + names[k] + "-" + who, tryRef(rawR, k, recv));
            }
        }
        // A wrong-typed value through the raw reference updater (valueCheck).
        Holder hv = new Holder();
        hv.s = "keep";
        try {
            rawR.set(hv, Integer.valueOf(1));
            fact(p + "rfu-set-wrong-value", "no-throw " + describe(hv.s));
        } catch (Throwable t) {
            fact(p + "rfu-set-wrong-value", exc(t) + " " + describe(hv.s));
        }
        try {
            boolean ok = rawR.compareAndSet(hv, "keep", Integer.valueOf(2));
            fact(p + "rfu-cas-wrong-value", "no-throw " + ok + " " + describe(hv.s));
        } catch (Throwable t) {
            fact(p + "rfu-cas-wrong-value", exc(t) + " " + describe(hv.s));
        }
        try {
            Object prev = rawR.getAndSet(hv, new Object());
            fact(p + "rfu-getandset-wrong-value", "no-throw " + prev + " " + describe(hv.s));
        } catch (Throwable t) {
            fact(p + "rfu-getandset-wrong-value", exc(t) + " " + describe(hv.s));
        }
        try {
            Object prev = rawR.getAndUpdate(hv, x -> Long.valueOf(3));
            fact(p + "rfu-getandupdate-wrong-value", "no-throw " + prev + " " + describe(hv.s));
        } catch (Throwable t) {
            fact(p + "rfu-getandupdate-wrong-value", exc(t) + " " + describe(hv.s));
        }
    }

    /** The field's value without a checkcast (a wrong-typed store shows its class). */
    static String describe(Object v) {
        return v == null ? "null" : v.getClass().getName() + ":" + v;
    }

    @SuppressWarnings({"unchecked", "rawtypes"})
    static String tryInt(AtomicIntegerFieldUpdater u, int k, Object recv) {
        try {
            Object r;
            switch (k) {
                case 0: r = u.get(recv); break;
                case 1: u.set(recv, 1); r = "void"; break;
                case 2: r = u.compareAndSet(recv, 0, 1); break;
                case 3: r = u.getAndAdd(recv, 1); break;
                case 4: r = u.getAndIncrement(recv); break;
                default: r = u.getAndSet(recv, 2); break;
            }
            return "no-throw " + r;
        } catch (Throwable t) {
            return exc(t);
        }
    }

    @SuppressWarnings({"unchecked", "rawtypes"})
    static String tryLong(AtomicLongFieldUpdater u, int k, Object recv) {
        try {
            Object r;
            switch (k) {
                case 0: r = u.get(recv); break;
                case 1: u.set(recv, 1L); r = "void"; break;
                case 2: r = u.compareAndSet(recv, 0L, 1L); break;
                case 3: r = u.getAndAdd(recv, 1L); break;
                case 4: r = u.getAndIncrement(recv); break;
                default: r = u.getAndSet(recv, 2L); break;
            }
            return "no-throw " + r;
        } catch (Throwable t) {
            return exc(t);
        }
    }

    @SuppressWarnings({"unchecked", "rawtypes"})
    static String tryRef(AtomicReferenceFieldUpdater u, int k, Object recv) {
        try {
            Object r;
            switch (k) {
                case 0: r = u.get(recv); break;
                case 1: u.set(recv, "x"); r = "void"; break;
                case 2: r = u.compareAndSet(recv, null, "x"); break;
                case 3: r = u.getAndUpdate(recv, x -> "y"); break;
                case 4: r = u.accumulateAndGet(recv, "z", (a, c) -> c); break;
                default: r = u.getAndSet(recv, "w"); break;
            }
            return "no-throw " + r;
        } catch (Throwable t) {
            return exc(t);
        }
    }

    // ── kernels (one per row, each its own method so the optimizing tier
    // compiles the loop) ─────────────────────────────────────────────────

    static long ifuGet(Holder h, int n) {
        long s = 0;
        for (int k = 0; k < n; k++) s += IFU.get(h);
        return s;
    }

    static long ifuSet(Holder h, int n) {
        for (int k = 0; k < n; k++) IFU.set(h, k);
        return h.i;
    }

    static long ifuLazySet(Holder h, int n) {
        for (int k = 0; k < n; k++) IFU.lazySet(h, k);
        return h.i;
    }

    static long ifuCas(Holder h, int n) {
        long s = 0;
        for (int k = 0; k < n; k++) s += IFU.compareAndSet(h, k, k + 1) ? 1 : 0;
        return s;
    }

    static long ifuGetAndAdd(Holder h, int n) {
        long s = 0;
        for (int k = 0; k < n; k++) s += IFU.getAndAdd(h, 3);
        return s;
    }

    static long ifuGetAndInc(Holder h, int n) {
        long s = 0;
        for (int k = 0; k < n; k++) s += IFU.getAndIncrement(h);
        return s;
    }

    static long ifuGetAndSet(Holder h, int n) {
        long s = 0;
        for (int k = 0; k < n; k++) s += IFU.getAndSet(h, k);
        return s;
    }

    static long lfuGet(Holder h, int n) {
        long s = 0;
        for (int k = 0; k < n; k++) s += LFU.get(h);
        return s;
    }

    static long lfuSet(Holder h, int n) {
        for (int k = 0; k < n; k++) LFU.set(h, k);
        return h.l;
    }

    static long lfuLazySet(Holder h, int n) {
        for (int k = 0; k < n; k++) LFU.lazySet(h, k);
        return h.l;
    }

    static long lfuCas(Holder h, int n) {
        long s = 0;
        for (int k = 0; k < n; k++) s += LFU.compareAndSet(h, k, k + 1) ? 1 : 0;
        return s;
    }

    static long lfuGetAndAdd(Holder h, int n) {
        long s = 0;
        for (int k = 0; k < n; k++) s += LFU.getAndAdd(h, 3L);
        return s;
    }

    static long lfuGetAndInc(Holder h, int n) {
        long s = 0;
        for (int k = 0; k < n; k++) s += LFU.getAndIncrement(h);
        return s;
    }

    static long lfuGetAndSet(Holder h, int n) {
        long s = 0;
        for (int k = 0; k < n; k++) s += LFU.getAndSet(h, k);
        return s;
    }

    static final String[] VALS = {"v0", "v1", "v2", "v3"};

    static long rfuGet(Holder h, int n) {
        long s = 0;
        for (int k = 0; k < n; k++) s += RFU.get(h).length();
        return s;
    }

    static long rfuSet(Holder h, int n) {
        for (int k = 0; k < n; k++) RFU.set(h, VALS[k & 3]);
        return h.s.length();
    }

    static long rfuLazySet(Holder h, int n) {
        for (int k = 0; k < n; k++) RFU.lazySet(h, VALS[k & 3]);
        return h.s.length();
    }

    static long rfuCas(Holder h, int n) {
        long s = 0;
        for (int k = 0; k < n; k++) s += RFU.compareAndSet(h, VALS[k & 3], VALS[(k + 1) & 3]) ? 1 : 0;
        return s;
    }

    static long rfuGetAndSet(Holder h, int n) {
        long s = 0;
        for (int k = 0; k < n; k++) s += RFU.getAndSet(h, VALS[k & 3]).length();
        return s;
    }

    interface Kernel {
        long run(Holder h, int n);
    }

    // ── constant-VarHandle rows (wave 3): every access mode on int / long /
    // reference fields and int[] / long[] / Object[] elements, plus the two
    // javac shapes that change the call-site descriptor: a CAST context (a
    // narrower return) and an expression STATEMENT (a `V` return). ─────────

    static final VarHandle VI;
    static final VarHandle VL;
    static final VarHandle VS;
    static final VarHandle VO;
    static final VarHandle VIA = MethodHandles.arrayElementVarHandle(int[].class);
    static final VarHandle VLA = MethodHandles.arrayElementVarHandle(long[].class);
    static final VarHandle VOA = MethodHandles.arrayElementVarHandle(Object[].class);
    static {
        try {
            MethodHandles.Lookup lk = MethodHandles.lookup();
            VI = lk.findVarHandle(Holder.class, "i", int.class);
            VL = lk.findVarHandle(Holder.class, "l", long.class);
            VS = lk.findVarHandle(Holder.class, "s", String.class);
            VO = lk.findVarHandle(Holder.class, "o", Object.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }
    static final int[] IA = new int[8];
    static final long[] LA = new long[8];
    static final Object[] OA = {"a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7"};

    static long vhIntGet(Holder h, int n) { long s = 0; for (int k = 0; k < n; k++) s += (int) VI.get(h); return s; }
    static long vhIntGetAcquire(Holder h, int n) { long s = 0; for (int k = 0; k < n; k++) s += (int) VI.getAcquire(h); return s; }
    static long vhIntGetOpaque(Holder h, int n) { long s = 0; for (int k = 0; k < n; k++) s += (int) VI.getOpaque(h); return s; }
    static long vhIntSet(Holder h, int n) { for (int k = 0; k < n; k++) VI.set(h, k); return h.i; }
    static long vhIntSetVolatile(Holder h, int n) { for (int k = 0; k < n; k++) VI.setVolatile(h, k); return h.i; }
    static long vhIntSetRelease(Holder h, int n) { for (int k = 0; k < n; k++) VI.setRelease(h, k); return h.i; }
    static long vhIntGetAndAdd(Holder h, int n) { long s = 0; for (int k = 0; k < n; k++) s += (int) VI.getAndAdd(h, 3); return s; }
    static long vhIntGetAndAddVoid(Holder h, int n) { for (int k = 0; k < n; k++) VI.getAndAdd(h, 3); return h.i; }
    static long vhIntGetAndSet(Holder h, int n) { long s = 0; for (int k = 0; k < n; k++) s += (int) VI.getAndSet(h, k); return s; }
    static long vhIntCas(Holder h, int n) { long s = 0; for (int k = 0; k < n; k++) s += VI.compareAndSet(h, k, k + 1) ? 1 : 0; return s; }
    static long vhIntCasVoid(Holder h, int n) { for (int k = 0; k < n; k++) VI.compareAndSet(h, k, k + 1); return h.i; }
    static long vhLongGetAcquire(Holder h, int n) { long s = 0; for (int k = 0; k < n; k++) s += (long) VL.getAcquire(h); return s; }
    static long vhLongSetVolatile(Holder h, int n) { for (int k = 0; k < n; k++) VL.setVolatile(h, (long) k); return h.l; }
    static long vhLongGetAndAdd(Holder h, int n) { long s = 0; for (int k = 0; k < n; k++) s += (long) VL.getAndAdd(h, 3L); return s; }
    static long vhLongGetAndSet(Holder h, int n) { long s = 0; for (int k = 0; k < n; k++) s += (long) VL.getAndSet(h, (long) k); return s; }
    static long vhRefGetAcquire(Holder h, int n) { long s = 0; for (int k = 0; k < n; k++) s += ((String) VS.getAcquire(h)).length(); return s; }
    static long vhRefCastGet(Holder h, int n) { long s = 0; for (int k = 0; k < n; k++) s += ((String) VO.getAcquire(h)).length(); return s; }
    static long vhRefObjectGet(Holder h, int n) { long s = 0; for (int k = 0; k < n; k++) s += VO.getAcquire(h).hashCode() & 1; return s; }
    static long vhRefSetVolatile(Holder h, int n) { for (int k = 0; k < n; k++) VS.setVolatile(h, VALS[k & 3]); return h.s.length(); }
    static long vhRefSetRelease(Holder h, int n) { for (int k = 0; k < n; k++) VS.setRelease(h, VALS[k & 3]); return h.s.length(); }
    static long vhRefGetAndSet(Holder h, int n) { long s = 0; for (int k = 0; k < n; k++) s += ((String) VS.getAndSet(h, VALS[k & 3])).length(); return s; }
    static long vhRefCas(Holder h, int n) { long s = 0; for (int k = 0; k < n; k++) s += VS.compareAndSet(h, VALS[k & 3], VALS[(k + 1) & 3]) ? 1 : 0; return s; }
    static long vhIntArrGetAcquire(Holder h, int n) { long s = 0; for (int k = 0; k < n; k++) s += (int) VIA.getAcquire(IA, k & 7); return s; }
    static long vhIntArrSetRelease(Holder h, int n) { for (int k = 0; k < n; k++) VIA.setRelease(IA, k & 7, k); return IA[3]; }
    static long vhIntArrGetAndAdd(Holder h, int n) { long s = 0; for (int k = 0; k < n; k++) s += (int) VIA.getAndAdd(IA, k & 7, 1); return s; }
    static long vhLongArrGetAndSet(Holder h, int n) { long s = 0; for (int k = 0; k < n; k++) s += (long) VLA.getAndSet(LA, k & 7, (long) k); return s; }
    static long vhObjArrGetAcquire(Holder h, int n) { long s = 0; for (int k = 0; k < n; k++) s += ((String) VOA.getAcquire(OA, k & 7)).length(); return s; }

    static final String[] VH_ROWS = {
        "vh-int-get", "vh-int-getacquire", "vh-int-getopaque", "vh-int-set", "vh-int-setvolatile",
        "vh-int-setrelease", "vh-int-getandadd", "vh-int-getandadd-void", "vh-int-getandset", "vh-int-cas",
        "vh-int-cas-void", "vh-long-getacquire", "vh-long-setvolatile", "vh-long-getandadd", "vh-long-getandset",
        "vh-ref-getacquire", "vh-ref-cast-get", "vh-ref-object-get", "vh-ref-setvolatile", "vh-ref-setrelease",
        "vh-ref-getandset", "vh-ref-cas", "vh-iarr-getacquire", "vh-iarr-setrelease", "vh-iarr-getandadd",
        "vh-larr-getandset", "vh-oarr-getacquire",
    };

    static long callVh(int row, Holder h, int n) {
        // Fixed inputs per call: the array rows start from zeroes.
        java.util.Arrays.fill(IA, 0);
        java.util.Arrays.fill(LA, 0L);
        switch (row) {
            case 0: return vhIntGet(h, n);
            case 1: return vhIntGetAcquire(h, n);
            case 2: return vhIntGetOpaque(h, n);
            case 3: return vhIntSet(h, n);
            case 4: return vhIntSetVolatile(h, n);
            case 5: return vhIntSetRelease(h, n);
            case 6: return vhIntGetAndAdd(h, n);
            case 7: return vhIntGetAndAddVoid(h, n);
            case 8: return vhIntGetAndSet(h, n);
            case 9: return vhIntCas(h, n);
            case 10: return vhIntCasVoid(h, n);
            case 11: return vhLongGetAcquire(h, n);
            case 12: return vhLongSetVolatile(h, n);
            case 13: return vhLongGetAndAdd(h, n);
            case 14: return vhLongGetAndSet(h, n);
            case 15: return vhRefGetAcquire(h, n);
            case 16: return vhRefCastGet(h, n);
            case 17: return vhRefObjectGet(h, n);
            case 18: return vhRefSetVolatile(h, n);
            case 19: return vhRefSetRelease(h, n);
            case 20: return vhRefGetAndSet(h, n);
            case 21: return vhRefCas(h, n);
            case 22: return vhIntArrGetAcquire(h, n);
            case 23: return vhIntArrSetRelease(h, n);
            case 24: return vhIntArrGetAndAdd(h, n);
            case 25: return vhLongArrGetAndSet(h, n);
            default: return vhObjArrGetAcquire(h, n);
        }
    }

    /** The cast context's failure: a non-String in the Object field through
     *  {@code (String) VO.getAcquire(h)} is the invoker's ClassCastException. */
    static void vhCastFacts(String phase) {
        Holder h = fresh();
        h.o = Integer.valueOf(7);
        try {
            String v = (String) VO.getAcquire(h);
            fact(phase + " vh-cast-get-wrong-class", "no-throw " + v);
        } catch (Throwable t) {
            fact(phase + " vh-cast-get-wrong-class", t.getClass().getName());
        }
        h.o = null;
        fact(phase + " vh-cast-get-null", String.valueOf((String) VO.getAcquire(h)));
    }

    static final String[] ROWS = {
        "ifu-get", "ifu-set", "ifu-lazyset", "ifu-cas", "ifu-getandadd", "ifu-getandinc", "ifu-getandset",
        "lfu-get", "lfu-set", "lfu-lazyset", "lfu-cas", "lfu-getandadd", "lfu-getandinc", "lfu-getandset",
        "rfu-get", "rfu-set", "rfu-lazyset", "rfu-cas", "rfu-getandset",
    };

    static long call(int row, Holder h, int n) {
        switch (row) {
            case 0: return ifuGet(h, n);
            case 1: return ifuSet(h, n);
            case 2: return ifuLazySet(h, n);
            case 3: return ifuCas(h, n);
            case 4: return ifuGetAndAdd(h, n);
            case 5: return ifuGetAndInc(h, n);
            case 6: return ifuGetAndSet(h, n);
            case 7: return lfuGet(h, n);
            case 8: return lfuSet(h, n);
            case 9: return lfuLazySet(h, n);
            case 10: return lfuCas(h, n);
            case 11: return lfuGetAndAdd(h, n);
            case 12: return lfuGetAndInc(h, n);
            case 13: return lfuGetAndSet(h, n);
            case 14: return rfuGet(h, n);
            case 15: return rfuSet(h, n);
            case 16: return rfuLazySet(h, n);
            case 17: return rfuCas(h, n);
            default: return rfuGetAndSet(h, n);
        }
    }

    static long sink;

    static Holder fresh() {
        Holder h = new Holder();
        h.s = "v0";
        h.o = "o0";
        return h;
    }

    static void warm() {
        for (int w = 0; w < WARM; w++) {
            for (int row = 0; row < ROWS.length; row++) {
                sink += call(row, fresh(), N / 10);
            }
            for (int row = 0; row < VH_ROWS.length; row++) {
                sink += callVh(row, fresh(), N / 10);
            }
        }
    }

    static void bench() {
        for (int row = 0; row < ROWS.length; row++) {
            List<Long> times = new ArrayList<>();
            for (int r = 0; r < REPS; r++) {
                Holder h = fresh();
                long t0 = System.nanoTime();
                sink += call(row, h, N);
                long t1 = System.nanoTime();
                times.add(t1 - t0);
            }
            Collections.sort(times);
            System.out.println("time " + ROWS[row] + " " + (times.get(REPS / 2) / N) + " ns/op");
        }
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
            factoryFacts();
            facts("cold");
            vhCastFacts("cold");
        }
        warm();
        if (conf) {
            facts("warm");
            // The kernels' own answers on fixed inputs, after warm-up.
            for (int row = 0; row < ROWS.length; row++) {
                fact("kernel " + ROWS[row], call(row, fresh(), 1000));
            }
            for (int row = 0; row < VH_ROWS.length; row++) {
                fact("kernel " + VH_ROWS[row], callVh(row, fresh(), 1000));
            }
            vhCastFacts("warm");
        }
        if (timing) {
            bench();
        }
        if (sink == 42) {
            System.out.println("unlikely");
        }
    }
}

/** A different top-level class: not a nestmate of R21UpdaterRows. */
class R21UpdaterOther {
    private volatile int p;
    volatile int q;
}
