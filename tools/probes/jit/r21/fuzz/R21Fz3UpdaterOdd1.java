// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 13, lane fuzz3: Atomic{Integer,Long,Reference}FieldUpdater
// on odd fields, made and then driven hot.
//
// Risk targeted (waves 6-10, `native-builtins/src/atomic_updater.rs` and the
// JIT unsafe field-cell arm): wave 9 dropped the up-front field-name screen
// (any name is a getDeclaredField miss: RuntimeException(NoSuchFieldException));
// wave 7's null vclass / null fieldName order; `require_target` name order and
// the declaring-class resolver; "unsafe field-cell refusals settle only fixed
// reasons, per own key, per epoch". Odd but legal names (`$`, `_u`, `x$y`, a
// non-ASCII name), a field that SHADOWS a superclass field of the same name
// (two updaters, two offsets, one name), a field only declared in the
// superclass (a miss through the subclass), wrong types, non-volatile, static,
// private (a nestmate of this class), and the same field name in an unrelated
// class driven through one polymorphic call site. Then hot loops over T and its
// subclass U, a late never-loaded subclass V (deopt trigger), and the final
// field values read directly. Every printed string is ASCII-escaped.
//
// Run:
//   javac -d out R21Fz3UpdaterOdd1.java
//   java -Xmx256m -cp out R21Fz3UpdaterOdd1                     # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz3UpdaterOdd1                 # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz3UpdaterOdd1
//   cratonvm --nojit -Xmx256m -cp out R21Fz3UpdaterOdd1
//   CRATONVM_JIT_UNSAFE_FIELD_CELLS_BYTE_OFFSETS=1 CRATONVM_JIT_UNSAFE_VOLATILE_PUBLISHES_FIELD_CELL=1 cratonvm ...
//   CRATONVM_FIELD_UPDATER_JDK_RULES=0 cratonvm ...   # kill-switch control: `factory` odd-name rows DIFF
// HotSpot expectation: every line identical on every arm.

import java.util.concurrent.atomic.AtomicIntegerFieldUpdater;
import java.util.concurrent.atomic.AtomicLongFieldUpdater;
import java.util.concurrent.atomic.AtomicReferenceFieldUpdater;
import java.util.function.Supplier;

public class R21Fz3UpdaterOdd1 {
    static final int HOT = 60_000;

    static class Base {
        volatile int inherited;
        volatile long baseOnly;
        volatile Object refb;
    }

    static class T extends Base {
        volatile int $;
        volatile int _u;
        volatile int x$y;
        volatile int \u00fcber;
        volatile int inherited;
        volatile long l;
        volatile Integer boxed;
        int plain;
        static volatile int st;
        private volatile int priv;
        volatile String s;
        volatile CharSequence cs;
        volatile int[] arr;
    }

    static class U extends T {
    }

    static class V extends U {
        volatile int $;
    }

    static class Other {
        volatile int $;
    }

    static String esc(String s) {
        if (s == null) {
            return "null";
        }
        StringBuilder sb = new StringBuilder();
        int max = Math.min(s.length(), 90);
        for (int i = 0; i < max; i++) {
            char c = s.charAt(i);
            if (c >= 0x20 && c < 0x7f) {
                sb.append(c);
            } else {
                sb.append(String.format("\\u%04x", (int) c));
            }
        }
        if (s.length() > max) {
            sb.append("...(").append(s.length()).append(')');
        }
        return sb.toString();
    }

    static String describe(Throwable e) {
        String r = e.getClass().getName() + " msg=" + esc(e.getMessage());
        Throwable c = e.getCause();
        return c == null ? r : r + " cause=" + c.getClass().getName() + " msg=" + esc(c.getMessage());
    }

    static <X> X fact(String id, Supplier<X> s) {
        try {
            X x = s.get();
            System.out.println("factory " + id + " ok");
            return x;
        } catch (Throwable e) {
            System.out.println("factory " + id + " " + describe(e));
            return null;
        }
    }

    static String repeat(char c, int n) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < n; i++) {
            sb.append(c);
        }
        return sb.toString();
    }

    static AtomicIntegerFieldUpdater<T> uDollar;
    static AtomicIntegerFieldUpdater<T> uUnder;
    static AtomicIntegerFieldUpdater<T> uXy;
    static AtomicIntegerFieldUpdater<T> uUber;
    static AtomicIntegerFieldUpdater<T> uInhT;
    static AtomicIntegerFieldUpdater<Base> uInhBase;
    static AtomicLongFieldUpdater<Base> uBaseOnly;
    static AtomicLongFieldUpdater<T> uL;
    static AtomicIntegerFieldUpdater<T> uPriv;
    static AtomicIntegerFieldUpdater<V> uVDollar;
    static AtomicIntegerFieldUpdater<Other> uOther;
    static AtomicReferenceFieldUpdater<T, String> uS;
    static AtomicReferenceFieldUpdater<T, CharSequence> uCs;
    static AtomicReferenceFieldUpdater<T, int[]> uArr;
    static AtomicReferenceFieldUpdater<T, Integer> uBoxed;
    static AtomicReferenceFieldUpdater<Base, Object> uRefb;

    static void factories() {
        uDollar = fact("int-dollar", () -> AtomicIntegerFieldUpdater.newUpdater(T.class, "$"));
        uUnder = fact("int-underscore", () -> AtomicIntegerFieldUpdater.newUpdater(T.class, "_u"));
        uXy = fact("int-x$y", () -> AtomicIntegerFieldUpdater.newUpdater(T.class, "x$y"));
        uUber = fact("int-nonascii", () -> AtomicIntegerFieldUpdater.newUpdater(T.class, "\u00fcber"));
        fact("int-nonascii-decomposed", () -> AtomicIntegerFieldUpdater.newUpdater(T.class, "u\u0308ber"));
        uInhT = fact("int-shadowing", () -> AtomicIntegerFieldUpdater.newUpdater(T.class, "inherited"));
        uInhBase = fact("int-shadowed", () -> AtomicIntegerFieldUpdater.newUpdater(Base.class, "inherited"));
        fact("long-superclass-only-via-sub", () -> AtomicLongFieldUpdater.newUpdater(T.class, "baseOnly"));
        uBaseOnly = fact("long-superclass-only", () -> AtomicLongFieldUpdater.newUpdater(Base.class, "baseOnly"));
        uL = fact("long-l", () -> AtomicLongFieldUpdater.newUpdater(T.class, "l"));
        uPriv = fact("int-private-nestmate", () -> AtomicIntegerFieldUpdater.newUpdater(T.class, "priv"));
        uVDollar = fact("int-dollar-subclass-shadow", () -> AtomicIntegerFieldUpdater.newUpdater(V.class, "$"));
        uOther = fact("int-dollar-other", () -> AtomicIntegerFieldUpdater.newUpdater(Other.class, "$"));
        fact("int-on-long", () -> AtomicIntegerFieldUpdater.newUpdater(T.class, "l"));
        fact("long-on-int", () -> AtomicLongFieldUpdater.newUpdater(T.class, "$"));
        fact("int-on-boxed", () -> AtomicIntegerFieldUpdater.newUpdater(T.class, "boxed"));
        fact("int-plain", () -> AtomicIntegerFieldUpdater.newUpdater(T.class, "plain"));
        fact("int-static", () -> AtomicIntegerFieldUpdater.newUpdater(T.class, "st"));
        fact("int-empty", () -> AtomicIntegerFieldUpdater.newUpdater(T.class, ""));
        fact("int-slash", () -> AtomicIntegerFieldUpdater.newUpdater(T.class, "a/b"));
        fact("int-dot", () -> AtomicIntegerFieldUpdater.newUpdater(T.class, "x.y"));
        fact("int-semicolon", () -> AtomicIntegerFieldUpdater.newUpdater(T.class, "a;b"));
        fact("int-bracket", () -> AtomicIntegerFieldUpdater.newUpdater(T.class, "[I"));
        fact("int-lt", () -> AtomicIntegerFieldUpdater.newUpdater(T.class, "<init>"));
        fact("int-space-dollar", () -> AtomicIntegerFieldUpdater.newUpdater(T.class, " $"));
        fact("int-nul", () -> AtomicIntegerFieldUpdater.newUpdater(T.class, "$\u0000"));
        fact("int-long-name", () -> AtomicIntegerFieldUpdater.newUpdater(T.class, repeat('q', 70_000)));
        fact("long-empty", () -> AtomicLongFieldUpdater.newUpdater(T.class, ""));
        fact("ref-empty", () -> AtomicReferenceFieldUpdater.newUpdater(T.class, String.class, ""));
        fact("ref-null-vclass-dollar", () -> AtomicReferenceFieldUpdater.newUpdater(T.class, null, "$"));
        fact("ref-null-vclass-empty", () -> AtomicReferenceFieldUpdater.newUpdater(T.class, null, ""));
        fact("ref-null-name-empty-class", () -> AtomicReferenceFieldUpdater.newUpdater(T.class, String.class, null));
        fact("int-null-tclass", () -> AtomicIntegerFieldUpdater.newUpdater(null, "$"));
        uS = fact("ref-s", () -> AtomicReferenceFieldUpdater.newUpdater(T.class, String.class, "s"));
        fact("ref-s-as-object", () -> AtomicReferenceFieldUpdater.newUpdater(T.class, Object.class, "s"));
        fact("ref-s-as-charseq", () -> AtomicReferenceFieldUpdater.newUpdater(T.class, CharSequence.class, "s"));
        uCs = fact("ref-cs", () -> AtomicReferenceFieldUpdater.newUpdater(T.class, CharSequence.class, "cs"));
        uArr = fact("ref-int-array", () -> AtomicReferenceFieldUpdater.newUpdater(T.class, int[].class, "arr"));
        fact("ref-int-array-as-long-array",
                () -> AtomicReferenceFieldUpdater.newUpdater(T.class, long[].class, "arr"));
        uBoxed = fact("ref-boxed", () -> AtomicReferenceFieldUpdater.newUpdater(T.class, Integer.class, "boxed"));
        fact("ref-on-int", () -> AtomicReferenceFieldUpdater.newUpdater(T.class, int.class, "$"));
        uRefb = fact("ref-superclass", () -> AtomicReferenceFieldUpdater.newUpdater(Base.class, Object.class, "refb"));
        fact("ref-superclass-via-sub", () -> AtomicReferenceFieldUpdater.newUpdater(T.class, Object.class, "refb"));
    }

    @SuppressWarnings({"unchecked", "rawtypes"})
    static String misuse() {
        StringBuilder sb = new StringBuilder();
        Object[] probes = {new Other(), null, new Base()};
        String[] names = {"other", "null", "base"};
        for (int i = 0; i < probes.length; i++) {
            try {
                ((AtomicIntegerFieldUpdater) uDollar).incrementAndGet(probes[i]);
                sb.append(" dollar-").append(names[i]).append("=ok");
            } catch (Throwable e) {
                sb.append(" dollar-").append(names[i]).append('=').append(describe(e));
            }
        }
        try {
            ((AtomicIntegerFieldUpdater) uOther).incrementAndGet(new T());
            sb.append(" other-on-t=ok");
        } catch (Throwable e) {
            sb.append(" other-on-t=").append(describe(e));
        }
        try {
            ((AtomicReferenceFieldUpdater) uS).set(new T(), Integer.valueOf(3));
            sb.append(" s-wrong-value=ok");
        } catch (Throwable e) {
            sb.append(" s-wrong-value=").append(describe(e));
        }
        try {
            ((AtomicReferenceFieldUpdater) uCs).set(new U(), new StringBuilder("sb"));
            sb.append(" cs-builder=ok");
        } catch (Throwable e) {
            sb.append(" cs-builder=").append(describe(e));
        }
        try {
            ((AtomicIntegerFieldUpdater) uInhBase).incrementAndGet(new V());
            sb.append(" base-inh-on-v=ok");
        } catch (Throwable e) {
            sb.append(" base-inh-on-v=").append(describe(e));
        }
        return sb.toString();
    }

    static final AtomicIntegerFieldUpdater<?>[] POLY = new AtomicIntegerFieldUpdater<?>[2];
    static Object[] polyRecv;

    @SuppressWarnings({"unchecked", "rawtypes"})
    static long kernel(T[] rs, int n) {
        long h = 0;
        for (int i = 0; i < n; i++) {
            T t = rs[i % rs.length];
            h += uDollar.incrementAndGet(t);
            h += uUnder.getAndAdd(t, 3);
            uXy.compareAndSet(t, t.x$y, t.x$y + (i & 7));
            h += uUber.accumulateAndGet(t, i, (a, b) -> a ^ b);
            h += uInhT.updateAndGet(t, v -> v + 2);
            h += uInhBase.getAndIncrement(t);
            h += uBaseOnly.addAndGet(t, i);
            h += uL.getAndAdd(t, -i);
            h += uPriv == null ? 0 : uPriv.incrementAndGet(t);
            String prev = uS.getAndSet(t, (i & 1) == 0 ? "e" : "o");
            h += prev == null ? 0 : prev.length();
            uCs.lazySet(t, (i & 3) == 0 ? "four" : "x");
            int[] arr = uArr.get(t);
            if (arr == null || arr.length != (i & 3)) {
                uArr.set(t, new int[i & 3]);
            }
            uBoxed.accumulateAndGet(t, i & 15, (a, b) -> a == null ? b : Integer.valueOf(a + b));
            uRefb.compareAndSet(t, null, t);
            int k = i & 1;
            h += ((AtomicIntegerFieldUpdater) POLY[k]).incrementAndGet(polyRecv[k]);
        }
        return h;
    }

    static String values(String tag, T[] rs) {
        StringBuilder sb = new StringBuilder("values ").append(tag);
        for (T t : rs) {
            Base b = t;
            sb.append(" [").append(t.getClass().getSimpleName()).append(' ').append(t.$).append(',').append(t._u)
                    .append(',').append(t.x$y).append(',').append(t.\u00fcber).append(',').append(t.inherited)
                    .append(',').append(b.inherited).append(',').append(b.baseOnly).append(',').append(t.l)
                    .append(',').append(t.priv).append(',').append(t.s).append(',').append(t.cs).append(',')
                    .append(t.arr == null ? -1 : t.arr.length).append(',').append(t.boxed).append(',')
                    .append(b.refb == t).append(']');
            if (t instanceof V v) {
                sb.append(" vOwnDollar=").append(v.$);
            }
        }
        return sb.toString();
    }

    public static void main(String[] args) {
        factories();
        System.out.println("misuse cold" + misuse());
        Other other = new Other();
        T polyT = new T();
        POLY[0] = uDollar;
        POLY[1] = uOther;
        polyRecv = new Object[] {polyT, other};

        T[] rs = {new T(), new U(), new T()};
        System.out.println("kernel cold " + kernel(rs, 100));
        System.out.println(values("cold", rs));
        System.out.println("kernel hot " + kernel(rs, HOT));
        System.out.println("kernel hot2 " + kernel(rs, HOT));
        System.out.println(values("hot", rs));
        T[] late = {new U(), new V(), new T()};
        System.out.println("kernel late " + kernel(late, HOT));
        System.out.println(values("late", late));
        System.out.println("poly t$=" + polyT.$ + " other$=" + other.$);
        System.out.println("misuse hot" + misuse());
        // A late factory (after the kernels compiled) for an already-driven field.
        AtomicIntegerFieldUpdater<T> again = fact("int-dollar-again", () -> AtomicIntegerFieldUpdater.newUpdater(T.class, "$"));
        V v = (V) late[1];
        int vOwn = uVDollar == null ? -1 : uVDollar.addAndGet(v, 5);
        System.out.println("again " + (again == null ? "null" : again.get(rs[0]) + " " + again.incrementAndGet(v))
                + " vOwn=" + vOwn + " " + v.$ + " vAsT=" + ((T) v).$);
    }
}
