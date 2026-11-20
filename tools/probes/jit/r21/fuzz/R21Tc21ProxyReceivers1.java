// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 tc21 (typecheck page closure): lambda-proxy receivers at
// compiled `instanceof` / `checkcast` sites. A lambda's class is a VM-spun
// proxy (class id >= 0x8000_0000, absent from the class store); the helper
// entry for such a receiver used to take the class manager's read lock to
// look for a display that can never be published
// (`vh_const_ensure_display`, fixed by tc21). The answers must not change.
//
// Expected output is DERIVED from JLS 15.20.2 / 15.16 and JDK 25 behaviour,
// NOT measured (no JDK 25 HotSpot on the authoring host):
//   * a lambda is an instance of its functional interface only;
//   * a plain (non-serializable) lambda is not `java.io.Serializable`;
//   * per pass over the 5 objects { F1, F2, "s", Integer 3, Runnable lambda }:
//       instanceof Fn -> 2 (F1, F2), Runnable -> 10 (one lambda),
//       Serializable -> 200 (String and Integer), so 212 per pass;
//   * the cast loop adds F1.ap(0) + F2.ap(0) = 1 + 2 = 3 per pass;
//   * `(Fn) runnableLambda` throws ClassCastException once per pass.
// With REPS = 20_000 passes: inst = 4_240_000, cast = 60_000, cce = 20_000.
//
// Run:
//   javac -d out R21Tc21ProxyReceivers1.java
//   java -Xmx256m -cp out R21Tc21ProxyReceivers1                          # HotSpot 25 (when available)
//   cratonvm -Xmx256m -cp out R21Tc21ProxyReceivers1
//   cratonvm --compatible -Xmx256m -cp out R21Tc21ProxyReceivers1
//   CRATONVM_JIT_IR_INSTANCEOF_PROXY_WAYS=0 cratonvm -Xmx256m -cp out R21Tc21ProxyReceivers1
//   CRATONVM_C2_ACCEPT=never cratonvm -Xmx256m -cp out R21Tc21ProxyReceivers1   # single-pass bodies
// PASS: every line below reads "OK" on every arm and in both modes.

public class R21Tc21ProxyReceivers1 {
    static final int REPS = 20_000;
    static final long EXPECT_INST = 4_240_000L;
    static final long EXPECT_CAST = 60_000L;
    static final long EXPECT_CCE = 20_000L;

    interface Fn {
        int ap(int x);
    }

    static final Fn F1 = x -> x + 1;
    static final Fn F2 = x -> x + 2;

    static Object[] objects() {
        Runnable r = () -> { };
        return new Object[] {F1, F2, "s", Integer.valueOf(3), r};
    }

    static long kInst(Object[] a, int reps) {
        long n = 0;
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < a.length; i++) {
                Object o = a[i];
                if (o instanceof Fn) {
                    n += 1;
                }
                if (o instanceof Runnable) {
                    n += 10;
                }
                if (o instanceof java.io.Serializable) {
                    n += 100;
                }
            }
        }
        return n;
    }

    static long kCast(Object[] a, int reps) {
        long n = 0;
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < a.length; i++) {
                Object o = a[i];
                if (o instanceof Fn) {
                    n += ((Fn) o).ap(0);
                }
            }
        }
        return n;
    }

    static long kCce(Object[] a, int reps) {
        long n = 0;
        for (int r = 0; r < reps; r++) {
            try {
                Fn f = (Fn) a[4];
                n -= f.ap(0);
            } catch (ClassCastException e) {
                n += 1;
            }
        }
        return n;
    }

    static void check(String row, long got, long expect) {
        System.out.println("fact " + row + " " + (got == expect ? "OK" : "FAIL got=" + got + " expect=" + expect));
    }

    public static void main(String[] args) {
        Object[] a = objects();
        // Cold, warm and hot: the same answers whichever tier runs the kernel.
        for (int round = 0; round < 5; round++) {
            check("inst." + round, kInst(a, REPS), EXPECT_INST);
            check("cast." + round, kCast(a, REPS), EXPECT_CAST);
            check("cce." + round, kCce(a, REPS), EXPECT_CCE);
        }
    }
}
