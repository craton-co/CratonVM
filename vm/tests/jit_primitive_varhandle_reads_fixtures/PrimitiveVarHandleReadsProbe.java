// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

public final class PrimitiveVarHandleReadsProbe {
    static class Fields {
        volatile boolean z = true;
        volatile byte b = -103;
        volatile char c = 65530;
        volatile short s = -30001;
        volatile int i = -2000000001;
        volatile long j = -800000000000000003L;
        volatile float f = -3.25f;
        volatile double d = -123456789.125;
        Object ref = "reference";
    }
    static final class Child extends Fields { long extra = 17; }
    static VarHandle handle(String name, Class<?> type) throws Exception {
        return MethodHandles.lookup().findVarHandle(Fields.class, name, type);
    }
    static final VarHandle Z, B, C, S, I, J, F, D, R;
    static {
        try {
            Z=handle("z", boolean.class); B=handle("b", byte.class);
            C=handle("c", char.class); S=handle("s", short.class);
            I=handle("i", int.class); J=handle("j", long.class);
            F=handle("f", float.class); D=handle("d", double.class);
            R=handle("ref", Object.class);
        } catch (Exception e) { throw new ExceptionInInitializerError(e); }
    }
    static void check(Fields v) {
        if ((boolean)Z.get(v)!=v.z || (boolean)Z.getOpaque(v)!=v.z
                || (boolean)Z.getAcquire(v)!=v.z || (boolean)Z.getVolatile(v)!=v.z
                || (byte)B.get(v)!=v.b || (byte)B.getOpaque(v)!=v.b
                || (byte)B.getAcquire(v)!=v.b || (byte)B.getVolatile(v)!=v.b
                || (char)C.get(v)!=v.c || (char)C.getOpaque(v)!=v.c
                || (char)C.getAcquire(v)!=v.c || (char)C.getVolatile(v)!=v.c
                || (short)S.get(v)!=v.s || (short)S.getOpaque(v)!=v.s
                || (short)S.getAcquire(v)!=v.s || (short)S.getVolatile(v)!=v.s
                || (int)I.get(v)!=v.i || (int)I.getOpaque(v)!=v.i
                || (int)I.getAcquire(v)!=v.i || (int)I.getVolatile(v)!=v.i
                || (long)J.get(v)!=v.j || (long)J.getOpaque(v)!=v.j
                || (long)J.getAcquire(v)!=v.j || (long)J.getVolatile(v)!=v.j
                || Float.floatToRawIntBits((float)F.get(v))!=Float.floatToRawIntBits(v.f)
                || Float.floatToRawIntBits((float)F.getOpaque(v))!=Float.floatToRawIntBits(v.f)
                || Float.floatToRawIntBits((float)F.getAcquire(v))!=Float.floatToRawIntBits(v.f)
                || Float.floatToRawIntBits((float)F.getVolatile(v))!=Float.floatToRawIntBits(v.f)
                || Double.doubleToRawLongBits((double)D.get(v))!=Double.doubleToRawLongBits(v.d)
                || Double.doubleToRawLongBits((double)D.getOpaque(v))!=Double.doubleToRawLongBits(v.d)
                || Double.doubleToRawLongBits((double)D.getAcquire(v))!=Double.doubleToRawLongBits(v.d)
                || Double.doubleToRawLongBits((double)D.getVolatile(v))!=Double.doubleToRawLongBits(v.d)
                || R.get(v)!=v.ref || R.getAcquire(v)!=v.ref) {
            throw new AssertionError("field read changed its value or storage kind");
        }
    }
    static void rejected(Object v, Class<? extends Throwable> expected) {
        try { int value = (int)I.get(v); throw new AssertionError(value); }
        catch (Throwable e) { if (e.getClass()!=expected) throw new AssertionError(e); }
    }
    public static void main(String[] args) {
        for (int round=0; round<3; round++) {
            Fields v = round%2==0 ? new Fields() : new Child();
            for (int n=0; n<20000; n++) {
                if ((n&2047)==0) { v.z=!v.z; v.b++; v.c--; v.s++; v.i++; v.j--;
                    v.f=-v.f; v.d=-v.d; v.ref=new Object(); }
                check(v);
            }
            System.gc(); check(v);
        }
        rejected(null, NullPointerException.class);
        rejected(new Object(), ClassCastException.class);
        System.out.println("PRIMITIVE-VARHANDLE-READS-OK");
    }
}
