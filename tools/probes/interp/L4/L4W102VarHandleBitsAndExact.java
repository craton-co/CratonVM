// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 102, lane L4 (hunt): VarHandle value semantics
// against HotSpot 25, both modes:
//
// * `getAndBitwiseOr` / `And` / `Xor` on a `boolean` field, static and array
//   element (supported: `VarHandleBooleans`), `getAndAdd` on one refused;
// * `compareAndExchange` / `compareAndSet` on `float` / `double` compare RAW
//   BITS (`Unsafe.compareAndExchangeFloat`): -0.0 is not 0.0, a NaN equals
//   the same NaN bits, and a NaN with another payload is not equal;
// * an invoke-exact handle's `WrongMethodTypeException` texts (field, array
//   element and `byte[]` view handles) and `withInvokeBehavior()`.
//
// By reading CratonVM's doors agree (`vh_values_match` /
// `values_equal_for_cas` compare `to_bits`); this is a regression pin, no fix.
//
// Run: javac -d . L4W102VarHandleBitsAndExact.java
//      java -cp . L4W102VarHandleBitsAndExact                          (HotSpot)
//      cratonvm --jdk-only [--nojit] -cp . L4W102VarHandleBitsAndExact
//      cratonvm --compatible [--nojit] -cp . L4W102VarHandleBitsAndExact
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived from the sources)
// bool-field or false->true and true->false xor false->true
// bool-static or false->true
// bool-array xor true->false
// bool-getAndAdd java.lang.UnsupportedOperationException: getAndAdd
// float-cae-zero witness 80000000 now 80000000
// float-cas-zero false now 80000000
// float-cas-negzero true now 40000000
// float-cae-nan witness 7fc00000 now 40400000
// float-cas-other-nan false now 7fc00001
// double-cae-zero witness 8000000000000000 now 8000000000000000
// double-caeAcquire-negzero witness 8000000000000000 now 4014000000000000
// float-array-cae-nan witness 7fc00000 now 3f800000
// float-static-cae-zero witness 80000000 now 80000000
// exact-get-long java.lang.invoke.WrongMethodTypeException: handle's method type (Holder)int but found (Holder)long
// exact-get-object-receiver java.lang.invoke.WrongMethodTypeException: handle's method type (Holder)int but found (Object)int
// exact-set-short java.lang.invoke.WrongMethodTypeException: handle's method type (Holder,int)void but found (Holder,short)void
// exact-cas true 2
// exact-flags true false true
// exact-array-get-object java.lang.invoke.WrongMethodTypeException: handle's method type (int[],int)int but found (int[],int)Object
// exact-view-get-long java.lang.invoke.WrongMethodTypeException: handle's method type (byte[],int)int but found (byte[],int)long
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.nio.ByteOrder;

public class L4W102VarHandleBitsAndExact {
    static class Holder {
        boolean z;
        float f;
        double d;
        int i = 1;
    }

    static boolean SZ;
    static float SF = -0.0f;

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        try {
            System.out.println(name + " " + r.run());
        } catch (Throwable e) {
            System.out.println(name + " " + e.getClass().getName() + ": " + e.getMessage());
        }
    }

    static String fb(float f) {
        return Integer.toHexString(Float.floatToRawIntBits(f));
    }

    static String db(double d) {
        return Long.toHexString(Double.doubleToRawLongBits(d));
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        VarHandle z = l.findVarHandle(Holder.class, "z", boolean.class);
        VarHandle f = l.findVarHandle(Holder.class, "f", float.class);
        VarHandle d = l.findVarHandle(Holder.class, "d", double.class);
        VarHandle i = l.findVarHandle(Holder.class, "i", int.class);
        VarHandle sz = l.findStaticVarHandle(L4W102VarHandleBitsAndExact.class, "SZ", boolean.class);
        VarHandle sf = l.findStaticVarHandle(L4W102VarHandleBitsAndExact.class, "SF", float.class);
        VarHandle za = MethodHandles.arrayElementVarHandle(boolean[].class);
        VarHandle fa = MethodHandles.arrayElementVarHandle(float[].class);
        Holder h = new Holder();

        row("bool-field", () -> {
            boolean a = (boolean) z.getAndBitwiseOr(h, true);
            String s = "or " + a + "->" + h.z;
            boolean b = (boolean) z.getAndBitwiseAnd(h, false);
            s += " and " + b + "->" + h.z;
            boolean c = (boolean) z.getAndBitwiseXor(h, true);
            return s + " xor " + c + "->" + h.z;
        });
        row("bool-static", () -> {
            boolean a = (boolean) sz.getAndBitwiseOrRelease(true);
            return "or " + a + "->" + SZ;
        });
        row("bool-array", () -> {
            boolean[] arr = {true};
            boolean a = (boolean) za.getAndBitwiseXorAcquire(arr, 0, true);
            return "xor " + a + "->" + arr[0];
        });
        row("bool-getAndAdd", () -> (boolean) z.getAndAdd(h, true));

        h.f = -0.0f;
        row("float-cae-zero", () -> {
            float w = (float) f.compareAndExchange(h, 0.0f, 1.0f);
            return "witness " + fb(w) + " now " + fb(h.f);
        });
        row("float-cas-zero", () -> (boolean) f.compareAndSet(h, 0.0f, 1.0f) + " now " + fb(h.f));
        row("float-cas-negzero", () -> (boolean) f.compareAndSet(h, -0.0f, 2.0f) + " now " + fb(h.f));
        h.f = Float.NaN;
        row("float-cae-nan", () -> {
            float w = (float) f.compareAndExchange(h, Float.NaN, 3.0f);
            return "witness " + fb(w) + " now " + fb(h.f);
        });
        h.f = Float.intBitsToFloat(0x7fc00001);
        row("float-cas-other-nan", () -> (boolean) f.compareAndSet(h, Float.NaN, 4.0f) + " now " + fb(h.f));
        h.d = -0.0;
        row("double-cae-zero", () -> {
            double w = (double) d.compareAndExchange(h, 0.0, 1.0);
            return "witness " + db(w) + " now " + db(h.d);
        });
        row("double-caeAcquire-negzero", () -> {
            double w = (double) d.compareAndExchangeAcquire(h, -0.0, 5.0);
            return "witness " + db(w) + " now " + db(h.d);
        });
        row("float-array-cae-nan", () -> {
            float[] arr = {Float.NaN};
            float w = (float) fa.compareAndExchange(arr, 0, Float.NaN, 1.0f);
            return "witness " + fb(w) + " now " + fb(arr[0]);
        });
        row("float-static-cae-zero", () -> {
            float w = (float) sf.compareAndExchangeRelease(0.0f, 7.0f);
            return "witness " + fb(w) + " now " + fb(SF);
        });

        VarHandle ex = i.withInvokeExactBehavior();
        row("exact-get-long", () -> (long) ex.get(h));
        row("exact-get-object-receiver", () -> (int) ex.get((Object) h));
        row("exact-set-short", () -> {
            ex.set(h, (short) 3);
            return "stored";
        });
        row("exact-cas", () -> (boolean) ex.compareAndSet(h, 1, 2) + " " + (int) ex.get(h));
        row("exact-flags", () -> ex.hasInvokeExactBehavior() + " "
                + ex.withInvokeBehavior().hasInvokeExactBehavior() + " "
                + (ex.withInvokeExactBehavior() == ex));
        VarHandle exa = MethodHandles.arrayElementVarHandle(int[].class).withInvokeExactBehavior();
        row("exact-array-get-object", () -> (Object) exa.get(new int[] {4}, 0));
        VarHandle exv = MethodHandles.byteArrayViewVarHandle(int[].class, ByteOrder.BIG_ENDIAN)
                .withInvokeExactBehavior();
        row("exact-view-get-long", () -> (long) exv.get(new byte[8], 0));
    }
}
