import java.lang.reflect.Field;
import jdk.internal.misc.Unsafe;

/**
 * SBR3 UNSAFECAS: jdk.internal.misc.Unsafe sub-word CAS on object fields and arrays, called directly.
 * Compile and run with --add-exports java.base/jdk.internal.misc=ALL-UNNAMED.
 */
public class Sbr3UnsafecasCae {
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
        long so = u.staticFieldOffset(Sbr3UnsafecasCae.class.getDeclaredField("sb"));
        Object sbase = u.staticFieldBase(Sbr3UnsafecasCae.class.getDeclaredField("sb"));
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
        extra(u, ab, so, sbase);
        row("allocateMemory neg", tryAlloc(-1));
        row("allocateMemory zero", tryAlloc(0));
        row("allocateMemory 8", tryAlloc(8));
    }
    static volatile int sink;
    static void extra(Unsafe u, long ab, long so, Object sbase) throws Throwable {
        // byte lanes of other element widths
        short[] sa = new short[2];
        row("cae byte in short[]", u.compareAndExchangeByte(sa, u.arrayBaseOffset(short[].class) + 1, (byte) 0, (byte) 0x12) + " " + Integer.toHexString(sa[0] & 0xffff));
        row("cas byte in short[] miss", u.compareAndSetByte(sa, u.arrayBaseOffset(short[].class) + 1, (byte) 0, (byte) 0x13) + " " + Integer.toHexString(sa[0] & 0xffff));
        char[] ca = new char[2];
        row("cae byte in char[]", u.compareAndExchangeByte(ca, u.arrayBaseOffset(char[].class) + 1, (byte) 0, (byte) 0x7e) + " " + Integer.toHexString(ca[0]));
        long[] la = new long[2];
        row("cae byte in long[] lane7", u.compareAndExchangeByte(la, u.arrayBaseOffset(long[].class) + 7, (byte) 0, (byte) 0x41) + " " + Long.toHexString(la[0]));
        row("cae byte in long[] lane2", u.compareAndExchangeByte(la, u.arrayBaseOffset(long[].class) + 2, (byte) 0, (byte) 0x42) + " " + Long.toHexString(la[0]));
        int[] ia = new int[3];
        ia[1] = 0x11223344;
        row("cae byte in int[] hit", u.compareAndExchangeByte(ia, u.arrayBaseOffset(int[].class) + 4, (byte) 0x44, (byte) -1) + " " + Integer.toHexString(ia[1]));
        row("cae byte in int[] lane3 neg", u.compareAndExchangeByte(ia, u.arrayBaseOffset(int[].class) + 7, (byte) 0x11, (byte) -2) + " " + Integer.toHexString(ia[1]));
        row("cae byte in int[] miss witness", u.compareAndExchangeByte(ia, u.arrayBaseOffset(int[].class) + 5, (byte) 0, (byte) 9) + " " + Integer.toHexString(ia[1]));
        row("cas short in int[] hi", u.compareAndSetShort(ia, u.arrayBaseOffset(int[].class) + 6, (short) 0xfffffffe, (short) 0x7777) + " " + Integer.toHexString(ia[1]));
        row("cae short in int[] lo", u.compareAndExchangeShort(ia, u.arrayBaseOffset(int[].class) + 4, (short) 0x33ff, (short) 0x5555) + " " + Integer.toHexString(ia[1]));
        row("cae short in int[] lo hit", u.compareAndExchangeShort(ia, u.arrayBaseOffset(int[].class) + 4, (short) 0x33ff, (short) 0x5555) + " " + Integer.toHexString(ia[1]));
        row("cae short in long[]", u.compareAndExchangeShort(la, u.arrayBaseOffset(long[].class) + 8 + 4, (short) 0, (short) 0x1234) + " " + Long.toHexString(la[1]));
        byte[] ba = new byte[8];
        row("cas short in byte[]", u.compareAndSetShort(ba, ab + 2, (short) 0, (short) 0x0102) + " " + java.util.Arrays.toString(ba));
        row("cae short in byte[] miss", u.compareAndExchangeShort(ba, ab + 2, (short) 0, (short) 5) + " " + java.util.Arrays.toString(ba));
        row("cae short in short[]", u.compareAndExchangeShort(sa, u.arrayBaseOffset(short[].class) + 2, (short) 0, (short) -3) + " " + sa[1]);
        row("cae char in char[]", u.compareAndExchangeChar(ca, u.arrayBaseOffset(char[].class) + 2, (char) 0, (char) 0xfffe) + " " + (int) ca[1]);
        boolean[] za = new boolean[4];
        row("cae bool in boolean[]", u.compareAndExchangeBoolean(za, ab + 1, false, true) + " " + java.util.Arrays.toString(za));
        // negative byte field values, sign extension of the witness
        H n = new H();
        n.b2 = (byte) 0x90;
        row("cae neg field", u.compareAndExchangeByte(n, u.objectFieldOffset(H.class, "b2"), (byte) 0x90, (byte) 0x91) + " " + n.b2);
        row("cae neg field miss", u.compareAndExchangeByte(n, u.objectFieldOffset(H.class, "b2"), (byte) 0x7, (byte) 0x92) + " " + n.b2);
        // getAndSet / bitwise at every shape (hang guard is the harness timeout)
        long m = u.allocateMemory(16);
        u.putLong(m, 0x0807060504030201L);
        row("getAndSetByte raw", u.getAndSetByte(null, m + 2, (byte) 0x55) + " " + Long.toHexString(u.getLong(m)));
        row("getAndSetShort raw", u.getAndSetShort(null, m + 4, (short) 0x1234) + " " + Long.toHexString(u.getLong(m)));
        row("cae short raw", u.compareAndExchangeShort(null, m + 6, (short) 0x0807, (short) 0x4321) + " " + Long.toHexString(u.getLong(m)));
        row("cas byte raw miss", u.compareAndSetByte(null, m + 7, (byte) 1, (byte) 2) + " " + Long.toHexString(u.getLong(m)));
        u.freeMemory(m);
        row("getAndSetByte int[]", u.getAndSetByte(ia, u.arrayBaseOffset(int[].class) + 8, (byte) 0x66) + " " + Integer.toHexString(ia[2]));
        row("getAndSetByte byte[]", u.getAndSetByte(ba, ab + 7, (byte) 0x66) + " " + java.util.Arrays.toString(ba));
        row("getAndBitwiseOrByte int[]", u.getAndBitwiseOrByte(ia, u.arrayBaseOffset(int[].class) + 9, (byte) 0x0f) + " " + Integer.toHexString(ia[2]));
        row("getAndSetBoolean field", u.getAndSetBoolean(n, u.objectFieldOffset(H.class, "z"), false) + " " + n.z);
        row("getAndSetChar field", u.getAndSetChar(n, u.objectFieldOffset(H.class, "c"), 'x') + " " + n.c);
        // plain and volatile sub-word access over the same lanes and raw memory
        int[] va = { 0x44332211, 0 };
        long vb = u.arrayBaseOffset(int[].class);
        row("getByte int[] lanes", u.getByte(va, vb) + "," + u.getByte(va, vb + 1) + "," + u.getByte(va, vb + 2) + "," + u.getByte(va, vb + 3));
        row("getByteVolatile int[] lane2", u.getByteVolatile(va, vb + 2));
        row("getShortVolatile int[] hi", Integer.toHexString(u.getShortVolatile(va, vb + 2)));
        row("getCharVolatile int[] lo", Integer.toHexString(u.getCharVolatile(va, vb)));
        u.putByteVolatile(va, vb + 1, (byte) 0x99);
        u.putByte(va, vb + 3, (byte) 0x88);
        row("putByte lanes", Integer.toHexString(va[0]));
        u.putShortVolatile(va, vb + 4, (short) 0xbeef);
        row("putShortVolatile int[] lo", Integer.toHexString(va[1]));
        long[] vl = new long[1];
        u.putByte(vl, u.arrayBaseOffset(long[].class) + 5, (byte) 0x7);
        row("putByte long[] lane5", Long.toHexString(vl[0]) + " " + u.getByte(vl, u.arrayBaseOffset(long[].class) + 5));
        row("getAndAddByte int[] ovf", u.getAndAddByte(va, vb + 1, (byte) 0x70) + " " + Integer.toHexString(va[0]));
        row("getAndAddShort int[]", u.getAndAddShort(va, vb + 2, (short) 1) + " " + Integer.toHexString(va[0]));
        n.b0 = 127;
        row("getAndAddByte field ovf", u.getAndAddByte(n, u.objectFieldOffset(H.class, "b0"), (byte) 1) + " " + n.b0);
        long vm = u.allocateMemory(8);
        u.putLong(vm, 0L);
        u.putByteVolatile(null, vm + 1, (byte) 0x5a);
        row("raw volatile byte", u.getByteVolatile(null, vm + 1) + " " + Long.toHexString(u.getLong(vm)));
        u.putShortVolatile(null, vm + 4, (short) 0x1234);
        row("raw volatile short", Integer.toHexString(u.getShortVolatile(null, vm + 4)) + " " + Long.toHexString(u.getLong(vm)));
        row("raw volatile char", Integer.toHexString(u.getCharVolatile(null, vm + 4)));
        u.freeMemory(vm);
        // contention: four threads, one byte each of the same int[] word, and of one object's bytes
        final int[] shared = new int[1];
        final H hh = new H();
        final long base = u.arrayBaseOffset(int[].class);
        final long[] fo = { u.objectFieldOffset(H.class, "b0"), u.objectFieldOffset(H.class, "b1"), u.objectFieldOffset(H.class, "b2"), u.objectFieldOffset(H.class, "b3") };
        hh.b0 = hh.b1 = hh.b2 = hh.b3 = 0;
        Thread[] ts = new Thread[4];
        for (int t = 0; t < 4; t++) {
            final int lane = t;
            ts[t] = new Thread(() -> {
                for (int i = 0; i < 20000; i++) {
                    byte cur;
                    do { cur = u.getByteVolatile(shared, base + lane); } while (!u.compareAndSetByte(shared, base + lane, cur, (byte) (cur + 1)));
                    byte c2;
                    do { c2 = u.getByteVolatile(hh, fo[lane]); } while (u.compareAndExchangeByte(hh, fo[lane], c2, (byte) (c2 + 1)) != c2);
                }
            });
            ts[t].start();
        }
        for (Thread t : ts) t.join();
        row("contended int[] word", Integer.toHexString(shared[0]));
        row("contended object bytes", hh.b0 + "," + hh.b1 + "," + hh.b2 + "," + hh.b3);
        // mixed: one thread does int CAS increments on the high half while others byte-CAS the low bytes
        final int[] mix = new int[1];
        Thread hi = new Thread(() -> { for (int i = 0; i < 20000; i++) { int c; do { c = u.getIntVolatile(mix, base); } while (!u.compareAndSetInt(mix, base, c, (c & 0x00ffffff) | (((c >>> 24) + 1) << 24))); } });
        Thread lo = new Thread(() -> { for (int i = 0; i < 20000; i++) { byte c; do { c = u.getByteVolatile(mix, base); } while (!u.compareAndSetByte(mix, base, c, (byte) (c + 1))); } });
        hi.start(); lo.start(); hi.join(); lo.join();
        row("contended mixed int/byte", Integer.toHexString(mix[0]));
    }
    static String tryAlloc(long n) {
        try { Unsafe u = Unsafe.getUnsafe(); long p = u.allocateMemory(n); if (p != 0) { u.setMemory(p, Math.max(n, 0), (byte) 1); u.freeMemory(p); } return "ok " + (p != 0); }
        catch (Throwable t) { return t.getClass().getName() + ": " + t.getMessage(); }
    }
}
