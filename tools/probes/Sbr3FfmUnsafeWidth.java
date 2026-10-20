import java.util.Arrays;

/** run with --add-exports java.base/jdk.internal.misc=ALL-UNNAMED: raw-width Unsafe access to primitive arrays of every element type. */
public class Sbr3FfmUnsafeWidth {
    static jdk.internal.misc.Unsafe u = jdk.internal.misc.Unsafe.getUnsafe();
    static void row(String k, Object v) { System.out.println(k + " |" + v + "|"); }
    interface T { Object run() throws Throwable; }
    static void t(String k, T body) {
        try { row(k, body.run()); } catch (Throwable e) { row(k, "THREW " + e.getClass().getName()); }
    }
    static String bytes(Object arr) {
        long base = u.arrayBaseOffset(arr.getClass());
        int n = java.lang.reflect.Array.getLength(arr) * u.arrayIndexScale(arr.getClass());
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < n; i++) sb.append(String.format("%02x", u.getByte(arr, base + i)));
        return sb.toString();
    }
    public static void main(String[] args) {
        Object[] arrays = { new byte[16], new short[8], new char[8], new int[4], new long[2], new float[4], new double[2] };
        for (Object arr : arrays) {
            String n = arr.getClass().getSimpleName();
            long b = u.arrayBaseOffset(arr.getClass());
            for (long off : new long[] { 0, 1, 2, 4, 8 }) {
                final long o = off;
                t(n + ".putLong@" + off, () -> { u.putLong(arr, b + o, 0x0102030405060708L); return bytes(arr); });
                t(n + ".putInt@" + off, () -> { u.putInt(arr, b + o, 0x0a0b0c0d); return bytes(arr); });
                t(n + ".putShort@" + off, () -> { u.putShort(arr, b + o, (short) 0x1112); return bytes(arr); });
                t(n + ".putByte@" + off, () -> { u.putByte(arr, b + o, (byte) 0x21); return bytes(arr); });
                t(n + ".putLongUnaligned@" + off, () -> { u.putLongUnaligned(arr, b + o, 0x1112131415161718L); return bytes(arr); });
                t(n + ".putIntUnaligned@" + off, () -> { u.putIntUnaligned(arr, b + o, 0x31323334); return bytes(arr); });
                t(n + ".putShortUnaligned@" + off, () -> { u.putShortUnaligned(arr, b + o, (short) 0x4142); return bytes(arr); });
                t(n + ".getLong@" + off, () -> u.getLong(arr, b + o));
                t(n + ".getInt@" + off, () -> u.getInt(arr, b + o));
                t(n + ".getShort@" + off, () -> u.getShort(arr, b + o));
                t(n + ".getChar@" + off, () -> (int) u.getChar(arr, b + o));
                t(n + ".getLongUnaligned@" + off, () -> u.getLongUnaligned(arr, b + o));
                t(n + ".getIntUnaligned@" + off, () -> u.getIntUnaligned(arr, b + o));
                t(n + ".getShortUnaligned@" + off, () -> u.getShortUnaligned(arr, b + o));
            }
            t(n + ".setMemory", () -> { u.setMemory(arr, b + 4, 6, (byte) 0x55); return bytes(arr); });
            t(n + ".putFloat@4", () -> { u.putFloat(arr, b + 4, 1.5f); return bytes(arr); });
            t(n + ".putDouble@8", () -> { u.putDouble(arr, b + 8, 2.5); return bytes(arr); });
            t(n + ".getDouble@8", () -> Double.doubleToRawLongBits(u.getDouble(arr, b + 8)));
            t(n + ".getFloat@4", () -> Float.floatToRawIntBits(u.getFloat(arr, b + 4)));
        }
    }
}
