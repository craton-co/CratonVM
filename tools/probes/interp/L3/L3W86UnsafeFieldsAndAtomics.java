/*
 * Interpreter round i4, wave 86, lane L3: jdk.internal.misc.Unsafe and
 * sun.misc.Unsafe as the interpreter serves them.
 *
 * Rows:
 *  - sun.misc refusals of a record's / hidden class's field (objectFieldOffset,
 *    staticFieldOffset, staticFieldBase): UnsupportedOperationException
 *    "can't get <field offset|base address> on a <hidden|record> class: " + f.
 *    CratonVM's text lacked ": " + f, tested record before hidden, and the two
 *    static accessors did not refuse. (The hidden class's /0x suffix is
 *    masked; it is an address.)
 *  - the internal spelling accepts a record's field;
 *  - offset of a static / base of an instance field / a null Field: HotSpot's
 *    bare IllegalArgumentException / NullPointerException (null messages);
 *  - CAS / CAE / getAndAdd / getAndSet on byte, short and char FIELDS. A char
 *    of 0x8000 or more failed (the Short natives compared the zero-extended
 *    slot with the sign-extended short) while compareAndSetChar still
 *    answered true;
 *  - array base / scale for the primitive arrays, int reads of a long[]
 *    element, unaligned big/little-endian reads of a byte[];
 *  - allocateInstance refusals and its no-constructor / class-init rule;
 *  - raw memory: allocate / put / get / setMemory / copyMemory / realloc /
 *    free and allocateMemory(-1);
 *  - throwException.
 *
 * Run (both modes must print the HotSpot lines):
 *   javac --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -d out L3W86UnsafeFieldsAndAtomics.java
 *   cratonvm --jdk-only   --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp out L3W86UnsafeFieldsAndAtomics
 *   cratonvm --compatible --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp out L3W86UnsafeFieldsAndAtomics
 *   java                  --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp out L3W86UnsafeFieldsAndAtomics
 * (sun.misc.Unsafe prints its JDK 25 deprecation warning on stderr only.)
 *
 */
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// sun-record-offset: java.lang.UnsupportedOperationException: can't get field offset on a record class: private final int L3W86UnsafeFieldsAndAtomics$Rec.x
// sun-record-static-offset: java.lang.UnsupportedOperationException: can't get field offset on a record class: static int L3W86UnsafeFieldsAndAtomics$Rec.S
// sun-record-static-base: java.lang.UnsupportedOperationException: can't get base address on a record class: static int L3W86UnsafeFieldsAndAtomics$Rec.S
// sun-hidden-offset: java.lang.UnsupportedOperationException: can't get field offset on a hidden class: int L3W86UnsafeFieldsAndAtomics$HiddenSrc/0x.v
// sun-hidden-static-base: java.lang.UnsupportedOperationException: can't get base address on a hidden class: static int L3W86UnsafeFieldsAndAtomics$HiddenSrc/0x.SV
// internal-record-offset: ok
// static-as-instance: java.lang.IllegalArgumentException: null
// instance-as-static: java.lang.IllegalArgumentException: null
// instance-base: java.lang.IllegalArgumentException: null
// null-field: java.lang.NullPointerException: null
// byte: true 5 | 5 -128 | -128 -127 | false -127
// short: true 32767 | 32767 -32768 | -32768 -32765
// char: true 32769 | false 32769 | 32769 65 | 65 64 | 64 36864
// bool: true true | false true
// base: 16 16 16 16 16 16 16 16
// scale: 1 1 2 2 4 4 8 8
// long-as-ints: 55667788 11223344
// unaligned: 2030405 5040302 b0a090807060504 405060708090a0b
// abstract: java.lang.InstantiationException: L3W86UnsafeFieldsAndAtomics$Abs
// interface: java.lang.InstantiationException: java.lang.Runnable
// allocate: ctorRuns=0 clinitRan=true v=0
// memory: 55667788 11 abababababababab abababababababab 7788 0
// allocate-negative: java.lang.IllegalArgumentException: null
// allocate-zero: 0
// throw: java.io.IOException: thrown
// init: true true false
// park: returned
// HOTSPOT_EXPECTED_END
import java.io.IOException;
import java.io.InputStream;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Field;

public class L3W86UnsafeFieldsAndAtomics {
    record Rec(int x) {
        static int S = 1;
    }

    static class HiddenSrc {
        int v;
        static int SV;
    }

    static class Cells {
        byte b = -1;
        short s = Short.MAX_VALUE;
        char c = '￿';
        boolean z = false;
    }

    abstract static class Abs {}

    static int ctorRuns;
    static boolean clinitRan;

    static class Made {
        static {
            clinitRan = true;
        }

        int v = 5;

        Made() {
            ctorRuns++;
        }
    }

    static boolean lateRan;

    static class Late {
        static {
            lateRan = true;
        }
    }

    static final jdk.internal.misc.Unsafe U = jdk.internal.misc.Unsafe.getUnsafe();
    static sun.misc.Unsafe S;

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out.replaceAll("/0x[0-9a-fA-F]+", "/0x"));
    }

    public static void main(String[] args) throws Throwable {
        Field theUnsafe = sun.misc.Unsafe.class.getDeclaredField("theUnsafe");
        theUnsafe.setAccessible(true);
        S = (sun.misc.Unsafe) theUnsafe.get(null);

        Field recX = Rec.class.getDeclaredField("x");
        Field recS = Rec.class.getDeclaredField("S");
        row("sun-record-offset", () -> S.objectFieldOffset(recX));
        row("sun-record-static-offset", () -> S.staticFieldOffset(recS));
        row("sun-record-static-base", () -> S.staticFieldBase(recS));

        byte[] hb;
        try (InputStream in = L3W86UnsafeFieldsAndAtomics.class
                .getResourceAsStream("L3W86UnsafeFieldsAndAtomics$HiddenSrc.class")) {
            hb = in.readAllBytes();
        }
        Class<?> hidden = MethodHandles.lookup().defineHiddenClass(hb, true).lookupClass();
        Field hv = hidden.getDeclaredField("v");
        Field hsv = hidden.getDeclaredField("SV");
        row("sun-hidden-offset", () -> S.objectFieldOffset(hv));
        row("sun-hidden-static-base", () -> S.staticFieldBase(hsv));

        row("internal-record-offset", () -> {
            U.objectFieldOffset(recX);
            return "ok";
        });
        row("static-as-instance", () -> U.objectFieldOffset(recS));
        row("instance-as-static", () -> U.staticFieldOffset(recX));
        row("instance-base", () -> U.staticFieldBase(recX));
        row("null-field", () -> U.objectFieldOffset((Field) null));

        Cells o = new Cells();
        long bo = U.objectFieldOffset(Cells.class.getDeclaredField("b"));
        long so = U.objectFieldOffset(Cells.class.getDeclaredField("s"));
        long co = U.objectFieldOffset(Cells.class.getDeclaredField("c"));
        long zo = U.objectFieldOffset(Cells.class.getDeclaredField("z"));

        StringBuilder sb = new StringBuilder();
        sb.append(U.compareAndSetByte(o, bo, (byte) -1, (byte) 5)).append(' ').append(o.b);
        sb.append(" | ").append(U.compareAndExchangeByte(o, bo, (byte) 5, (byte) -128)).append(' ').append(o.b);
        sb.append(" | ").append(U.getAndAddByte(o, bo, (byte) 1)).append(' ').append(o.b);
        sb.append(" | ").append(U.compareAndSetByte(o, bo, (byte) 5, (byte) 6)).append(' ').append(o.b);
        System.out.println("byte: " + sb);

        sb.setLength(0);
        sb.append(U.compareAndSetShort(o, so, Short.MAX_VALUE, Short.MAX_VALUE)).append(' ').append(o.s);
        sb.append(" | ").append(U.compareAndExchangeShort(o, so, Short.MAX_VALUE, Short.MIN_VALUE)).append(' ').append(o.s);
        sb.append(" | ").append(U.getAndAddShort(o, so, (short) 3)).append(' ').append(o.s);
        System.out.println("short: " + sb);

        sb.setLength(0);
        sb.append(U.compareAndSetChar(o, co, '￿', '老')).append(' ').append((int) o.c);
        sb.append(" | ").append(U.compareAndSetChar(o, co, '￿', 'X')).append(' ').append((int) o.c);
        sb.append(" | ").append((int) U.compareAndExchangeChar(o, co, '老', 'A')).append(' ').append((int) o.c);
        sb.append(" | ").append((int) U.getAndAddChar(o, co, '￿')).append(' ').append((int) o.c);
        sb.append(" | ").append((int) U.getAndSetChar(o, co, '退')).append(' ').append((int) o.c);
        System.out.println("char: " + sb);

        sb.setLength(0);
        sb.append(U.compareAndSetBoolean(o, zo, false, true)).append(' ').append(o.z);
        sb.append(" | ").append(U.compareAndSetBoolean(o, zo, false, true)).append(' ').append(o.z);
        System.out.println("bool: " + sb);

        Class<?>[] arrays = {boolean[].class, byte[].class, short[].class, char[].class,
                int[].class, float[].class, long[].class, double[].class};
        sb.setLength(0);
        for (Class<?> a : arrays) {
            sb.append(sb.length() == 0 ? "" : " ").append(U.arrayBaseOffset(a));
        }
        System.out.println("base: " + sb);
        sb.setLength(0);
        for (Class<?> a : arrays) {
            sb.append(sb.length() == 0 ? "" : " ").append(U.arrayIndexScale(a));
        }
        System.out.println("scale: " + sb);

        long[] la = {0x1122334455667788L};
        long lbase = U.arrayBaseOffset(long[].class);
        System.out.println("long-as-ints: " + Integer.toHexString(U.getInt(la, lbase))
                + " " + Integer.toHexString(U.getInt(la, lbase + 4)));

        byte[] bytes = new byte[16];
        for (int i = 0; i < bytes.length; i++) {
            bytes[i] = (byte) (i + 1);
        }
        long bbase = U.arrayBaseOffset(byte[].class);
        System.out.println("unaligned: "
                + Integer.toHexString(U.getIntUnaligned(bytes, bbase + 1, true)) + " "
                + Integer.toHexString(U.getIntUnaligned(bytes, bbase + 1, false)) + " "
                + Long.toHexString(U.getLongUnaligned(bytes, bbase + 3, false)) + " "
                + Long.toHexString(U.getLongUnaligned(bytes, bbase + 3, true)));

        row("abstract", () -> U.allocateInstance(Abs.class));
        row("interface", () -> U.allocateInstance(Runnable.class));
        Made m = (Made) U.allocateInstance(Made.class);
        System.out.println("allocate: ctorRuns=" + ctorRuns + " clinitRan=" + clinitRan + " v=" + m.v);

        long a = U.allocateMemory(16);
        U.putLong(a, 0x1122334455667788L);
        String r1 = Integer.toHexString(U.getInt(a)) + " " + Integer.toHexString(U.getByte(a + 7));
        U.setMemory(a, 16, (byte) 0xAB);
        String r2 = Long.toHexString(U.getLong(a + 8));
        long b = U.allocateMemory(16);
        U.copyMemory(a, b, 16);
        String r3 = Long.toHexString(U.getLong(b));
        U.putShort(b, (short) 0x7788);
        b = U.reallocateMemory(b, 64);
        String r4 = Integer.toHexString(U.getShort(b) & 0xFFFF);
        U.freeMemory(a);
        long z = U.reallocateMemory(b, 0);
        System.out.println("memory: " + r1 + " " + r2 + " " + r3 + " " + r4 + " " + z);
        row("allocate-negative", () -> U.allocateMemory(-1));
        row("allocate-zero", () -> U.allocateMemory(0));

        row("throw", () -> {
            S.throwException(new IOException("thrown"));
            return "returned";
        });

        boolean before = U.shouldBeInitialized(Late.class);
        U.ensureClassInitialized(Late.class);
        System.out.println("init: " + before + " " + lateRan + " " + U.shouldBeInitialized(Late.class));

        // A pending unpark makes the next park return at once.
        U.unpark(Thread.currentThread());
        U.park(false, 0L);
        System.out.println("park: returned");
    }
}
