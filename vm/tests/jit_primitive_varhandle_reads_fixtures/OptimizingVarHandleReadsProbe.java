// SPDX-License-Identifier: Apache-2.0
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;

public final class OptimizingVarHandleReadsProbe {
    static class Fields {
        boolean z = true;
        byte b = -113;
        char c = 65531;
        short s = -30003;
        int i = -123456789;
        long j = Long.MIN_VALUE;
        float f = 1234.5f;
        double d = -345.5;
    }
    static final class Child extends Fields { }
    static volatile long sink;
    static boolean staticFlag = true;
    static boolean fieldZget(VarHandle h, Object v) { return (boolean)h.get(v); }
    static boolean fieldZgetOpaque(VarHandle h, Object v) { return (boolean)h.getOpaque(v); }
    static boolean fieldZgetAcquire(VarHandle h, Object v) { return (boolean)h.getAcquire(v); }
    static boolean fieldZgetVolatile(VarHandle h, Object v) { return (boolean)h.getVolatile(v); }
    static byte fieldBget(VarHandle h, Object v) { return (byte)h.get(v); }
    static byte fieldBgetOpaque(VarHandle h, Object v) { return (byte)h.getOpaque(v); }
    static byte fieldBgetAcquire(VarHandle h, Object v) { return (byte)h.getAcquire(v); }
    static byte fieldBgetVolatile(VarHandle h, Object v) { return (byte)h.getVolatile(v); }
    static char fieldCget(VarHandle h, Object v) { return (char)h.get(v); }
    static char fieldCgetOpaque(VarHandle h, Object v) { return (char)h.getOpaque(v); }
    static char fieldCgetAcquire(VarHandle h, Object v) { return (char)h.getAcquire(v); }
    static char fieldCgetVolatile(VarHandle h, Object v) { return (char)h.getVolatile(v); }
    static short fieldSget(VarHandle h, Object v) { return (short)h.get(v); }
    static short fieldSgetOpaque(VarHandle h, Object v) { return (short)h.getOpaque(v); }
    static short fieldSgetAcquire(VarHandle h, Object v) { return (short)h.getAcquire(v); }
    static short fieldSgetVolatile(VarHandle h, Object v) { return (short)h.getVolatile(v); }
    static int fieldIget(VarHandle h, Object v) { return (int)h.get(v); }
    static int fieldIgetOpaque(VarHandle h, Object v) { return (int)h.getOpaque(v); }
    static int fieldIgetAcquire(VarHandle h, Object v) { return (int)h.getAcquire(v); }
    static int fieldIgetVolatile(VarHandle h, Object v) { return (int)h.getVolatile(v); }
    static long fieldJget(VarHandle h, Object v) { return (long)h.get(v); }
    static long fieldJgetOpaque(VarHandle h, Object v) { return (long)h.getOpaque(v); }
    static long fieldJgetAcquire(VarHandle h, Object v) { return (long)h.getAcquire(v); }
    static long fieldJgetVolatile(VarHandle h, Object v) { return (long)h.getVolatile(v); }
    static float fieldFget(VarHandle h, Object v) { return (float)h.get(v); }
    static float fieldFgetOpaque(VarHandle h, Object v) { return (float)h.getOpaque(v); }
    static float fieldFgetAcquire(VarHandle h, Object v) { return (float)h.getAcquire(v); }
    static float fieldFgetVolatile(VarHandle h, Object v) { return (float)h.getVolatile(v); }
    static double fieldDget(VarHandle h, Object v) { return (double)h.get(v); }
    static double fieldDgetOpaque(VarHandle h, Object v) { return (double)h.getOpaque(v); }
    static double fieldDgetAcquire(VarHandle h, Object v) { return (double)h.getAcquire(v); }
    static double fieldDgetVolatile(VarHandle h, Object v) { return (double)h.getVolatile(v); }
    static short viewS(VarHandle h, byte[] a, int i) { return (short)h.get(a, i); }
    static int viewI(VarHandle h, byte[] a, int i) { return (int)h.get(a, i); }
    static long viewJ(VarHandle h, byte[] a, int i) { return (long)h.get(a, i); }
    static float viewF(VarHandle h, byte[] a, int i) { return (float)h.get(a, i); }
    static double viewD(VarHandle h, byte[] a, int i) { return (double)h.get(a, i); }
    static char viewC(VarHandle h, byte[] a, int i) { return (char)h.get(a, i); }
    // The callers remove a real getter frame when IR splices these wrappers;
    // that earns optimization evidence while preserving the native bind.
    static int fieldCaller(VarHandle h, Object v) { return fieldIget(h, v) + 1; }
    static int viewCaller(VarHandle h, byte[] a, int i) { return viewI(h, a, i) + 1; }
    static boolean fieldCallerZget(VarHandle h, Object v) { return fieldZget(h, v); }
    static boolean fieldCallerZgetOpaque(VarHandle h, Object v) { return fieldZgetOpaque(h, v); }
    static boolean fieldCallerZgetAcquire(VarHandle h, Object v) { return fieldZgetAcquire(h, v); }
    static boolean fieldCallerZgetVolatile(VarHandle h, Object v) { return fieldZgetVolatile(h, v); }
    static byte fieldCallerBget(VarHandle h, Object v) { return fieldBget(h, v); }
    static byte fieldCallerBgetOpaque(VarHandle h, Object v) { return fieldBgetOpaque(h, v); }
    static byte fieldCallerBgetAcquire(VarHandle h, Object v) { return fieldBgetAcquire(h, v); }
    static byte fieldCallerBgetVolatile(VarHandle h, Object v) { return fieldBgetVolatile(h, v); }
    static char fieldCallerCget(VarHandle h, Object v) { return fieldCget(h, v); }
    static char fieldCallerCgetOpaque(VarHandle h, Object v) { return fieldCgetOpaque(h, v); }
    static char fieldCallerCgetAcquire(VarHandle h, Object v) { return fieldCgetAcquire(h, v); }
    static char fieldCallerCgetVolatile(VarHandle h, Object v) { return fieldCgetVolatile(h, v); }
    static short fieldCallerSget(VarHandle h, Object v) { return fieldSget(h, v); }
    static short fieldCallerSgetOpaque(VarHandle h, Object v) { return fieldSgetOpaque(h, v); }
    static short fieldCallerSgetAcquire(VarHandle h, Object v) { return fieldSgetAcquire(h, v); }
    static short fieldCallerSgetVolatile(VarHandle h, Object v) { return fieldSgetVolatile(h, v); }
    static int fieldCallerIget(VarHandle h, Object v) { return fieldIget(h, v); }
    static int fieldCallerIgetOpaque(VarHandle h, Object v) { return fieldIgetOpaque(h, v); }
    static int fieldCallerIgetAcquire(VarHandle h, Object v) { return fieldIgetAcquire(h, v); }
    static int fieldCallerIgetVolatile(VarHandle h, Object v) { return fieldIgetVolatile(h, v); }
    static long fieldCallerJget(VarHandle h, Object v) { return fieldJget(h, v); }
    static long fieldCallerJgetOpaque(VarHandle h, Object v) { return fieldJgetOpaque(h, v); }
    static long fieldCallerJgetAcquire(VarHandle h, Object v) { return fieldJgetAcquire(h, v); }
    static long fieldCallerJgetVolatile(VarHandle h, Object v) { return fieldJgetVolatile(h, v); }
    static float fieldCallerFget(VarHandle h, Object v) { return fieldFget(h, v); }
    static float fieldCallerFgetOpaque(VarHandle h, Object v) { return fieldFgetOpaque(h, v); }
    static float fieldCallerFgetAcquire(VarHandle h, Object v) { return fieldFgetAcquire(h, v); }
    static float fieldCallerFgetVolatile(VarHandle h, Object v) { return fieldFgetVolatile(h, v); }
    static double fieldCallerDget(VarHandle h, Object v) { return fieldDget(h, v); }
    static double fieldCallerDgetOpaque(VarHandle h, Object v) { return fieldDgetOpaque(h, v); }
    static double fieldCallerDgetAcquire(VarHandle h, Object v) { return fieldDgetAcquire(h, v); }
    static double fieldCallerDgetVolatile(VarHandle h, Object v) { return fieldDgetVolatile(h, v); }
    static char viewCallerC(VarHandle h, byte[] a, int i) { return viewC(h, a, i); }
    static short viewCallerS(VarHandle h, byte[] a, int i) { return viewS(h, a, i); }
    static int viewCallerI(VarHandle h, byte[] a, int i) { return viewI(h, a, i); }
    static long viewCallerJ(VarHandle h, byte[] a, int i) { return viewJ(h, a, i); }
    static float viewCallerF(VarHandle h, byte[] a, int i) { return viewF(h, a, i); }
    static double viewCallerD(VarHandle h, byte[] a, int i) { return viewD(h, a, i); }
    interface Action { void run() throws Throwable; }
    static String failure(Action action) {
        try { action.run(); throw new AssertionError("call accepted"); }
        catch (Throwable e) { return e.getClass().getName() + ":" + e.getMessage(); }
    }
    public static void main(String[] args) throws Throwable {
        int warmIterations = args.length == 0 ? 20000 : Integer.parseInt(args[0]);
        Fields f = new Fields();
        VarHandle[] fields = new VarHandle[8];
        fields[0] = MethodHandles.lookup().findVarHandle(Fields.class, "z", boolean.class);
        fields[1] = MethodHandles.lookup().findVarHandle(Fields.class, "b", byte.class);
        fields[2] = MethodHandles.lookup().findVarHandle(Fields.class, "c", char.class);
        fields[3] = MethodHandles.lookup().findVarHandle(Fields.class, "s", short.class);
        fields[4] = MethodHandles.lookup().findVarHandle(Fields.class, "i", int.class);
        fields[5] = MethodHandles.lookup().findVarHandle(Fields.class, "j", long.class);
        fields[6] = MethodHandles.lookup().findVarHandle(Fields.class, "f", float.class);
        fields[7] = MethodHandles.lookup().findVarHandle(Fields.class, "d", double.class);
        ByteOrder[] orders = {ByteOrder.BIG_ENDIAN, ByteOrder.LITTLE_ENDIAN};
        VarHandle[][] views = new VarHandle[2][6];
        byte[][] data = {new byte[32], new byte[32]};
        for (int e=0; e<2; e++) views[e][0] = MethodHandles.byteArrayViewVarHandle(short[].class, orders[e]);
        for (int e=0; e<2; e++) views[e][1] = MethodHandles.byteArrayViewVarHandle(char[].class, orders[e]);
        for (int e=0; e<2; e++) views[e][2] = MethodHandles.byteArrayViewVarHandle(int[].class, orders[e]);
        for (int e=0; e<2; e++) views[e][3] = MethodHandles.byteArrayViewVarHandle(long[].class, orders[e]);
        for (int e=0; e<2; e++) views[e][4] = MethodHandles.byteArrayViewVarHandle(float[].class, orders[e]);
        for (int e=0; e<2; e++) views[e][5] = MethodHandles.byteArrayViewVarHandle(double[].class, orders[e]);
        for (int e=0; e<2; e++) ByteBuffer.wrap(data[e]).order(orders[e]).putLong(1, 0x8123456789abcdefL);
        for (int round=0; round<2; round++) {
            for (int n=0; n<warmIterations; n++) {
                long sum = fieldCaller(fields[4], f) + viewCaller(views[0][2], data[0], 1);
                sum += (fieldCallerZget(fields[0], f) ? 1 : 0);
                sum += (fieldCallerZgetOpaque(fields[0], f) ? 1 : 0);
                sum += (fieldCallerZgetAcquire(fields[0], f) ? 1 : 0);
                sum += (fieldCallerZgetVolatile(fields[0], f) ? 1 : 0);
                sum += fieldCallerBget(fields[1], f);
                sum += fieldCallerBgetOpaque(fields[1], f);
                sum += fieldCallerBgetAcquire(fields[1], f);
                sum += fieldCallerBgetVolatile(fields[1], f);
                sum += fieldCallerCget(fields[2], f);
                sum += fieldCallerCgetOpaque(fields[2], f);
                sum += fieldCallerCgetAcquire(fields[2], f);
                sum += fieldCallerCgetVolatile(fields[2], f);
                sum += fieldCallerSget(fields[3], f);
                sum += fieldCallerSgetOpaque(fields[3], f);
                sum += fieldCallerSgetAcquire(fields[3], f);
                sum += fieldCallerSgetVolatile(fields[3], f);
                sum += fieldCallerIget(fields[4], f);
                sum += fieldCallerIgetOpaque(fields[4], f);
                sum += fieldCallerIgetAcquire(fields[4], f);
                sum += fieldCallerIgetVolatile(fields[4], f);
                sum += fieldCallerJget(fields[5], f);
                sum += fieldCallerJgetOpaque(fields[5], f);
                sum += fieldCallerJgetAcquire(fields[5], f);
                sum += fieldCallerJgetVolatile(fields[5], f);
                sum += Float.floatToRawIntBits(fieldCallerFget(fields[6], f));
                sum += Float.floatToRawIntBits(fieldCallerFgetOpaque(fields[6], f));
                sum += Float.floatToRawIntBits(fieldCallerFgetAcquire(fields[6], f));
                sum += Float.floatToRawIntBits(fieldCallerFgetVolatile(fields[6], f));
                sum += Double.doubleToRawLongBits(fieldCallerDget(fields[7], f));
                sum += Double.doubleToRawLongBits(fieldCallerDgetOpaque(fields[7], f));
                sum += Double.doubleToRawLongBits(fieldCallerDgetAcquire(fields[7], f));
                sum += Double.doubleToRawLongBits(fieldCallerDgetVolatile(fields[7], f));
                for (int e=0; e<2; e++) {
                    sum += viewCallerS(views[e][0], data[e], 1);
                    sum += viewCallerC(views[e][1], data[e], 1);
                    sum += viewCallerI(views[e][2], data[e], 1);
                    sum += viewCallerJ(views[e][3], data[e], 1);
                    sum += Float.floatToRawIntBits(viewCallerF(views[e][4], data[e], 1));
                    sum += Double.doubleToRawLongBits(viewCallerD(views[e][5], data[e], 1));
                }
                sink = sum;
            }
            System.gc();
            f = new Child();
            f.f = Float.intBitsToFloat(0xffc00001);
            f.d = -0.0;
            for (int e=0; e<2; e++) {
                ByteBuffer.wrap(data[e]).order(orders[e]).putLong(1, Long.MIN_VALUE);
            }
        }
        for (int e=0; e<2; e++) {
            final byte[] a = data[e];
            final VarHandle h = views[e][2];
            System.out.println(failure(() -> viewCaller(h, null, 0)));
            System.out.println(failure(() -> viewCaller(h, a, -1)));
            System.out.println(failure(() -> viewCaller(h, a, 29)));
            System.out.println(failure(() -> viewI(views[0][4], a, 1)));
            System.out.println(viewI(h.withInvokeExactBehavior(), a, 1));
        }
        VarHandle staticHandle = MethodHandles.lookup().findStaticVarHandle(
            OptimizingVarHandleReadsProbe.class, "staticFlag", boolean.class);
        System.out.println(failure(() -> { int ignored = (int)staticHandle.get(); }));
        final Object object = f;
        System.out.println(failure(() -> fieldCaller(fields[4], null)));
        System.out.println(failure(() -> fieldCaller(fields[4], "wrong")));
        System.out.println(failure(() -> fieldCaller(fields[0], object)));
        System.out.println(failure(() -> fieldCaller(fields[0], null)));
        System.out.println(failure(() -> fieldCaller(fields[4].withInvokeExactBehavior(), object)));
        System.out.println("OPTIMIZING-VARHANDLE-READS-OK:" + sink);
    }
}
