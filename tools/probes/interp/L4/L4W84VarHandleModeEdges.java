// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 84, lane L4: VarHandle access modes at their
// refusals and at float/double raw-bit comparison. JDK 25 sources:
// `VarForm.getMemberName` refuses an unsupported mode with
// `UnsupportedOperationException(<mode method name>)`; a final field's handle
// is read-only (every write mode refused); a `boolean` handle has no
// `getAndAdd`, a reference handle no numeric or bitwise mode, a float/double
// handle no bitwise mode; float/double CAS compares RAW bits
// (`Unsafe.compareAndSetFloat` -> `compareAndSetInt(floatToRawIntBits ..)`),
// so `-0.0` does not match `0.0` and a NaN matches only its own payload; a
// `byte[]` view (`VarHandleByteArrayAs*.ArrayHandle`) has `get` / `set` only;
// a `ByteBuffer` view checks, past `get` / `set`, a heap buffer
// (`IllegalStateException: Atomic access not supported for heap buffer`),
// then a read-only buffer on a write (`ReadOnlyBufferException`), then
// alignment (`IllegalStateException: Misaligned access at index: N`); a plain
// `set` on a read-only buffer is `ReadOnlyBufferException` too; a `short`
// view has no CAS; `withInvokeExactBehavior` refuses an inexact call site with
// `WrongMethodTypeException: handle's method type (..)T but found (..)U`.
// `iface-const` is the wave-84 `--compatible` fix: a static final interface
// constant reached through an implementing class is read-only.
//
// Run: javac -d . L4W84VarHandleModeEdges.java
//      cratonvm --jdk-only [--nojit] -cp . L4W84VarHandleModeEdges
//      cratonvm --compatible [--nojit] -cp . L4W84VarHandleModeEdges
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// final-instance: get=1 set=java.lang.UnsupportedOperationException: set cas=java.lang.UnsupportedOperationException: compareAndSet supports=true,false,true,false
// static-final: get=2 set=java.lang.UnsupportedOperationException: set supports=true,false
// iface-const: get=7 set=java.lang.UnsupportedOperationException: set supports=true,false
// boolean: getAndAdd=java.lang.UnsupportedOperationException: getAndAdd getAndBitwiseOr=false now=true
// object: getAndAdd=java.lang.UnsupportedOperationException: getAndAdd getAndBitwiseOr=java.lang.UnsupportedOperationException: getAndBitwiseOr
// float-modes: getAndBitwiseOr=java.lang.UnsupportedOperationException: getAndBitwiseOr getAndAdd=1.0 now=2.5
// field-float: zero=false negzero=true nan=false payload=true cae=80000000 left=80000000
// field-double: zero=false negzero=true nan=false payload=true cae=8000000000000000 left=8000000000000000
// static-float: zero=false negzero=true nan=false payload=true cae=80000000 left=80000000
// array-float: zero=false negzero=true nan=false payload=true cae=80000000 left=80000000
// array-double: zero=false negzero=true nan=false payload=true cae=8000000000000000 left=8000000000000000
// buffer-float: zero=false negzero=true nan=false payload=true cae=80000000 left=80000000
// buffer-double: zero=false negzero=true nan=false payload=true cae=8000000000000000 left=8000000000000000
// heap-getVolatile: java.lang.IllegalStateException: Atomic access not supported for heap buffer
// heap-get: 0
// ro-heap-set: java.nio.ReadOnlyBufferException: null
// ro-heap-setVolatile: java.lang.IllegalStateException: Atomic access not supported for heap buffer
// ro-heap-get: 0
// ro-direct-setVolatile: java.nio.ReadOnlyBufferException: null
// ro-direct-cas: java.nio.ReadOnlyBufferException: null
// ro-direct-getVolatile: 0
// misaligned-cas: java.lang.IllegalStateException: Misaligned access at index: 1
// misaligned-get: 0
// aligned-cas: true
// short-view-cas: java.lang.UnsupportedOperationException: compareAndSet
// short-view-getVolatile: 0
// float-view-getAndAdd: java.lang.UnsupportedOperationException: getAndAdd
// bytes-view: set-get=305419896 getVolatile=java.lang.UnsupportedOperationException: getVolatile cas=java.lang.UnsupportedOperationException: compareAndSet supports=true,true,false,false
// exact: exact=true ok=1.0 wrong=java.lang.invoke.WrongMethodTypeException: handle's method type (F)float but found (F)double plain=1.0
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.lang.invoke.VarHandle.AccessMode;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;

public class L4W84VarHandleModeEdges {
    public static class F {
        public final int fin = 1;
        public static final int SFIN = 2;
        public float f;
        public double d;
        public boolean z;
        public Object o;
        public static float sf;
    }

    public interface K {
        int KONST = 7;
    }

    public static class Impl implements K {
    }

    interface Row {
        Object run() throws Throwable;
    }

    static String r(Row row) {
        try {
            return String.valueOf(row.run());
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    static String fx(float v) {
        return Integer.toHexString(Float.floatToRawIntBits(v));
    }

    static String dx(double v) {
        return Long.toHexString(Double.doubleToRawLongBits(v));
    }

    static final float NAN_PAYLOAD_F = Float.intBitsToFloat(0x7fc00001);
    static final double NAN_PAYLOAD_D = Double.longBitsToDouble(0x7ff8000000000001L);

    /** A float cell: set it, CAS it, read it -- the coordinates are bound by the caller. */
    interface FloatCell {
        void set(float v) throws Throwable;
        boolean cas(float e, float n) throws Throwable;
        float cae(float e, float n) throws Throwable;
        float get() throws Throwable;
    }

    interface DoubleCell {
        void set(double v) throws Throwable;
        boolean cas(double e, double n) throws Throwable;
        double cae(double e, double n) throws Throwable;
        double get() throws Throwable;
    }

    static String floats(FloatCell c) {
        try {
            c.set(-0.0f);
            boolean zero = c.cas(0.0f, 1f);
            boolean negzero = c.cas(-0.0f, 1f);
            c.set(NAN_PAYLOAD_F);
            boolean nan = c.cas(Float.NaN, 2f);
            boolean payload = c.cas(NAN_PAYLOAD_F, 2f);
            c.set(-0.0f);
            float w = c.cae(0.0f, 3f);
            return "zero=" + zero + " negzero=" + negzero + " nan=" + nan + " payload=" + payload + " cae=" + fx(w)
                    + " left=" + fx(c.get());
        } catch (Throwable t) {
            return t.toString();
        }
    }

    static String doubles(DoubleCell c) {
        try {
            c.set(-0.0);
            boolean zero = c.cas(0.0, 1);
            boolean negzero = c.cas(-0.0, 1);
            c.set(NAN_PAYLOAD_D);
            boolean nan = c.cas(Double.NaN, 2);
            boolean payload = c.cas(NAN_PAYLOAD_D, 2);
            c.set(-0.0);
            double w = c.cae(0.0, 3);
            return "zero=" + zero + " negzero=" + negzero + " nan=" + nan + " payload=" + payload + " cae=" + dx(w)
                    + " left=" + dx(c.get());
        } catch (Throwable t) {
            return t.toString();
        }
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        F f = new F();

        VarHandle fin = l.findVarHandle(F.class, "fin", int.class);
        System.out.println("final-instance: get=" + r(() -> (int) fin.get(f)) + " set=" + r(() -> {
            fin.set(f, 5);
            return "ok";
        }) + " cas=" + r(() -> (boolean) fin.compareAndSet(f, 1, 5)) + " supports="
                + fin.isAccessModeSupported(AccessMode.GET) + "," + fin.isAccessModeSupported(AccessMode.SET) + ","
                + fin.isAccessModeSupported(AccessMode.GET_VOLATILE) + ","
                + fin.isAccessModeSupported(AccessMode.COMPARE_AND_SET));

        VarHandle sfin = l.findStaticVarHandle(F.class, "SFIN", int.class);
        System.out.println("static-final: get=" + r(() -> (int) sfin.get()) + " set=" + r(() -> {
            sfin.set(5);
            return "ok";
        }) + " supports=" + sfin.isAccessModeSupported(AccessMode.GET) + ","
                + sfin.isAccessModeSupported(AccessMode.SET));

        VarHandle konst = l.findStaticVarHandle(Impl.class, "KONST", int.class);
        System.out.println("iface-const: get=" + r(() -> (int) konst.get()) + " set=" + r(() -> {
            konst.set(5);
            return "ok";
        }) + " supports=" + konst.isAccessModeSupported(AccessMode.GET) + ","
                + konst.isAccessModeSupported(AccessMode.SET));

        VarHandle z = l.findVarHandle(F.class, "z", boolean.class);
        System.out.println("boolean: getAndAdd=" + r(() -> (boolean) z.getAndAdd(f, true)) + " getAndBitwiseOr="
                + r(() -> (boolean) z.getAndBitwiseOr(f, true)) + " now=" + f.z);

        VarHandle o = l.findVarHandle(F.class, "o", Object.class);
        System.out.println("object: getAndAdd=" + r(() -> (Object) o.getAndAdd(f, (Object) "x"))
                + " getAndBitwiseOr=" + r(() -> (Object) o.getAndBitwiseOr(f, (Object) "x")));

        VarHandle fv = l.findVarHandle(F.class, "f", float.class);
        f.f = 1.0f;
        System.out.println("float-modes: getAndBitwiseOr=" + r(() -> (float) fv.getAndBitwiseOr(f, 1f))
                + " getAndAdd=" + r(() -> (float) fv.getAndAdd(f, 1.5f)) + " now=" + f.f);

        System.out.println("field-float: " + floats(new FloatCell() {
            public void set(float v) { fv.set(f, v); }
            public boolean cas(float e, float n) { return (boolean) fv.compareAndSet(f, e, n); }
            public float cae(float e, float n) { return (float) fv.compareAndExchange(f, e, n); }
            public float get() { return (float) fv.get(f); }
        }));
        VarHandle dv = l.findVarHandle(F.class, "d", double.class);
        System.out.println("field-double: " + doubles(new DoubleCell() {
            public void set(double v) { dv.set(f, v); }
            public boolean cas(double e, double n) { return (boolean) dv.compareAndSet(f, e, n); }
            public double cae(double e, double n) { return (double) dv.compareAndExchange(f, e, n); }
            public double get() { return (double) dv.get(f); }
        }));
        VarHandle sv = l.findStaticVarHandle(F.class, "sf", float.class);
        System.out.println("static-float: " + floats(new FloatCell() {
            public void set(float v) { sv.set(v); }
            public boolean cas(float e, float n) { return (boolean) sv.compareAndSet(e, n); }
            public float cae(float e, float n) { return (float) sv.compareAndExchange(e, n); }
            public float get() { return (float) sv.get(); }
        }));
        float[] fa = new float[2];
        VarHandle fav = MethodHandles.arrayElementVarHandle(float[].class);
        System.out.println("array-float: " + floats(new FloatCell() {
            public void set(float v) { fav.set(fa, 1, v); }
            public boolean cas(float e, float n) { return (boolean) fav.compareAndSet(fa, 1, e, n); }
            public float cae(float e, float n) { return (float) fav.compareAndExchange(fa, 1, e, n); }
            public float get() { return (float) fav.get(fa, 1); }
        }));
        double[] da = new double[2];
        VarHandle dav = MethodHandles.arrayElementVarHandle(double[].class);
        System.out.println("array-double: " + doubles(new DoubleCell() {
            public void set(double v) { dav.set(da, 1, v); }
            public boolean cas(double e, double n) { return (boolean) dav.compareAndSet(da, 1, e, n); }
            public double cae(double e, double n) { return (double) dav.compareAndExchange(da, 1, e, n); }
            public double get() { return (double) dav.get(da, 1); }
        }));
        ByteBuffer direct = ByteBuffer.allocateDirect(32);
        VarHandle fbv = MethodHandles.byteBufferViewVarHandle(float[].class, ByteOrder.BIG_ENDIAN);
        System.out.println("buffer-float: " + floats(new FloatCell() {
            public void set(float v) { fbv.set(direct, 8, v); }
            public boolean cas(float e, float n) { return (boolean) fbv.compareAndSet(direct, 8, e, n); }
            public float cae(float e, float n) { return (float) fbv.compareAndExchange(direct, 8, e, n); }
            public float get() { return (float) fbv.get(direct, 8); }
        }));
        VarHandle dbv = MethodHandles.byteBufferViewVarHandle(double[].class, ByteOrder.LITTLE_ENDIAN);
        System.out.println("buffer-double: " + doubles(new DoubleCell() {
            public void set(double v) { dbv.set(direct, 16, v); }
            public boolean cas(double e, double n) { return (boolean) dbv.compareAndSet(direct, 16, e, n); }
            public double cae(double e, double n) { return (double) dbv.compareAndExchange(direct, 16, e, n); }
            public double get() { return (double) dbv.get(direct, 16); }
        }));

        VarHandle ibv = MethodHandles.byteBufferViewVarHandle(int[].class, ByteOrder.BIG_ENDIAN);
        ByteBuffer heap = ByteBuffer.allocate(16);
        ByteBuffer roHeap = heap.asReadOnlyBuffer();
        ByteBuffer roDirect = ByteBuffer.allocateDirect(16).asReadOnlyBuffer();
        ByteBuffer dir = ByteBuffer.allocateDirect(16);
        System.out.println("heap-getVolatile: " + r(() -> (int) ibv.getVolatile(heap, 0)));
        System.out.println("heap-get: " + r(() -> (int) ibv.get(heap, 0)));
        System.out.println("ro-heap-set: " + r(() -> {
            ibv.set(roHeap, 0, 1);
            return "ok";
        }));
        System.out.println("ro-heap-setVolatile: " + r(() -> {
            ibv.setVolatile(roHeap, 0, 1);
            return "ok";
        }));
        System.out.println("ro-heap-get: " + r(() -> (int) ibv.get(roHeap, 0)));
        System.out.println("ro-direct-setVolatile: " + r(() -> {
            ibv.setVolatile(roDirect, 0, 1);
            return "ok";
        }));
        System.out.println("ro-direct-cas: " + r(() -> (boolean) ibv.compareAndSet(roDirect, 0, 0, 1)));
        System.out.println("ro-direct-getVolatile: " + r(() -> (int) ibv.getVolatile(roDirect, 0)));
        System.out.println("misaligned-cas: " + r(() -> (boolean) ibv.compareAndSet(dir, 1, 0, 1)));
        System.out.println("misaligned-get: " + r(() -> (int) ibv.get(dir, 1)));
        System.out.println("aligned-cas: " + r(() -> (boolean) ibv.compareAndSet(dir, 4, 0, 1)));
        VarHandle sbv = MethodHandles.byteBufferViewVarHandle(short[].class, ByteOrder.BIG_ENDIAN);
        System.out.println("short-view-cas: " + r(() -> (boolean) sbv.compareAndSet(dir, 8, (short) 0, (short) 1)));
        System.out.println("short-view-getVolatile: " + r(() -> (short) sbv.getVolatile(dir, 8)));
        System.out.println("float-view-getAndAdd: " + r(() -> (float) fbv.getAndAdd(direct, 0, 1f)));

        VarHandle bav = MethodHandles.byteArrayViewVarHandle(int[].class, ByteOrder.BIG_ENDIAN);
        byte[] bytes = new byte[16];
        System.out.println("bytes-view: set-get=" + r(() -> {
            bav.set(bytes, 4, 0x12345678);
            return (int) bav.get(bytes, 4);
        }) + " getVolatile=" + r(() -> (int) bav.getVolatile(bytes, 4)) + " cas="
                + r(() -> (boolean) bav.compareAndSet(bytes, 4, 0, 1)) + " supports="
                + bav.isAccessModeSupported(AccessMode.GET) + "," + bav.isAccessModeSupported(AccessMode.SET) + ","
                + bav.isAccessModeSupported(AccessMode.GET_VOLATILE) + ","
                + bav.isAccessModeSupported(AccessMode.COMPARE_AND_SET));

        VarHandle ex = fv.withInvokeExactBehavior();
        f.f = 1.0f;
        System.out.println("exact: exact=" + ex.hasInvokeExactBehavior() + " ok=" + r(() -> (float) ex.get(f))
                + " wrong=" + r(() -> (double) ex.get(f)) + " plain=" + r(() -> (double) fv.get(f)));
    }
}
