import java.lang.reflect.Field;
import jdk.internal.misc.Unsafe;

/**
 * SBR3 NIOBUF: jdk.internal.misc.Unsafe sub-word CAS on object fields and arrays, called directly.
 * Compile and run with --add-exports java.base/jdk.internal.misc=ALL-UNNAMED.
 */
public class Sbr3NioBufUnsafe {
    static class H { byte b0 = 1; byte b1 = 2; byte b2 = 3; byte b3 = 4; short s0 = 5; short s1 = 6; boolean z = true; char c = 'c'; int i = 7; long l = 8; }
    static byte sb = 9;
    static void row(String n, Object v) { System.out.println(n + " = " + v); }
    public static void main(String[] a) throws Throwable {
        Unsafe u = Unsafe.getUnsafe();
        H h = new H();
        long o0 = u.objectFieldOffset(H.class, "b0"), o1 = u.objectFieldOffset(H.class, "b1"), o2 = u.objectFieldOffset(H.class, "b2"), o3 = u.objectFieldOffset(H.class, "b3");
        row("cas b1", u.compareAndSetByte(h, o1, (byte) 2, (byte) 20) + " " + h.b0 + "," + h.b1 + "," + h.b2 + "," + h.b3);
        row("cas b1 fail", u.compareAndSetByte(h, o1, (byte) 2, (byte) 21) + " " + h.b1);
        row("cas b3", u.compareAndSetByte(h, o3, (byte) 4, (byte) 40) + " " + h.b0 + "," + h.b1 + "," + h.b2 + "," + h.b3);
        row("cas b0", u.compareAndSetByte(h, o0, (byte) 1, (byte) 10) + " " + h.b0 + "," + h.b1 + "," + h.b2 + "," + h.b3 + " " + h.s0 + "," + h.s1 + "," + h.z + "," + h.c + "," + h.i + "," + h.l);
        row("cas short", u.compareAndSetShort(h, u.objectFieldOffset(H.class, "s1"), (short) 6, (short) 60) + " " + h.s0 + "," + h.s1);
        row("cas bool", u.compareAndSetBoolean(h, u.objectFieldOffset(H.class, "z"), true, false) + " " + h.z);
        row("cas char", u.compareAndSetChar(h, u.objectFieldOffset(H.class, "c"), 'c', 'd') + " " + h.c);
        row("getAndAddByte", u.getAndAddByte(h, o2, (byte) 5) + " " + h.b2);
        row("getAndSetByte", u.getAndSetByte(h, o2, (byte) 77) + " " + h.b2);
        row("getAndBitwiseOrByte", u.getAndBitwiseOrByte(h, o2, (byte) 0x0f) + " " + h.b2);
        row("weakCas byte", u.weakCompareAndSetByte(h, o1, (byte) 20, (byte) 22) + " " + h.b1);
        long so = u.staticFieldOffset(Sbr3NioBufUnsafe.class.getDeclaredField("sb"));
        Object sbase = u.staticFieldBase(Sbr3NioBufUnsafe.class.getDeclaredField("sb"));
        row("cas static", u.compareAndSetByte(sbase, so, (byte) 9, (byte) 90) + " " + sb);
        byte[] arr = new byte[8];
        long ab = u.arrayBaseOffset(byte[].class);
        row("cas array", u.compareAndSetByte(arr, ab + 5, (byte) 0, (byte) 5) + " " + java.util.Arrays.toString(arr));
        row("cas array fail", u.compareAndSetByte(arr, ab + 5, (byte) 0, (byte) 6) + " " + java.util.Arrays.toString(arr));
        long m = u.allocateMemory(16);
        u.putLong(m, 0L);
        row("cas raw", u.compareAndSetByte(null, m + 3, (byte) 0, (byte) 7) + " " + Long.toHexString(u.getLong(m)));
        u.freeMemory(m);
        int[] ia = new int[2];
        row("cas byte in int[]", u.compareAndSetByte(ia, u.arrayBaseOffset(int[].class) + 1, (byte) 0, (byte) 0x7f) + " " + Integer.toHexString(ia[0]));
        H g = new H();
        row("cae b1 hit", u.compareAndExchangeByte(g, u.objectFieldOffset(H.class, "b1"), (byte) 2, (byte) 20) + " " + g.b0 + "," + g.b1 + "," + g.b2 + "," + g.b3);
        row("cae b1 miss", u.compareAndExchangeByte(g, u.objectFieldOffset(H.class, "b1"), (byte) 2, (byte) 21) + " " + g.b1);
        row("cae b3", u.compareAndExchangeByte(g, u.objectFieldOffset(H.class, "b3"), (byte) 4, (byte) 41) + " " + g.b0 + "," + g.b1 + "," + g.b2 + "," + g.b3);
        row("cae static", u.compareAndExchangeByte(sbase, so, (byte) 90, (byte) 91) + " " + sb);
        byte[] ar2 = new byte[8];
        row("cae array", u.compareAndExchangeByte(ar2, ab + 6, (byte) 0, (byte) 6) + " " + java.util.Arrays.toString(ar2));
        row("cae array miss", u.compareAndExchangeByte(ar2, ab + 6, (byte) 0, (byte) 7) + " " + java.util.Arrays.toString(ar2));
        row("cae bool", u.compareAndExchangeBoolean(g, u.objectFieldOffset(H.class, "z"), true, false) + " " + g.z);
        row("cae short", u.compareAndExchangeShort(g, u.objectFieldOffset(H.class, "s1"), (short) 6, (short) 61) + " " + g.s0 + "," + g.s1);
        long m2 = u.allocateMemory(16);
        u.putLong(m2, 0L);
        row("cae raw", u.compareAndExchangeByte(null, m2 + 5, (byte) 0, (byte) 9) + " " + Long.toHexString(u.getLong(m2)));
        u.freeMemory(m2);
        int[] ia2 = new int[2];
        row("cae byte in int[]", u.compareAndExchangeByte(ia2, u.arrayBaseOffset(int[].class) + 2, (byte) 0, (byte) 0x55) + " " + Integer.toHexString(ia2[0]));
        row("allocateMemory neg", tryAlloc(-1));
        row("allocateMemory zero", tryAlloc(0));
        row("allocateMemory 8", tryAlloc(8));
    }
    static String tryAlloc(long n) {
        try { Unsafe u = Unsafe.getUnsafe(); long p = u.allocateMemory(n); if (p != 0) { u.setMemory(p, Math.max(n, 0), (byte) 1); u.freeMemory(p); } return "ok " + (p != 0); }
        catch (Throwable t) { return t.getClass().getName() + ": " + t.getMessage(); }
    }
}
