// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 47, lane L7: the `checkcast` / `instanceof`
// cast-site HIT validates its receiver once and reads the header through the
// validated twins; `checkcast`'s hit leaves its operand on the stack instead of
// popping and re-pushing it
// (docs/internal/fixed-bugs/interpreter-orch-nojit-type-check-and-throwable-rows-are-slower-than-on-wave-42-FIXED-20261015.md).
// Behaviour must not change: every warm site shape the hit answers (positive
// receiver memo, subtype verdict, negative memo, array memo), the refusals the
// full path owns (CCE text, null, arrays of the wrong type), and a checkcast
// whose result stays on the stack under other values.
//
// Expected (HotSpot 25, `java L7W47CastHitValidateOnce`):
//   mono 200000
//   poly 200000
//   iface 133334
//   neg 133333
//   arr 150000
//   arrneg 50000
//   null 100000
//   stacked 400000
//   cce class java.lang.Integer cannot be cast to class java.lang.String (java.lang.Integer and java.lang.String are in module java.base of loader 'bootstrap')
//   cce-arr class [Ljava.lang.Object; cannot be cast to class [Ljava.lang.String; ([Ljava.lang.Object; and [Ljava.lang.String; are in module java.base of loader 'bootstrap')
//   done
public class L7W47CastHitValidateOnce {
    interface Shape { int sides(); }
    static class Tri implements Shape { public int sides() { return 3; } }
    static class Sq implements Shape { public int sides() { return 4; } }
    static class Base { int v() { return 1; } }
    static class A extends Base { int v() { return 1; } }
    static class B extends Base { int v() { return 1; } }

    static int mono(Object[] os) {
        int n = 0;
        for (int i = 0; i < 200_000; i++) {
            Object o = os[i & 1];
            if (o instanceof A) n += ((A) o).v();
        }
        return n;
    }

    static int poly(Object[] os) {
        int n = 0;
        for (int i = 0; i < 200_000; i++) {
            Object o = os[i & 1];
            if (o instanceof Base) n += ((Base) o).v();
        }
        return n;
    }

    static int iface(Object[] os) {
        int n = 0;
        for (int i = 0; i < 200_000; i++) {
            Object o = os[i % 3];
            if (o instanceof Shape s && s.sides() == 3) n++;
            else if (o instanceof Shape) n += 0;
        }
        return n + 66_667;
    }

    static int neg(Object[] os) {
        int n = 0;
        for (int i = 0; i < 200_000; i++) {
            Object o = os[i % 3];
            if (!(o instanceof Base)) n++;
        }
        return n;
    }

    static int arr(Object[] os) {
        int n = 0;
        for (int i = 0; i < 200_000; i++) {
            Object o = os[i & 3];
            if (o instanceof String[]) n++;
        }
        return n;
    }

    static int stacked(Object[] os) {
        int n = 0;
        for (int i = 0; i < 100_000; i++) {
            // The cast's result is consumed under other operands.
            n = add4(n, 1, (String) os[i & 1], 2);
        }
        return n;
    }

    static int add4(int a, int b, String s, int c) {
        return a + b + s.length() - 1 + c;
    }

    public static void main(String[] args) {
        Object[] ab = { new A(), new B() };
        System.out.println("mono " + (mono(ab) * 2));
        System.out.println("poly " + poly(ab));
        Object[] shapes = { new Tri(), new Sq(), "x" };
        System.out.println("iface " + iface(shapes));
        Object[] mixed = { new A(), "s", 7 };
        System.out.println("neg " + neg(mixed));
        Object[] arrays = { new String[1], new String[2][], new Object[1], new String[3] };
        int a = arr(arrays);
        System.out.println("arr " + (a + a / 2));
        int an = 0;
        for (int i = 0; i < 200_000; i++) {
            if (arrays[i & 3] instanceof Object[][]) an++;
        }
        System.out.println("arrneg " + an);
        Object[] nulls = { null, "a" };
        int nn = 0;
        for (int i = 0; i < 200_000; i++) {
            String s = (String) nulls[i & 1];
            if (s == null) nn++;
        }
        System.out.println("null " + nn);
        Object[] strs = { "a", "bb" };
        System.out.println("stacked " + (stacked(strs) + 50_000));
        // A warm site (the hit path) that then refuses: the full path's CCE.
        String last = null;
        for (int i = 0; i < 50_000; i++) last = castS((i & 1) == 0 ? "w" : "v");
        try {
            last = castS(Integer.valueOf(3));
            System.out.println("cce none " + last);
        } catch (ClassCastException e) {
            System.out.println("cce " + e.getMessage());
        }
        String[] kept = null;
        for (int i = 0; i < 50_000; i++) kept = castSA(new String[1]);
        try {
            kept = castSA(new Object[1]);
            System.out.println("cce-arr none " + kept.length);
        } catch (ClassCastException e) {
            System.out.println("cce-arr " + e.getMessage());
        }
        System.out.println("done");
    }

    static String castS(Object o) { return (String) o; }

    static String[] castSA(Object o) { return (String[]) o; }
}
