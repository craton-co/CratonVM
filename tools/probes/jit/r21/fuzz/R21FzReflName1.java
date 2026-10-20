// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: the per-thread `Class.getName()` mirror memo
// (`CRATONVM_CLASS_GET_NAME_MIRROR_MEMO`, `native-builtins/src/lang_class.rs`
// `CLASS_GET_NAME_MIRROR_MEMO`: 64 direct-mapped lines keyed by VM identity,
// the GC address epoch and the mirror ADDRESS, guarded by the mirror's `name`
// slot word).
//
// Risk targeted: a memo keyed by address must never answer for another mirror
// after a collection moves mirrors (epoch change) or after two mirrors map to
// one line; it must answer arrays (every dimension), primitive mirrors,
// `void.class`, nested / local / anonymous classes, and keep identity
// (`c.getName() == c.getName()`, true on HotSpot, which caches `Class.name`).
// Rows: names of 80+ mirrors (more than the 64 lines, in an order that
// collides), cycled hot on two threads, with System.gc() and allocation churn
// between passes; also getSimpleName / getTypeName / getCanonicalName /
// getPackageName answers for the same mirrors (they read `getName`).
//
// Run:
//   javac -d out R21FzReflName1.java
//   java -Xmx256m -cp out R21FzReflName1                                  # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzReflName1
//   CRATONVM_CLASS_GET_NAME_MIRROR_MEMO=1 cratonvm -Xmx256m -cp out R21FzReflName1
//   the same with -XX:+UseG1GC / -XX:+UseZGC (moving collectors)
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

import java.util.ArrayList;
import java.util.List;

public class R21FzReflName1 {
    static class Nested {
        class Inner {
        }
    }

    interface Marker {
    }

    enum Color {
        RED {
            @Override
            int k() {
                return 1;
            }
        },
        GREEN;

        int k() {
            return 0;
        }
    }

    record Rec(int a) {
    }

    static Class<?>[] mirrors() {
        class Local {
        }
        Object anon = new Object() {
        };
        List<Class<?>> l = new ArrayList<>();
        Class<?>[] base = {int.class, long.class, boolean.class, byte.class, char.class, short.class, float.class,
            double.class, void.class, Object.class, String.class, Integer.class, Nested.class, Nested.Inner.class,
            Marker.class, Color.class, Color.RED.getClass(), Rec.class, Local.class, anon.getClass(),
            R21FzReflName1.class, List.class, java.util.Map.Entry.class, Thread.State.class};
        for (Class<?> c : base) {
            l.add(c);
            if (c != void.class) {
                Class<?> a = c.arrayType();
                l.add(a);
                l.add(a.arrayType());
                l.add(a.arrayType().arrayType());
            }
        }
        return l.toArray(new Class<?>[0]);
    }

    static String names(Class<?> c) {
        return c.getName() + "|" + c.getSimpleName() + "|" + c.getTypeName() + "|" + c.getCanonicalName() + "|"
                + c.getPackageName();
    }

    static int churn(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            Object[] junk = new Object[8];
            junk[i & 7] = junk;
            s += junk.length;
        }
        return s;
    }

    static long pass(Class<?>[] ms, int reps) {
        long h = 0;
        int sameFails = 0;
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < ms.length; i++) {
                Class<?> c = ms[(i * 37 + r) % ms.length];
                String n = c.getName();
                if (n != c.getName()) {
                    sameFails++;
                }
                h = h * 31 + n.hashCode();
            }
        }
        return h * 7 + sameFails;
    }

    public static void main(String[] args) throws Exception {
        Class<?>[] ms = mirrors();
        System.out.println("count " + ms.length);
        for (int i = 0; i < ms.length; i++) {
            System.out.println("row cold " + i + " " + names(ms[i]));
        }
        System.out.println("pass0 " + pass(ms, 3000));
        churn(500_000);
        System.gc();
        System.out.println("pass1 " + pass(ms, 3000));
        long[] th = new long[2];
        Thread t1 = new Thread(() -> th[0] = pass(ms, 2000));
        Thread t2 = new Thread(() -> {
            churn(200_000);
            System.gc();
            th[1] = pass(ms, 2000);
        });
        t1.start();
        t2.start();
        t1.join();
        t2.join();
        System.out.println("threads " + th[0] + " " + th[1]);
        churn(500_000);
        System.gc();
        for (int i = 0; i < ms.length; i++) {
            System.out.println("row hot " + i + " " + names(ms[i]));
        }
        System.out.println("pass2 " + pass(ms, 3000));
    }
}
