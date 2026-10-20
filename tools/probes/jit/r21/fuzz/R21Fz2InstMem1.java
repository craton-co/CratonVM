// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: TYPECHECK21-1 part A, the header-only
// `instanceof` (`CRATONVM_JIT_IR_INSTANCEOF_HEADER_READ`: `MemAccess::HeaderRead`,
// reads `AliasClass::None`, writes nothing, still a safepoint) and the
// off-chain form (`CRATONVM_JIT_IR_INSTANCEOF_OFF_CHAIN`: the builder no longer
// advances `self.mem` past it).
//
// Risk targeted: loads and stores around a test that is no longer a memory
// barrier. Every row stores, tests, then loads, and the answer depends on
// the store being seen: a field store then a load across the test, the same
// field through an alias (two refs, one object), an array element, a static,
// a field of the TESTED object itself (`h.ref = x; h.ref instanceof T`), a
// store inside the taken arm, a loop that stores and tests every trip (LICM's
// restricted read hoist now proceeds in such loops: a hoisted load would
// read the pre-loop value), a call in the loop that writes the field
// (opaque), and a volatile spin that must terminate. Also DCE of an unread
// off-chain test: its operand's own exceptions (a checkcast CCE, an aaload
// AIOOBE, a getfield NPE) must still be raised. Cold, hot (200k trips),
// after a deopt (a new subclass reaches the tests), again.
//
// Run:
//   javac -d out R21Fz2InstMem1.java
//   java -Xmx256m -cp out R21Fz2InstMem1                                 # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz2InstMem1
//   CRATONVM_JIT_IR_INSTANCEOF_HEADER_READ=1 cratonvm -Xmx256m -cp out R21Fz2InstMem1
//   CRATONVM_JIT_IR_INSTANCEOF_HEADER_READ=1 CRATONVM_JIT_IR_INSTANCEOF_OFF_CHAIN=1 cratonvm -Xmx256m -cp out R21Fz2InstMem1
//   ... + CRATONVM_JIT_IR_CHECKCAST_AFTER_INSTANCEOF=1
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

public class R21Fz2InstMem1 {
    static final int WARM = 200_000;

    interface Tag {
    }

    static class A {
        int f;
        Object ref;
    }

    static class B extends A implements Tag {
    }

    static class C extends A {
    }

    static int sf;
    static volatile int vflag;
    static volatile boolean go;

    static int t0(A a, Object o) {
        a.f = 1;
        if (o instanceof Tag) {
            a.f += 10;
        }
        return a.f;
    }

    static int t1(A a, A alias, Object o) {
        a.f = 5;
        boolean t = o instanceof B;
        alias.f = 7;
        boolean u = o instanceof Tag;
        return a.f * 100 + (t ? 10 : 0) + (u ? 1 : 0);
    }

    static int t2(int[] arr, int i, Object o) {
        arr[i] = 3;
        int r = o instanceof C ? 1 : 0;
        arr[i] += 4;
        r += o instanceof A ? 2 : 0;
        return arr[i] * 10 + r;
    }

    static int t3(Object o) {
        sf = 11;
        int r = o instanceof Tag ? 1 : 0;
        sf *= 2;
        return sf + r * 1000;
    }

    static int t4(A h, Object x) {
        h.ref = x;
        int r = h.ref instanceof Tag ? 1 : 0;
        h.ref = null;
        r += h.ref instanceof Tag ? 2 : 0;
        h.ref = x;
        return r + (h.ref instanceof A ? 4 : 0);
    }

    static int t5(A a, Object[] os, int n) {
        int s = 0;
        a.f = 0;
        for (int i = 0; i < n; i++) {
            Object o = os[i % os.length];
            if (o instanceof Tag) {
                a.f++;
            }
            s += a.f;
        }
        return s;
    }

    static void bump(A a) {
        a.f += 3;
    }

    static int t6(A a, Object[] os, int n) {
        int s = 0;
        a.f = 1;
        for (int i = 0; i < n; i++) {
            if (os[i % os.length] instanceof B) {
                bump(a);
            }
            s += a.f;
        }
        return s;
    }

    static int t7(A a, Object o) {
        int before = a.f;
        boolean t = o instanceof C;
        a.f = before + (t ? 1 : 2);
        boolean u = o instanceof C;
        return a.f + (u == t ? 0 : 100);
    }

    // Unread tests whose operands raise.
    static int u0(Object o) {
        boolean unused = ((A) o) instanceof Tag;
        return 1;
    }

    static int u1(Object[] os, int i) {
        boolean unused = os[i] instanceof Tag;
        return 2;
    }

    static int u2(A a) {
        boolean unused = a.ref instanceof Tag;
        return 3;
    }

    static String safe(java.util.function.IntSupplier s) {
        try {
            return Integer.toString(s.getAsInt());
        } catch (ClassCastException e) {
            return "CCE";
        } catch (ArrayIndexOutOfBoundsException e) {
            return "AIOOBE(" + e.getMessage() + ")";
        } catch (NullPointerException e) {
            return "NPE";
        }
    }

    static String row(Object o, Object[] os) {
        A a = new A();
        A b = new B();
        int[] arr = new int[4];
        return t0(a, o) + " " + t1(a, a, o) + " " + t1(a, b, o) + " " + t2(arr, 2, o) + " " + t3(o) + " "
                + t4(new A(), o) + " " + t5(new A(), os, 37) + " " + t6(new A(), os, 37) + " " + t7(new C(), o)
                + " | " + safe(() -> u0(o)) + " " + safe(() -> u1(os, 1)) + " " + safe(() -> u1(os, 99)) + " "
                + safe(() -> u2(null)) + " " + safe(() -> u2(new A()));
    }

    static void rows(String tag, Object[] vs) {
        for (int i = 0; i < vs.length; i++) {
            System.out.println("row " + tag + " v" + i + " | " + row(vs[i], vs));
        }
    }

    public static void main(String[] args) throws Exception {
        Object[] vs = {new A(), new B(), new C(), "str", null, new Object()};
        rows("cold", vs);
        long h = 0;
        A a = new A();
        A b = new B();
        int[] arr = new int[4];
        for (int r = 0; r < WARM; r++) {
            Object o = vs[r % vs.length];
            h = h * 31 + t0(a, o) + t1(a, (r & 1) == 0 ? a : b, o) + t2(arr, r & 3, o) + t3(o) + t4(a, o);
            h = h * 31 + t7(b, o) + u0(b) + u1(vs, r % vs.length) + u2(a);
            if ((r & 15) == 0) {
                h = h * 31 + t5(a, vs, 50) + t6(b, vs, 50);
            }
        }
        System.out.println("warm hash " + h);
        rows("hot", vs);
        // A volatile spin around a test must see another thread's write.
        Thread w = new Thread(() -> {
            while (!go) {
                Thread.onSpinWait();
            }
            vflag = 42;
        });
        w.start();
        go = true;
        int spins = 0;
        Object probe = new B();
        while (vflag != 42) {
            if (probe instanceof Tag) {
                spins++;
            }
        }
        w.join();
        System.out.println("spin done " + (spins >= 0) + " vflag " + vflag);
        // Deopt trigger: a subclass first seen now, in every test.
        Object late = new B() {
            @Override
            public String toString() {
                return "late";
            }
        };
        Object[] lv = {late, new C(), late, new A()};
        rows("late", lv);
        rows("after", vs);
    }
}
