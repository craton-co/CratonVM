// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 119, lane L8: what CRATONVM_INTERP_FUSED_STORE_CHAIN
// (default off) must leave unchanged. With it on, a served `aload_N; {aload_M
// | iload_M}; putfield` pair (CRATONVM_INTERP_FUSED_LOAD_PAIR, default on)
// runs the same pairs right after it in the same dispatch.
//
// Rows:
//   ctor <n>     constructors storing 2, 4 and 6 fields (ints and references,
//                final and not) from their parameters
//   setters      a method storing four fields of `this` in a row
//   mixed        a chain broken by a long store (lload, not a pair) and
//                resumed after it
//   twoTargets   `p.a = x; q.b = y;` with two receivers
//   nullMid <k>  the same with the k-th receiver null: the NPE, HotSpot's
//                helpful message (it names the putfield that threw) and its
//                line
//   subclass     a subclass constructor's chain after `super(...)`
//   nullWide <k> (wave 120, CRATONVM_INTERP_FUSED_STORE_CHAIN_WIDE) the
//                values from `iload 4`, a null at the k-th receiver
//
// Every line must be the same with the switch off and on, --jdk-only and
// --compatible, JIT and --nojit.
//
// Run: javac -g -d . L8W119StoreChain.java
//      java -cp . L8W119StoreChain                                       (HotSpot reference)
//      cratonvm --jdk-only --nojit -cp . L8W119StoreChain
//      CRATONVM_INTERP_FUSED_STORE_CHAIN=1 cratonvm --jdk-only --nojit -cp . L8W119StoreChain
//      CRATONVM_INTERP_FUSED_STORE_CHAIN=1 CRATONVM_INTERP_FUSED_STORE_CHAIN_WIDE=1 //        cratonvm --jdk-only --nojit -cp . L8W119StoreChain
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// ctor 2 2999 t
// ctor 4 2999 3000 t true
// ctor 6 2999 5998 8997 t true true sum=40489500
// setters 2999 -2999 t t
// mixed 2999 25761213841408 true 2999
// twoTargets 2999 0 0 2999
// nullMid 0 Cannot assign field "a" because "p" is null line=140 p.a=0 q.b=0
// nullMid 1 Cannot assign field "b" because "q" is null line=141 p.a=7 q.b=0
// nullMid 2 ok 7 7
// nullWide 0 Cannot assign field "a" because "p" is null line=147 p.a=0 q.b=0
// nullWide 1 Cannot assign field "b" because "q" is null line=148 p.a=9 q.b=0
// nullWide 2 ok 9 9
// subclass 2999 3000 3001 true
// HOTSPOT_EXPECTED_END
public class L8W119StoreChain {
    static final class P2 {
        int a;
        Object b;

        P2(int a, Object b) {
            this.a = a;
            this.b = b;
        }
    }

    static final class P4 {
        final int a;
        final int b;
        Object c;
        final Object d;

        P4(int a, int b, Object c, Object d) {
            this.a = a;
            this.b = b;
            this.c = c;
            this.d = d;
        }
    }

    static final class P6 {
        int a;
        int b;
        int c;
        Object d;
        Object e;
        Object f;

        // Locals 0..3 are this, a, b, c: the first three pairs use iload_1..3;
        // d, e, f arrive in locals 4..6 (aload n), which end the chain.
        P6(int a, int b, int c, Object d, Object e, Object f) {
            this.a = a;
            this.b = b;
            this.c = c;
            this.d = d;
            this.e = e;
            this.f = f;
        }
    }

    static class Base {
        int x;

        Base(int x) {
            this.x = x;
        }
    }

    static final class Derived extends Base {
        int y;
        int z;
        Object w;

        Derived(int x, int y, int z) {
            super(x);
            this.y = y;
            this.z = z;
            this.w = this;
        }
    }

    int f1;
    int f2;
    Object f3;
    Object f4;
    long big;
    int after;

    void setters(int x, Object o, int y) {
        this.f1 = x;
        this.f3 = o;
        this.f2 = y;
        this.f4 = o;
    }

    void mixed(int x, long l, Object o) {
        this.f1 = x;
        this.big = l;
        this.f3 = o;
        this.after = x;
    }

    static final class Cell {
        int a;
        int b;
    }

    static void twoTargets(Cell p, Cell q, int x) {
        p.a = x;
        q.b = x;
    }

    // Wave 120 (lane L8, CRATONVM_INTERP_FUSED_STORE_CHAIN_WIDE): the value
    // comes from local 4 (`iload 4`), the receivers from locals 0 and 1.
    static void wideTargets(Cell p, Cell q, long pad, int x) {
        p.a = x;
        q.b = x;
    }

    static String nullWide(int k) {
        Cell p = new Cell();
        Cell q = new Cell();
        Cell good = new Cell();
        for (int i = 0; i < 2000; i++) {
            wideTargets(good, good, 0L, i);
        }
        try {
            wideTargets(k == 0 ? null : p, k == 1 ? null : q, 0L, 9);
            return "ok " + p.a + " " + q.b;
        } catch (NullPointerException e) {
            return e.getMessage() + " line=" + e.getStackTrace()[0].getLineNumber()
                    + " p.a=" + p.a + " q.b=" + q.b;
        }
    }

    static String nullMid(int k) {
        Cell p = new Cell();
        Cell q = new Cell();
        Cell good = new Cell();
        // Warm the sites so the stores are quickened when the null arrives.
        for (int i = 0; i < 2000; i++) {
            twoTargets(good, good, i);
        }
        try {
            twoTargets(k == 0 ? null : p, k == 1 ? null : q, 7);
            return "ok " + p.a + " " + q.b;
        } catch (NullPointerException e) {
            return e.getMessage() + " line=" + e.getStackTrace()[0].getLineNumber()
                    + " p.a=" + p.a + " q.b=" + q.b;
        }
    }

    public static void main(String[] args) {
        long s = 0;
        Object tag = "t";
        P2 p2 = null;
        P4 p4 = null;
        P6 p6 = null;
        for (int i = 0; i < 3000; i++) {
            p2 = new P2(i, tag);
            p4 = new P4(i, i + 1, tag, p2);
            p6 = new P6(i, i * 2, i * 3, tag, p2, p4);
            s += p2.a + p4.a + p4.b + p6.a + p6.b + p6.c;
        }
        System.out.println("ctor 2 " + p2.a + " " + p2.b);
        System.out.println("ctor 4 " + p4.a + " " + p4.b + " " + p4.c + " " + (p4.d == p2));
        System.out.println("ctor 6 " + p6.a + " " + p6.b + " " + p6.c + " " + p6.d + " "
                + (p6.e == p2) + " " + (p6.f == p4) + " sum=" + s);
        L8W119StoreChain t = new L8W119StoreChain();
        for (int i = 0; i < 3000; i++) {
            t.setters(i, tag, -i);
        }
        System.out.println("setters " + t.f1 + " " + t.f2 + " " + t.f3 + " " + t.f4);
        for (int i = 0; i < 3000; i++) {
            t.mixed(i, (long) i << 33, p2);
        }
        System.out.println("mixed " + t.f1 + " " + t.big + " " + (t.f3 == p2) + " " + t.after);
        Cell c1 = new Cell();
        Cell c2 = new Cell();
        for (int i = 0; i < 3000; i++) {
            twoTargets(c1, c2, i);
        }
        System.out.println("twoTargets " + c1.a + " " + c1.b + " " + c2.a + " " + c2.b);
        for (int k = 0; k <= 2; k++) {
            System.out.println("nullMid " + k + " " + nullMid(k));
        }
        for (int k = 0; k <= 2; k++) {
            System.out.println("nullWide " + k + " " + nullWide(k));
        }
        Derived d = null;
        for (int i = 0; i < 3000; i++) {
            d = new Derived(i, i + 1, i + 2);
        }
        System.out.println("subclass " + d.x + " " + d.y + " " + d.z + " " + (d.w == d));
    }
}
