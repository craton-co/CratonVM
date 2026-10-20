/*
 * Interpreter round i4, wave 87, lane L3: Unsafe accesses whose width or type
 * differs from the FIELD at that offset, and byte[] / heap ByteBuffer view
 * VarHandles at every width.
 *
 * HotSpot field offsets are byte offsets: getInt at a long field's offset is
 * its low half (little-endian), putInt there keeps the high half, getShort of
 * a char field is the 16 bits as a signed short, getInt of a float field its
 * bits, and an *Unaligned(.., true) access byte-swaps. CratonVM's field
 * offsets are slot indices, and these accesses replaced or narrowed the whole
 * slot. Only accesses at a field's OWN offset are probed: an access wider
 * than the field, or at offset+k, reads HotSpot's neighbouring bytes
 * (layout-dependent), and a primitive store into a reference field corrupts
 * HotSpot's heap (CratonVM ignores it).
 *
 * Run (both modes must print the HotSpot lines):
 *   javac --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -d out L3W87UnsafeMixedWidth.java
 *   cratonvm --jdk-only   --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp out L3W87UnsafeMixedWidth
 *   cratonvm --compatible --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp out L3W87UnsafeMixedWidth
 *   java                  --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp out L3W87UnsafeMixedWidth
 */
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// int-of-long: 55667788
// put-int-into-long: 11223344aabbccdd
// short-of-long: -13091
// short-of-char: -1
// char-of-short: 65534
// byte-of-int: 68
// put-byte-into-int: 11223399
// int-of-float: 3f800000
// put-int-into-float: 2.0
// long-of-double: 4000000000000000
// put-long-into-double: 1.0
// float-of-int: 1.5
// boolean-of-byte: true
// byte-of-boolean: 1
// int-unaligned-be-of-long: ddccbbaa
// put-int-unaligned-be-into-long: 1122334404030201
// same-kind: 7 9 x
// vh-int: 2030405 5040302
// vh-long-be: 405060708090a0b
// vh-char-be: 258
// vh-short-le: 4111
// vh-int-set-le: 2 44 33 22 11 7
// vh-double-le: 1.5
// vh-float-be: -0.25
// vh-int-oob: java.lang.ArrayIndexOutOfBoundsException: Index 14 out of bounds for length 13
// vh-int-volatile-array: java.lang.UnsupportedOperationException: getVolatile
// vh-int-volatile-heap-buffer: java.lang.IllegalStateException: Atomic access not supported for heap buffer
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;

public class L3W87UnsafeMixedWidth {
    static class Box {
        long l = 0x1122334455667788L;
        int i = 0x11223344;
        int j = 0x3fc00000;
        char c = '￿';
        short s = -2;
        float f = 1.0f;
        double d = 2.0;
        byte b = (byte) 0x80;
        boolean z = true;
        int k = 7;
        long m = 9;
        Object r = "x";
    }

    static final jdk.internal.misc.Unsafe U = jdk.internal.misc.Unsafe.getUnsafe();

    static long off(String name) throws Exception {
        return U.objectFieldOffset(Box.class.getDeclaredField(name));
    }

    static byte[] fresh() {
        byte[] a = new byte[16];
        for (int n = 0; n < a.length; n++) {
            a[n] = (byte) (n + 1);
        }
        return a;
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = t.getMessage() == null ? t.getClass().getName()
                    : t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Exception {
        Box o = new Box();
        long l = off("l"), i = off("i"), j = off("j"), c = off("c"), s = off("s");
        long f = off("f"), d = off("d"), b = off("b"), z = off("z");
        long k = off("k"), m = off("m"), r = off("r");

        System.out.println("int-of-long: " + Integer.toHexString(U.getInt(o, l)));
        U.putInt(o, l, 0xAABBCCDD);
        System.out.println("put-int-into-long: " + Long.toHexString(o.l));
        System.out.println("short-of-long: " + U.getShort(o, l));
        System.out.println("short-of-char: " + U.getShort(o, c));
        System.out.println("char-of-short: " + (int) U.getChar(o, s));
        System.out.println("byte-of-int: " + U.getByte(o, i));
        U.putByte(o, i, (byte) 0x99);
        System.out.println("put-byte-into-int: " + Integer.toHexString(o.i));
        System.out.println("int-of-float: " + Integer.toHexString(U.getInt(o, f)));
        U.putInt(o, f, 0x40000000);
        System.out.println("put-int-into-float: " + o.f);
        System.out.println("long-of-double: " + Long.toHexString(U.getLong(o, d)));
        U.putLong(o, d, 0x3ff0000000000000L);
        System.out.println("put-long-into-double: " + o.d);
        System.out.println("float-of-int: " + U.getFloat(o, j));
        System.out.println("boolean-of-byte: " + U.getBoolean(o, b));
        System.out.println("byte-of-boolean: " + U.getByte(o, z));
        System.out.println("int-unaligned-be-of-long: " + Integer.toHexString(U.getIntUnaligned(o, l, true)));
        U.putIntUnaligned(o, l, 0x01020304, true);
        System.out.println("put-int-unaligned-be-into-long: " + Long.toHexString(o.l));
        System.out.println("same-kind: " + U.getInt(o, k) + " " + U.getLong(o, m) + " " + U.getReference(o, r));

        VarHandle intBE = MethodHandles.byteArrayViewVarHandle(int[].class, ByteOrder.BIG_ENDIAN);
        VarHandle intLE = MethodHandles.byteArrayViewVarHandle(int[].class, ByteOrder.LITTLE_ENDIAN);
        VarHandle longBE = MethodHandles.byteArrayViewVarHandle(long[].class, ByteOrder.BIG_ENDIAN);
        VarHandle charBE = MethodHandles.byteArrayViewVarHandle(char[].class, ByteOrder.BIG_ENDIAN);
        VarHandle shortLE = MethodHandles.byteArrayViewVarHandle(short[].class, ByteOrder.LITTLE_ENDIAN);
        VarHandle doubleLE = MethodHandles.byteArrayViewVarHandle(double[].class, ByteOrder.LITTLE_ENDIAN);
        VarHandle floatBE = MethodHandles.byteArrayViewVarHandle(float[].class, ByteOrder.BIG_ENDIAN);
        VarHandle bufInt = MethodHandles.byteBufferViewVarHandle(int[].class, ByteOrder.BIG_ENDIAN);

        byte[] a = fresh();
        System.out.println("vh-int: " + Integer.toHexString((int) intBE.get(a, 1)) + " "
                + Integer.toHexString((int) intLE.get(a, 1)));
        System.out.println("vh-long-be: " + Long.toHexString((long) longBE.get(a, 3)));
        System.out.println("vh-char-be: " + (int) (char) charBE.get(a, 0));
        System.out.println("vh-short-le: " + (short) shortLE.get(a, 14));
        intLE.set(a, 2, 0x11223344);
        System.out.println("vh-int-set-le: " + Integer.toHexString(a[1] & 0xff) + " "
                + Integer.toHexString(a[2] & 0xff) + " " + Integer.toHexString(a[3] & 0xff) + " "
                + Integer.toHexString(a[4] & 0xff) + " " + Integer.toHexString(a[5] & 0xff) + " "
                + Integer.toHexString(a[6] & 0xff));
        byte[] a2 = fresh();
        doubleLE.set(a2, 5, 1.5);
        System.out.println("vh-double-le: " + (double) doubleLE.get(a2, 5));
        floatBE.set(a2, 9, -0.25f);
        System.out.println("vh-float-be: " + (float) floatBE.get(a2, 9));
        row("vh-int-oob", () -> (int) intBE.get(a2, 14));
        row("vh-int-volatile-array", () -> (int) intBE.getVolatile(a2, 0));
        ByteBuffer heap = ByteBuffer.wrap(fresh());
        row("vh-int-volatile-heap-buffer", () -> (int) bufInt.getVolatile(heap, 0));
    }
}
