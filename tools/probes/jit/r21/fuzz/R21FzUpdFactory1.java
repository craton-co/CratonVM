// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: the `--compatible` field-updater factory
// and value checks (`CRATONVM_FIELD_UPDATER_JDK_RULES`, `CRATONVM_ARFU_VALUE_CHECK`,
// both default ON; `native-builtins/src/atomic_updater.rs`).
//
// Risk targeted: the `$RustJvmImpl` natives must refuse exactly what the JDK
// factories refuse, with the same exception class and message:
//   * a non-volatile field ("Must be volatile type");
//   * the wrong primitive type (int updater on a long field, long on int,
//     int on Integer);
//   * a static field;
//   * a missing field (RuntimeException wrapping NoSuchFieldException);
//   * a reference updater whose vclass does not match the field type, a
//     primitive vclass, a vclass that is a supertype / subtype of the field;
//   * a private field of ANOTHER class (access: RuntimeException wrapping
//     IllegalAccessException);
//   * a final field;
//   * null arguments.
// Then the updaters that ARE legal, used on hot paths (so the compiled
// callers of the factories and of the accessors are exercised), and a
// wrong-class object at a hot site. Factory calls are repeated many times
// (the factory method itself gets compiled).
//
// Run:
//   javac -d out R21FzUpdFactory1.java
//   java -Xmx256m -cp out R21FzUpdFactory1                                # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzUpdFactory1
//   cratonvm --compatible -Xmx256m -cp out R21FzUpdFactory1
//   CRATONVM_FIELD_UPDATER_JDK_RULES=0 CRATONVM_ARFU_VALUE_CHECK=0 cratonvm --compatible ... (kill control)
//   cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm (the kill arm may differ: that is the defect it restores).

import java.util.concurrent.atomic.AtomicIntegerFieldUpdater;
import java.util.concurrent.atomic.AtomicLongFieldUpdater;
import java.util.concurrent.atomic.AtomicReferenceFieldUpdater;
import java.util.function.Supplier;

public class R21FzUpdFactory1 {
    static class T {
        volatile int vi;
        int plain;
        volatile long vl;
        volatile Integer boxed;
        static volatile int st;
        final int fin = 1;
        volatile CharSequence cs;
        volatile String str;
        volatile Object obj;
    }

    static final class Other {
        private volatile int secret;
        private volatile String secretRef;
    }

    static String probe(Supplier<?> s) {
        try {
            Object u = s.get();
            return u == null ? "null" : "ok";
        } catch (Throwable e) {
            String m = e.getClass().getName() + ":" + e.getMessage();
            if (e.getCause() != null) {
                m += " <- " + e.getCause().getClass().getName();
            }
            return m;
        }
    }

    static String[] factories() {
        return new String[] {
            probe(() -> AtomicIntegerFieldUpdater.newUpdater(T.class, "vi")),
            probe(() -> AtomicIntegerFieldUpdater.newUpdater(T.class, "plain")),
            probe(() -> AtomicIntegerFieldUpdater.newUpdater(T.class, "vl")),
            probe(() -> AtomicIntegerFieldUpdater.newUpdater(T.class, "boxed")),
            probe(() -> AtomicIntegerFieldUpdater.newUpdater(T.class, "st")),
            probe(() -> AtomicIntegerFieldUpdater.newUpdater(T.class, "fin")),
            probe(() -> AtomicIntegerFieldUpdater.newUpdater(T.class, "missing")),
            probe(() -> AtomicIntegerFieldUpdater.newUpdater(Other.class, "secret")),
            probe(() -> AtomicIntegerFieldUpdater.newUpdater(null, "vi")),
            probe(() -> AtomicIntegerFieldUpdater.newUpdater(T.class, null)),
            probe(() -> AtomicLongFieldUpdater.newUpdater(T.class, "vl")),
            probe(() -> AtomicLongFieldUpdater.newUpdater(T.class, "vi")),
            probe(() -> AtomicLongFieldUpdater.newUpdater(T.class, "plain")),
            probe(() -> AtomicReferenceFieldUpdater.newUpdater(T.class, Integer.class, "boxed")),
            probe(() -> AtomicReferenceFieldUpdater.newUpdater(T.class, Object.class, "boxed")),
            probe(() -> AtomicReferenceFieldUpdater.newUpdater(T.class, Number.class, "boxed")),
            probe(() -> AtomicReferenceFieldUpdater.newUpdater(T.class, String.class, "cs")),
            probe(() -> AtomicReferenceFieldUpdater.newUpdater(T.class, CharSequence.class, "cs")),
            probe(() -> AtomicReferenceFieldUpdater.newUpdater(T.class, CharSequence.class, "str")),
            probe(() -> AtomicReferenceFieldUpdater.newUpdater(T.class, int.class, "vi")),
            probe(() -> AtomicReferenceFieldUpdater.newUpdater(T.class, Object.class, "vi")),
            probe(() -> AtomicReferenceFieldUpdater.newUpdater(T.class, String.class, "obj")),
            probe(() -> AtomicReferenceFieldUpdater.newUpdater(Other.class, String.class, "secretRef")),
            probe(() -> AtomicReferenceFieldUpdater.newUpdater(T.class, null, "obj")),
        };
    }

    static final AtomicReferenceFieldUpdater<T, CharSequence> CS =
            AtomicReferenceFieldUpdater.newUpdater(T.class, CharSequence.class, "cs");
    static final AtomicReferenceFieldUpdater<T, Object> OBJ =
            AtomicReferenceFieldUpdater.newUpdater(T.class, Object.class, "obj");
    static final AtomicIntegerFieldUpdater<T> VI = AtomicIntegerFieldUpdater.newUpdater(T.class, "vi");

    @SuppressWarnings({"unchecked", "rawtypes"})
    static String values(T t) {
        AtomicReferenceFieldUpdater raw = CS;
        StringBuilder sb = new StringBuilder();
        for (Object v : new Object[] {"s", new StringBuilder("sb"), null, 5, new Object()}) {
            try {
                raw.set(t, v);
                sb.append(" set:").append(t.cs == null ? "null" : t.cs.toString());
            } catch (ClassCastException e) {
                sb.append(" CCE:").append(e.getMessage());
            }
        }
        try {
            sb.append(" lazy:");
            raw.lazySet(t, 9);
            sb.append(t.cs);
        } catch (ClassCastException e) {
            sb.append("CCE:").append(e.getMessage());
        }
        try {
            sb.append(" acc:");
            raw.accumulateAndGet(t, "x", (a, b) -> 7);
            sb.append(t.cs);
        } catch (ClassCastException e) {
            sb.append("CCE:").append(e.getMessage());
        }
        return sb.toString();
    }

    static void rows(String tag) {
        String[] f = factories();
        for (int i = 0; i < f.length; i++) {
            System.out.println("row " + tag + " f" + i + " " + f[i]);
        }
        System.out.println("row " + tag + " values" + values(new T()));
    }

    public static void main(String[] args) {
        rows("cold");
        long h = 0;
        T t = new T();
        for (int r = 0; r < 3_000; r++) {
            for (String s : factories()) {
                h = h * 31 + s.length();
            }
        }
        for (int r = 0; r < 100_000; r++) {
            CS.set(t, (r & 1) == 0 ? "a" : new StringBuilder("b"));
            OBJ.set(t, r);
            h = h * 31 + CS.get(t).length() + VI.incrementAndGet(t) + (Integer) OBJ.get(t);
        }
        System.out.println("warm hash " + h + " vi=" + t.vi);
        rows("hot");
        rows("after");
    }
}
