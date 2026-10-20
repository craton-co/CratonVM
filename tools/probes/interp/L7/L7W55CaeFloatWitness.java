// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 55, lane L7: deterministic single-thread rows of
// `VarHandle.compareAndExchange` (plain, `Acquire`, `Release`) and
// `compareAndSet` over `float` / `double` / `short` / `char` / `boolean` /
// reference instance fields and array elements: a successful exchange
// returns `expected` and stores; a failed one returns the value held and
// stores nothing; -0.0 against 0.0 and a NaN are compared by bits. These are
// the shapes the interpreter's route-cache exchange arm does not serve (it is
// `int` / `long` only), so every row reaches the native (and, under
// `--jdk-only`, whatever the JDK's VarHandle bodies call). Each row runs cold
// (r0) and again (r1, r2).
//
// Expected (HotSpot 25.0.3, `java L7W55CaeFloatWitness`, and `-Xint`):
//   r0 float 1.5 9.0 9.0 9.0 -0.0 -0.0 NaN 2.0
//   r0 double 1.5 9.0 9.0 9.0 -0.0 -0.0 NaN 2.0
//   r0 short 3 7 7 7 true
//   r0 char a z z z true
//   r0 boolean false true true true false
//   r0 ref a b b b true
//   r0 farr 1.5 9.0 9.0 9.0 true
//   r0 darr 1.5 9.0 9.0 9.0 true
//   r1 float 1.5 9.0 9.0 9.0 -0.0 -0.0 NaN 2.0
//   r1 double 1.5 9.0 9.0 9.0 -0.0 -0.0 NaN 2.0
//   r1 short 3 7 7 7 true
//   r1 char a z z z true
//   r1 boolean false true true true false
//   r1 ref a b b b true
//   r1 farr 1.5 9.0 9.0 9.0 true
//   r1 darr 1.5 9.0 9.0 9.0 true
//   r2 float 1.5 9.0 9.0 9.0 -0.0 -0.0 NaN 2.0
//   r2 double 1.5 9.0 9.0 9.0 -0.0 -0.0 NaN 2.0
//   r2 short 3 7 7 7 true
//   r2 char a z z z true
//   r2 boolean false true true true false
//   r2 ref a b b b true
//   r2 farr 1.5 9.0 9.0 9.0 true
//   r2 darr 1.5 9.0 9.0 9.0 true
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

public class L7W55CaeFloatWitness {
    static final class H {
        float f;
        double d;
        short s;
        char c;
        boolean z;
        Object r;
    }

    static final VarHandle F, D, S, C, Z, R;
    static final VarHandle FA = MethodHandles.arrayElementVarHandle(float[].class);
    static final VarHandle DA = MethodHandles.arrayElementVarHandle(double[].class);

    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            F = l.findVarHandle(H.class, "f", float.class);
            D = l.findVarHandle(H.class, "d", double.class);
            S = l.findVarHandle(H.class, "s", short.class);
            C = l.findVarHandle(H.class, "c", char.class);
            Z = l.findVarHandle(H.class, "z", boolean.class);
            R = l.findVarHandle(H.class, "r", Object.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static String floatRow() {
        H h = new H();
        h.f = 1.5f;
        // A failed exchange: the witness is the field, nothing stored.
        float w1 = (float) F.compareAndExchange(h, 2.0f, 3.0f);
        // Then three successful ones, one per ordering.
        float w2 = (float) F.compareAndExchangeAcquire(h, 1.5f, 9.0f);
        float w3 = h.f;
        boolean ok = F.compareAndSet(h, 9.0f, 9.0f);
        float w4 = h.f;
        // -0.0 is not 0.0 by bits: the exchange fails, the witness is -0.0.
        h.f = -0.0f;
        float w5 = (float) F.compareAndExchangeRelease(h, 0.0f, 5.0f);
        float w6 = h.f;
        // A NaN matches the same NaN by bits.
        h.f = Float.NaN;
        float w7 = (float) F.compareAndExchange(h, Float.NaN, 2.0f);
        float w8 = h.f;
        return w1 + " " + (ok ? w3 : -1f) + " " + w4 + " " + w3 + " " + w5 + " " + w6 + " " + w7 + " " + w8
                + check(w2 == 1.5f);
    }

    static String doubleRow() {
        H h = new H();
        h.d = 1.5;
        double w1 = (double) D.compareAndExchange(h, 2.0, 3.0);
        double w2 = (double) D.compareAndExchangeAcquire(h, 1.5, 9.0);
        double w3 = h.d;
        boolean ok = D.compareAndSet(h, 9.0, 9.0);
        double w4 = h.d;
        h.d = -0.0;
        double w5 = (double) D.compareAndExchangeRelease(h, 0.0, 5.0);
        double w6 = h.d;
        h.d = Double.NaN;
        double w7 = (double) D.compareAndExchange(h, Double.NaN, 2.0);
        double w8 = h.d;
        return w1 + " " + (ok ? w3 : -1d) + " " + w4 + " " + w3 + " " + w5 + " " + w6 + " " + w7 + " " + w8
                + check(w2 == 1.5);
    }

    static String check(boolean b) {
        return b ? "" : " (success witness wrong)";
    }

    static String shortRow() {
        H h = new H();
        h.s = 3;
        short w1 = (short) S.compareAndExchange(h, (short) 4, (short) 5);
        short w2 = (short) S.compareAndExchangeAcquire(h, (short) 3, (short) 7);
        short w3 = h.s;
        short w4 = (short) S.compareAndExchangeRelease(h, (short) 3, (short) 8);
        return w1 + " " + w3 + " " + w4 + " " + h.s + " " + (w2 == 3);
    }

    static String charRow() {
        H h = new H();
        h.c = 'a';
        char w1 = (char) C.compareAndExchange(h, 'b', 'c');
        char w2 = (char) C.compareAndExchange(h, 'a', 'z');
        char w3 = h.c;
        char w4 = (char) C.compareAndExchange(h, 'a', 'y');
        return w1 + " " + w3 + " " + w4 + " " + h.c + " " + (w2 == 'a');
    }

    static String booleanRow() {
        H h = new H();
        boolean w1 = (boolean) Z.compareAndExchange(h, true, false);
        boolean w2 = (boolean) Z.compareAndExchange(h, false, true);
        boolean w3 = h.z;
        boolean w4 = (boolean) Z.compareAndExchange(h, false, false);
        return w1 + " " + w3 + " " + w4 + " " + h.z + " " + w2;
    }

    static String refRow() {
        H h = new H();
        String a = "a";
        String b = "b";
        h.r = a;
        Object w1 = R.compareAndExchange(h, b, b);
        Object w2 = R.compareAndExchange(h, a, b);
        Object w3 = h.r;
        Object w4 = R.compareAndExchange(h, a, a);
        return w1 + " " + w3 + " " + w4 + " " + h.r + " " + (w2 == a);
    }

    static String floatArrayRow() {
        float[] a = {1.5f};
        float w1 = (float) FA.compareAndExchange(a, 0, 2.0f, 3.0f);
        float w2 = (float) FA.compareAndExchange(a, 0, 1.5f, 9.0f);
        float w3 = a[0];
        float w4 = (float) FA.compareAndExchange(a, 0, 1.5f, 4.0f);
        return w1 + " " + w3 + " " + w4 + " " + a[0] + " " + (w2 == 1.5f);
    }

    static String doubleArrayRow() {
        double[] a = {1.5};
        double w1 = (double) DA.compareAndExchange(a, 0, 2.0, 3.0);
        double w2 = (double) DA.compareAndExchange(a, 0, 1.5, 9.0);
        double w3 = a[0];
        double w4 = (double) DA.compareAndExchange(a, 0, 1.5, 4.0);
        return w1 + " " + w3 + " " + w4 + " " + a[0] + " " + (w2 == 1.5);
    }

    public static void main(String[] args) {
        for (int r = 0; r < 3; r++) {
            System.out.println("r" + r + " float " + floatRow());
            System.out.println("r" + r + " double " + doubleRow());
            System.out.println("r" + r + " short " + shortRow());
            System.out.println("r" + r + " char " + charRow());
            System.out.println("r" + r + " boolean " + booleanRow());
            System.out.println("r" + r + " ref " + refRow());
            System.out.println("r" + r + " farr " + floatArrayRow());
            System.out.println("r" + r + " darr " + doubleArrayRow());
        }
    }
}
