import java.lang.reflect.Field;
import jdk.internal.misc.Unsafe;

/**
 * L5 -- the sub-word atomic family at all four byte positions of a word.
 *
 * Reached with (on `javac` AND on `java`, both VMs):
 *
 *   --add-exports java.base/jdk.internal.misc=ALL-UNNAMED
 *
 * Check the `rows` trailer before believing a clean diff -- this file does not
 * compile without the flag, and two empty outputs diff clean.
 *
 * # Why four positions and not one
 *
 * `jdk.internal.misc.Unsafe` implements `compareAndSetByte`,
 * `compareAndExchangeByte`, `getAndAddByte` and the `short`/`char`/`boolean`
 * family IN JAVA, over a 4-byte CAS:
 *
 *   long wordOffset = offset & ~3;
 *   int  shift      = (int)(offset & 3) << 3;   // 24 - shift when BE
 *   ...  getIntVolatile(o, wordOffset) ... weakCompareAndSetInt(...)
 *
 * A probe that exercises ONE byte offset tests the one position where
 * `offset & 3 == 0` and the masking is the identity -- the case that works by
 * accident. So every sub-word row below is asked at four ADJACENT fields and
 * four ADJACENT array indices, and every row prints all four values, because
 * the failure mode of wrong shift arithmetic is a correct answer at the target
 * and a clobbered NEIGHBOUR.
 *
 * # What is deliberately not printed
 *
 * No offsets. `objectFieldOffset` is a VM's own numbering -- CratonVM returns
 * a SLOT INDEX where HotSpot returns a byte offset -- so printing one makes
 * every row differ for a reason that is not a defect. What is printed is what
 * the JDK's own callers observe: the boolean the CAS returned, the value
 * exchanged back, and whether the neighbours moved.
 */
public class L5SubwordAtomics {
    static final Unsafe U = Unsafe.getUnsafe();
    static int rows;

    /** Four adjacent fields of each sub-word width. */
    public static class Holder {
        public byte b0 = 10;
        public byte b1 = 11;
        public byte b2 = 12;
        public byte b3 = 13;
        public short s0 = 100;
        public short s1 = 101;
        public short s2 = 102;
        public short s3 = 103;
        public char c0 = 'a';
        public char c1 = 'b';
        public char c2 = 'c';
        public char c3 = 'd';
        public boolean z0 = false;
        public boolean z1 = true;
        public boolean z2 = false;
        public boolean z3 = true;
    }

    static long off(String name) {
        try {
            Field f = Holder.class.getField(name);
            return U.objectFieldOffset(f);
        } catch (Exception e) {
            return -1L;
        }
    }

    static void say(String s) {
        rows++;
        System.out.println(s);
    }

    static String bytes(Holder h) {
        return h.b0 + "," + h.b1 + "," + h.b2 + "," + h.b3;
    }

    static String shorts(Holder h) {
        return h.s0 + "," + h.s1 + "," + h.s2 + "," + h.s3;
    }

    static String chars(Holder h) {
        return h.c0 + "," + h.c1 + "," + h.c2 + "," + h.c3;
    }

    static String bools(Holder h) {
        return h.z0 + "," + h.z1 + "," + h.z2 + "," + h.z3;
    }

    // ---- byte fields, four positions ----------------------------------
    static void byteFields() {
        String[] n = {"b0", "b1", "b2", "b3"};
        byte[] init = {10, 11, 12, 13};
        for (int i = 0; i < 4; i++) {
            Holder h = new Holder();
            boolean ok = U.compareAndSetByte(h, off(n[i]), init[i], (byte) 99);
            say("casByteField pos=" + i + " ok=" + ok + " all=" + bytes(h));
        }
        for (int i = 0; i < 4; i++) {
            Holder h = new Holder();
            boolean ok = U.compareAndSetByte(h, off(n[i]), (byte) 77, (byte) 99);
            say("casByteFieldWrongWitness pos=" + i + " ok=" + ok + " all=" + bytes(h));
        }
        for (int i = 0; i < 4; i++) {
            Holder h = new Holder();
            byte prev = U.compareAndExchangeByte(h, off(n[i]), init[i], (byte) 99);
            say("caeByteField pos=" + i + " prev=" + prev + " all=" + bytes(h));
        }
        for (int i = 0; i < 4; i++) {
            Holder h = new Holder();
            byte prev = U.compareAndExchangeByte(h, off(n[i]), (byte) 77, (byte) 99);
            say("caeByteFieldWrongWitness pos=" + i + " prev=" + prev + " all=" + bytes(h));
        }
        for (int i = 0; i < 4; i++) {
            Holder h = new Holder();
            byte prev = U.getAndAddByte(h, off(n[i]), (byte) 5);
            say("getAndAddByteField pos=" + i + " prev=" + prev + " all=" + bytes(h));
        }
        for (int i = 0; i < 4; i++) {
            Holder h = new Holder();
            byte prev = U.getAndSetByte(h, off(n[i]), (byte) 42);
            say("getAndSetByteField pos=" + i + " prev=" + prev + " all=" + bytes(h));
        }
        for (int i = 0; i < 4; i++) {
            Holder h = new Holder();
            byte prev = U.getAndBitwiseOrByte(h, off(n[i]), (byte) 0x40);
            say("getAndBitwiseOrByteField pos=" + i + " prev=" + prev + " all=" + bytes(h));
        }
        for (int i = 0; i < 4; i++) {
            Holder h = new Holder();
            boolean ok = U.weakCompareAndSetByte(h, off(n[i]), init[i], (byte) 99);
            say("weakCasByteField pos=" + i + " ok=" + ok + " all=" + bytes(h));
        }
        for (int i = 0; i < 4; i++) {
            Holder h = new Holder();
            byte got = U.getByteVolatile(h, off(n[i]));
            U.putByteVolatile(h, off(n[i]), (byte) 55);
            say("volatileByteField pos=" + i + " got=" + got + " all=" + bytes(h));
        }
    }

    // ---- boolean fields (compareAndSetBoolean delegates to Byte) -------
    static void booleanFields() {
        String[] n = {"z0", "z1", "z2", "z3"};
        boolean[] init = {false, true, false, true};
        for (int i = 0; i < 4; i++) {
            Holder h = new Holder();
            boolean ok = U.compareAndSetBoolean(h, off(n[i]), init[i], !init[i]);
            say("casBooleanField pos=" + i + " ok=" + ok + " all=" + bools(h));
        }
        for (int i = 0; i < 4; i++) {
            Holder h = new Holder();
            boolean ok = U.compareAndSetBoolean(h, off(n[i]), !init[i], !init[i]);
            say("casBooleanFieldWrongWitness pos=" + i + " ok=" + ok + " all=" + bools(h));
        }
        for (int i = 0; i < 4; i++) {
            Holder h = new Holder();
            boolean prev = U.getAndSetBoolean(h, off(n[i]), true);
            say("getAndSetBooleanField pos=" + i + " prev=" + prev + " all=" + bools(h));
        }
    }

    // ---- short and char fields ----------------------------------------
    static void shortCharFields() {
        String[] sn = {"s0", "s1", "s2", "s3"};
        short[] si = {100, 101, 102, 103};
        for (int i = 0; i < 4; i++) {
            Holder h = new Holder();
            boolean ok = U.compareAndSetShort(h, off(sn[i]), si[i], (short) 999);
            say("casShortField pos=" + i + " ok=" + ok + " all=" + shorts(h));
        }
        for (int i = 0; i < 4; i++) {
            Holder h = new Holder();
            boolean ok = U.compareAndSetShort(h, off(sn[i]), (short) 7, (short) 999);
            say("casShortFieldWrongWitness pos=" + i + " ok=" + ok + " all=" + shorts(h));
        }
        for (int i = 0; i < 4; i++) {
            Holder h = new Holder();
            short prev = U.compareAndExchangeShort(h, off(sn[i]), si[i], (short) 999);
            say("caeShortField pos=" + i + " prev=" + prev + " all=" + shorts(h));
        }
        for (int i = 0; i < 4; i++) {
            Holder h = new Holder();
            short prev = U.getAndAddShort(h, off(sn[i]), (short) 5);
            say("getAndAddShortField pos=" + i + " prev=" + prev + " all=" + shorts(h));
        }
        for (int i = 0; i < 4; i++) {
            Holder h = new Holder();
            short prev = U.getAndSetShort(h, off(sn[i]), (short) 42);
            say("getAndSetShortField pos=" + i + " prev=" + prev + " all=" + shorts(h));
        }
        String[] cn = {"c0", "c1", "c2", "c3"};
        char[] ci = {'a', 'b', 'c', 'd'};
        for (int i = 0; i < 4; i++) {
            Holder h = new Holder();
            boolean ok = U.compareAndSetChar(h, off(cn[i]), ci[i], 'Z');
            say("casCharField pos=" + i + " ok=" + ok + " all=" + chars(h));
        }
        for (int i = 0; i < 4; i++) {
            Holder h = new Holder();
            boolean ok = U.compareAndSetChar(h, off(cn[i]), 'q', 'Z');
            say("casCharFieldWrongWitness pos=" + i + " ok=" + ok + " all=" + chars(h));
        }
        for (int i = 0; i < 4; i++) {
            Holder h = new Holder();
            char prev = U.compareAndExchangeChar(h, off(cn[i]), ci[i], 'Z');
            say("caeCharField pos=" + i + " prev=" + prev + " all=" + chars(h));
        }
        for (int i = 0; i < 4; i++) {
            Holder h = new Holder();
            char prev = U.getAndSetChar(h, off(cn[i]), 'Z');
            say("getAndSetCharField pos=" + i + " prev=" + prev + " all=" + chars(h));
        }
        for (int i = 0; i < 4; i++) {
            Holder h = new Holder();
            char prev = U.getAndAddChar(h, off(cn[i]), (char) 1);
            say("getAndAddCharField pos=" + i + " prev=" + prev + " all=" + chars(h));
        }
    }

    // ---- array elements, four positions of a word ----------------------
    static String arr(byte[] a) {
        return a[0] + "," + a[1] + "," + a[2] + "," + a[3];
    }

    static String arr(short[] a) {
        return a[0] + "," + a[1] + "," + a[2] + "," + a[3];
    }

    static String arr(char[] a) {
        return a[0] + "," + a[1] + "," + a[2] + "," + a[3];
    }

    static String arr(boolean[] a) {
        return a[0] + "," + a[1] + "," + a[2] + "," + a[3];
    }

    static void arrays() {
        long bb = U.arrayBaseOffset(byte[].class);
        long bs = U.arrayIndexScale(byte[].class);
        for (int i = 0; i < 4; i++) {
            byte[] a = {10, 11, 12, 13};
            boolean ok = U.compareAndSetByte(a, bb + i * bs, a[i], (byte) 99);
            say("casByteArray idx=" + i + " ok=" + ok + " all=" + arr(a));
        }
        for (int i = 0; i < 4; i++) {
            byte[] a = {10, 11, 12, 13};
            boolean ok = U.compareAndSetByte(a, bb + i * bs, (byte) 77, (byte) 99);
            say("casByteArrayWrongWitness idx=" + i + " ok=" + ok + " all=" + arr(a));
        }
        for (int i = 0; i < 4; i++) {
            byte[] a = {10, 11, 12, 13};
            byte prev = U.compareAndExchangeByte(a, bb + i * bs, a[i], (byte) 99);
            say("caeByteArray idx=" + i + " prev=" + prev + " all=" + arr(a));
        }
        for (int i = 0; i < 4; i++) {
            byte[] a = {10, 11, 12, 13};
            byte prev = U.getAndAddByte(a, bb + i * bs, (byte) 5);
            say("getAndAddByteArray idx=" + i + " prev=" + prev + " all=" + arr(a));
        }
        for (int i = 0; i < 4; i++) {
            byte[] a = {10, 11, 12, 13};
            byte prev = U.getAndSetByte(a, bb + i * bs, (byte) 42);
            say("getAndSetByteArray idx=" + i + " prev=" + prev + " all=" + arr(a));
        }
        long zb = U.arrayBaseOffset(boolean[].class);
        long zs = U.arrayIndexScale(boolean[].class);
        for (int i = 0; i < 4; i++) {
            boolean[] a = {false, true, false, true};
            boolean ok = U.compareAndSetBoolean(a, zb + i * zs, a[i], !a[i]);
            say("casBooleanArray idx=" + i + " ok=" + ok + " all=" + arr(a));
        }
        long sb = U.arrayBaseOffset(short[].class);
        long ss = U.arrayIndexScale(short[].class);
        for (int i = 0; i < 4; i++) {
            short[] a = {100, 101, 102, 103};
            boolean ok = U.compareAndSetShort(a, sb + i * ss, a[i], (short) 999);
            say("casShortArray idx=" + i + " ok=" + ok + " all=" + arr(a));
        }
        for (int i = 0; i < 4; i++) {
            short[] a = {100, 101, 102, 103};
            short prev = U.getAndAddShort(a, sb + i * ss, (short) 5);
            say("getAndAddShortArray idx=" + i + " prev=" + prev + " all=" + arr(a));
        }
        for (int i = 0; i < 4; i++) {
            short[] a = {100, 101, 102, 103};
            short prev = U.getAndSetShort(a, sb + i * ss, (short) 42);
            say("getAndSetShortArray idx=" + i + " prev=" + prev + " all=" + arr(a));
        }
        long cb = U.arrayBaseOffset(char[].class);
        long cs = U.arrayIndexScale(char[].class);
        for (int i = 0; i < 4; i++) {
            char[] a = {'a', 'b', 'c', 'd'};
            boolean ok = U.compareAndSetChar(a, cb + i * cs, a[i], 'Z');
            say("casCharArray idx=" + i + " ok=" + ok + " all=" + arr(a));
        }
        for (int i = 0; i < 4; i++) {
            char[] a = {'a', 'b', 'c', 'd'};
            char prev = U.getAndSetChar(a, cb + i * cs, 'Z');
            say("getAndSetCharArray idx=" + i + " prev=" + prev + " all=" + arr(a));
        }
    }

    public static void main(String[] args) {
        byteFields();
        booleanFields();
        shortCharFields();
        arrays();
        System.out.println("rows " + rows);
        System.out.println("DONE L5SubwordAtomics");
    }
}
