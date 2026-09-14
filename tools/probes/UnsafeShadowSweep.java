import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.List;

/** L1 -- the `sun/misc/Unsafe` (102) and `jdk/internal/misc/Unsafe` (120)
 *  bridge-with-code rows of the `--jdk-only` retirement surface, diffed against
 *  HotSpot in both modes.
 *
 *  UNSAFE IS DIFFED ON BEHAVIOUR, NEVER ON AN ADDRESS OR AN OFFSET. A field
 *  offset and a malloc address are implementation tokens: two VMs may legally
 *  disagree on their values and both be correct. What they must agree on is
 *  that a put at an offset is visible to the matching get, that a CAS with the
 *  wrong witness fails and leaves the slot alone, that a width writes exactly
 *  its own bytes, and that a refusal is the refusal the JDK specifies.
 *
 *  WHAT IS DELIBERATELY NOT PROBED, and why -- this lane is about memory
 *  safety, so a probe that crashes the ORACLE measures nothing:
 *    * a CAS or a get/put with a NULL base and an offset that is not a real
 *      address. On HotSpot a null base means an absolute address, so
 *      `compareAndSetInt(null, 0, 0, 1)` writes to address 0 and takes the JVM
 *      down. The null-base path IS probed -- against addresses that
 *      `allocateMemory` actually returned.
 *    * `freeMemory` of anything but 0 or a live allocation, and
 *      `reallocateMemory` of a stale one. Both are undefined behaviour in the
 *      JDK's own contract.
 *    * `writeback0` / `getUncompressedObject`, which take a raw address with no
 *      validation of any kind.
 *  Their registrations are reported by the reflective section, not exercised.
 *
 *  Methods the JDK may not declare at all on this image (`monitorEnter`,
 *  `defineAnonymousClass`, `defineClass`) are reached REFLECTIVELY, so their
 *  absence is a printed row rather than a link error that takes the probe down.
 */
public class UnsafeShadowSweep {

    // ------------------------------------------------------------------
    // harness
    // ------------------------------------------------------------------
    static int emitted = 0;

    static String esc(String s) {
        StringBuilder b = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }

    static void p(String tag, Object v) {
        emitted++;
        System.out.println(esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }

    /** Print the OUTCOME of a call, never a verdict about it. */
    static void t(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }

    /** Print a value or the throwable that replaced it, on one row. */
    static void tv(String tag, ThrowingSup r) {
        try { p(tag, r.get()); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }

    interface ThrowingRun { void run() throws Throwable; }
    interface ThrowingSup { Object get() throws Throwable; }

    /** Null and out-of-bounds arguments live in `UnsafeNullArgProbe`, one call
     *  per process. `sun.misc.Unsafe.allocateInstance(null)` SIGSEGVs HotSpot
     *  25.0.4+7 inside `Unsafe_AllocateInstance`, and
     *  `getLoadAverage(a, n > a.length)` has the JDK writing `n` doubles into a
     *  shorter array with no check anywhere on the path. Either one inside this
     *  sweep truncates it and reports every later section as a difference. */
    static void section(String name) {
        System.out.println("SECTION " + name + " at " + emitted);
    }

    // ------------------------------------------------------------------
    // subjects
    // ------------------------------------------------------------------
    static sun.misc.Unsafe u;
    static jdk.internal.misc.Unsafe v;

    static class Holder {
        boolean z = true;
        byte b = 1;
        char c = 'A';
        short s = 2;
        int i = 3;
        long l = 4L;
        float f = 5.5f;
        double d = 6.5;
        Object o = "init";
        final int fin = 77;
    }

    static class Sub extends Holder { int own = 9; }

    static class Statics {
        static boolean sz = true;
        static byte sb = 1;
        static char sc = 'B';
        static short ss = 2;
        static int si = 3;
        static long sl = 4L;
        static float sf = 5.5f;
        static double sd = 6.5;
        static Object so = "sinit";
    }

    static class Clinit1 { static boolean ran; static { ran = true; } }
    static class Clinit2 { static boolean ran; static { ran = true; } }
    static class Clinit3 { static boolean ran; static { ran = true; } }

    static class NoDefaultCtor {
        int a; String s;
        NoDefaultCtor(int a) { this.a = a; this.s = "ctor-ran"; }
    }
    abstract static class Abstract { int q; }
    interface Iface { }
    record Rec(int a, String b) { }
    enum En { X, Y }

    // ==================================================================
    // A. offsets: objectFieldOffset / staticFieldOffset / staticFieldBase
    // ==================================================================
    static void offsets() throws Exception {
        section("offsets");
        Field fi = Holder.class.getDeclaredField("i");
        Field fl = Holder.class.getDeclaredField("l");
        Field fo = Holder.class.getDeclaredField("o");
        Field ffin = Holder.class.getDeclaredField("fin");
        Field fsi = Statics.class.getDeclaredField("si");

        long oi = u.objectFieldOffset(fi);
        long ol = u.objectFieldOffset(fl);
        long oo = u.objectFieldOffset(fo);
        p("sun oFO distinct", oi != ol && ol != oo && oo != oi);
        p("sun oFO non-negative", oi >= 0 && ol >= 0 && oo >= 0);
        p("sun oFO on a final instance field is allowed", u.objectFieldOffset(ffin) >= 0);
        p("sun oFO stable across calls", u.objectFieldOffset(fi) == oi);
        t("sun oFO(static field)", () -> u.objectFieldOffset(fsi));
        t("sun oFO(record component)",
          () -> u.objectFieldOffset(Rec.class.getDeclaredField("a")));

        long vi = v.objectFieldOffset(fi);
        p("internal oFO non-negative", vi >= 0);
        p("internal oFO agrees with sun", vi == oi);
        t("internal oFO(static field)", () -> v.objectFieldOffset(fsi));
        t("internal oFO(record component)",
          () -> v.objectFieldOffset(Rec.class.getDeclaredField("a")));

        // objectFieldOffset(Class,String) -- the internal 2-arg door.
        tv("internal oFO(Class,String) equals the Field form",
           () -> v.objectFieldOffset(Holder.class, "i") == oi);
        t("internal oFO(Class,String) missing name",
          () -> v.objectFieldOffset(Holder.class, "nope"));
        t("internal oFO(Class,String) static name",
          () -> v.objectFieldOffset(Statics.class, "si"));

        // A HIDDEN class's field is the other half of the same refusal as the
        // record one. The bytes come from this probe's own class file, so no
        // fixture is needed and the two VMs are handed identical input.
        tv("oFO on a hidden class's field", () -> {
            byte[] bytes;
            try (java.io.InputStream in =
                     Holder.class.getResourceAsStream("UnsafeShadowSweep$Holder.class")) {
                if (in == null) return "NO CLASS BYTES";
                bytes = in.readAllBytes();
            }
            Class<?> hidden = java.lang.invoke.MethodHandles.lookup()
                .defineHiddenClass(bytes, false).lookupClass();
            return u.objectFieldOffset(hidden.getDeclaredField("i")) >= 0
                ? "an offset" : "a negative offset";
        });

        // An inherited field must resolve through the subclass too, and to the
        // SAME slot -- otherwise a subclass write lands somewhere else.
        Sub sub = new Sub();
        long subI = u.objectFieldOffset(Holder.class.getDeclaredField("i"));
        u.putInt(sub, subI, 4242);
        p("inherited field write is visible", sub.i);
        p("inherited write did not touch the subclass field", sub.own);

        // static offsets and bases
        long soff = u.staticFieldOffset(fsi);
        Object sbase = u.staticFieldBase(fsi);
        p("sun staticFieldOffset non-negative", soff >= 0);
        p("sun staticFieldBase non-null", sbase != null);
        u.putInt(sbase, soff, 31337);
        p("static write via base+offset visible to Java", Statics.si);
        p("static read via base+offset agrees", u.getInt(sbase, soff));
        Statics.si = 3;
        t("sun staticFieldOffset(instance field)", () -> u.staticFieldOffset(fi));
        t("sun staticFieldBase(instance field)", () -> u.staticFieldBase(fi));

        tv("internal staticFieldOffset agrees",
           () -> v.staticFieldOffset(fsi) == soff);
        tv("internal staticFieldBase agrees",
           () -> v.staticFieldBase(fsi) == sbase);
        t("internal staticFieldOffset(instance field)", () -> v.staticFieldOffset(fi));
        t("internal staticFieldBase(instance field)", () -> v.staticFieldBase(fi));
    }

    // ==================================================================
    // B. array base / scale
    // ==================================================================
    static void arrayShape() {
        section("array-shape");
        Class<?>[] cs = { boolean[].class, byte[].class, char[].class, short[].class,
                          int[].class, long[].class, float[].class, double[].class,
                          Object[].class, String[].class, int[][].class };
        for (Class<?> c : cs) {
            String n = c.getName();
            int sc = u.arrayIndexScale(c);
            p("sun scale " + n, sc);
            p("sun scale is a power of two " + n, sc > 0 && Integer.bitCount(sc) == 1);
            p("sun base positive " + n, u.arrayBaseOffset(c) > 0);
            p("internal scale agrees " + n, v.arrayIndexScale(c) == sc);
            p("internal base agrees " + n, v.arrayBaseOffset(c) == u.arrayBaseOffset(c));
        }
        // Reference arrays of different component types must share one scale --
        // they are all the same machine width.
        p("Object[] and String[] share a scale",
          u.arrayIndexScale(Object[].class) == u.arrayIndexScale(String[].class));
        p("int[][] scales as a reference array",
          u.arrayIndexScale(int[][].class) == u.arrayIndexScale(Object[].class));

        // Non-arrays. HotSpot's own refusal here is on record as broken (it
        // names java/lang/InvalidClassException, which does not exist), so this
        // prints the OUTCOME and the record adjudicates it.
        // ONE row each, value-or-throwable. Written with a nested `p` the
        // answering VM emitted two lines and the throwing VM one, which shifted
        // every later SECTION count and turned a six-row residual into a
        // thirty-line diff.
        tv("sun arrayIndexScale(String.class)", () -> u.arrayIndexScale(String.class));
        tv("sun arrayBaseOffset(String.class)", () -> u.arrayBaseOffset(String.class));
        tv("sun arrayIndexScale(int.class)", () -> u.arrayIndexScale(int.class));
        tv("sun arrayIndexScale(Iface.class)", () -> u.arrayIndexScale(Iface.class));
        tv("internal arrayIndexScale(String.class)", () -> v.arrayIndexScale(String.class));
        tv("internal arrayBaseOffset(String.class)", () -> v.arrayBaseOffset(String.class));
    }

    // ==================================================================
    // C. every width against a heap object field
    // ==================================================================
    static void heapFields() throws Exception {
        section("heap-fields");
        Holder h = new Holder();
        long oz = u.objectFieldOffset(Holder.class.getDeclaredField("z"));
        long ob = u.objectFieldOffset(Holder.class.getDeclaredField("b"));
        long oc = u.objectFieldOffset(Holder.class.getDeclaredField("c"));
        long os = u.objectFieldOffset(Holder.class.getDeclaredField("s"));
        long oi = u.objectFieldOffset(Holder.class.getDeclaredField("i"));
        long ol = u.objectFieldOffset(Holder.class.getDeclaredField("l"));
        long of = u.objectFieldOffset(Holder.class.getDeclaredField("f"));
        long od = u.objectFieldOffset(Holder.class.getDeclaredField("d"));
        long oo = u.objectFieldOffset(Holder.class.getDeclaredField("o"));

        p("read z", u.getBoolean(h, oz));
        p("read b", u.getByte(h, ob));
        p("read c", (int) u.getChar(h, oc));
        p("read s", u.getShort(h, os));
        p("read i", u.getInt(h, oi));
        p("read l", u.getLong(h, ol));
        p("read f", u.getFloat(h, of));
        p("read d", u.getDouble(h, od));
        p("read o", u.getObject(h, oo));

        // Extreme values: a width that silently sign- or zero-extends shows up
        // here and nowhere on the happy path.
        u.putBoolean(h, oz, false);          p("write z", h.z);
        u.putByte(h, ob, (byte) -128);       p("write b min", h.b);
        u.putByte(h, ob, (byte) 0xFF);       p("write b 0xFF", h.b);
        u.putChar(h, oc, '\uFFFF');          p("write c max", (int) h.c);
        u.putShort(h, os, (short) -32768);   p("write s min", h.s);
        u.putInt(h, oi, Integer.MIN_VALUE);  p("write i min", h.i);
        u.putLong(h, ol, Long.MIN_VALUE);    p("write l min", h.l);
        u.putFloat(h, of, Float.NaN);        p("write f NaN", Float.isNaN(h.f));
        u.putFloat(h, of, -0.0f);            p("write f -0.0 raw", Float.floatToRawIntBits(h.f));
        u.putDouble(h, od, Double.NaN);      p("write d NaN", Double.isNaN(h.d));
        u.putDouble(h, od, -0.0);            p("write d -0.0 raw", Double.doubleToRawLongBits(h.d));
        u.putObject(h, oo, null);            p("write o null", h.o);
        u.putObject(h, oo, "second");        p("write o", h.o);

        // The reads must see what Java sees, immediately after.
        p("readback b", u.getByte(h, ob));
        p("readback c", (int) u.getChar(h, oc));
        p("readback s", u.getShort(h, os));
        p("readback i", u.getInt(h, oi));
        p("readback l", u.getLong(h, ol));
        p("readback f raw", Float.floatToRawIntBits(u.getFloat(h, of)));
        p("readback d raw", Double.doubleToRawLongBits(u.getDouble(h, od)));
        p("readback o", u.getObject(h, oo));

        // A narrow write must not disturb the neighbouring fields. This is the
        // check that catches a `putByte` implemented as a slot-wide store.
        Holder n = new Holder();
        u.putByte(n, ob, (byte) 0x7F);
        p("narrow write left z", n.z);
        p("narrow write left c", (int) n.c);
        p("narrow write left s", n.s);
        p("narrow write left i", n.i);
        p("narrow write left l", n.l);
        u.putShort(n, os, (short) 0x1234);
        p("short write left b", n.b);
        p("short write left c", (int) n.c);
        p("short write left i", n.i);
        u.putChar(n, oc, '\u4321');
        p("char write left b", n.b);
        p("char write left s", n.s);

        // A final instance field is writable through Unsafe (this is what the
        // JDK's own deserialization does).
        long ofin = u.objectFieldOffset(Holder.class.getDeclaredField("fin"));
        t("write a final field", () -> u.putInt(n, ofin, 5));
        p("final field after write", n.fin);

        // internal Unsafe, same slots, `Reference` spelling.
        Holder g = new Holder();
        p("internal getInt", v.getInt(g, oi));
        v.putInt(g, oi, 12);           p("internal putInt", g.i);
        p("internal getReference", v.getReference(g, oo));
        v.putReference(g, oo, "vset"); p("internal putReference", g.o);
        p("internal getBoolean", v.getBoolean(g, oz));
        p("internal getByte", v.getByte(g, ob));
        p("internal getChar", (int) v.getChar(g, oc));
        p("internal getShort", v.getShort(g, os));
        p("internal getLong", v.getLong(g, ol));
        p("internal getFloat", v.getFloat(g, of));
        p("internal getDouble", v.getDouble(g, od));
    }

    // ==================================================================
    // D. every width against an array element
    // ==================================================================
    static void arrayElements() {
        section("array-elements");
        int ib = u.arrayBaseOffset(int[].class), is = u.arrayIndexScale(int[].class);
        int[] ia = { 10, 11, 12 };
        p("array read [1]", u.getInt(ia, ib + (long) is));
        u.putInt(ia, ib + 2L * is, 99);
        p("array write visible to Java", ia[2]);
        p("array write visible to unsafe", u.getInt(ia, ib + 2L * is));

        byte[] ba = { 1, 2, 3, 4 };
        int bb = u.arrayBaseOffset(byte[].class), bs = u.arrayIndexScale(byte[].class);
        u.putByte(ba, bb + 1L * bs, (byte) -1);
        p("byte[] write", java.util.Arrays.toString(ba));

        char[] ca = { 'a', 'b', 'c' };
        int cb = u.arrayBaseOffset(char[].class), cs = u.arrayIndexScale(char[].class);
        u.putChar(ca, cb + 1L * cs, 'Z');
        p("char[] write", new String(ca));

        short[] sa = { 1, 2, 3 };
        int sb = u.arrayBaseOffset(short[].class), ss = u.arrayIndexScale(short[].class);
        u.putShort(sa, sb + 2L * ss, (short) -5);
        p("short[] write", java.util.Arrays.toString(sa));

        long[] la = { 1L, 2L };
        int lb = u.arrayBaseOffset(long[].class), ls = u.arrayIndexScale(long[].class);
        u.putLong(la, lb + 1L * ls, Long.MIN_VALUE);
        p("long[] write", java.util.Arrays.toString(la));

        float[] fa = { 1f, 2f };
        int fb = u.arrayBaseOffset(float[].class), fs = u.arrayIndexScale(float[].class);
        u.putFloat(fa, fb + 1L * fs, -0.0f);
        p("float[] write raw", Float.floatToRawIntBits(fa[1]));

        double[] da = { 1d, 2d };
        int db = u.arrayBaseOffset(double[].class), ds = u.arrayIndexScale(double[].class);
        u.putDouble(da, db + 0L, Double.NaN);
        p("double[] write NaN", Double.isNaN(da[0]));

        boolean[] za = { true, false };
        int zb = u.arrayBaseOffset(boolean[].class), zs = u.arrayIndexScale(boolean[].class);
        u.putBoolean(za, zb + 1L * zs, true);
        p("boolean[] write", java.util.Arrays.toString(za));

        Object[] oa = { "a", "b" };
        int ob2 = u.arrayBaseOffset(Object[].class), os2 = u.arrayIndexScale(Object[].class);
        u.putObject(oa, ob2 + 1L * os2, "B");
        p("Object[] write", java.util.Arrays.toString(oa));
        p("Object[] read", u.getObject(oa, ob2));

        // A reference array store through Unsafe is NOT covariance-checked --
        // that is the documented difference from `aastore`, and a VM that adds
        // the check here is as wrong as one that drops it from `aastore`.
        String[] strs = { "x", "y" };
        t("Unsafe store of an Integer into a String[]",
          () -> u.putObject(strs, ob2 + 0L, Integer.valueOf(1)));
        tv("what the String[] slot holds now", () -> {
            Object got = u.getObject(strs, ob2 + 0L);
            String seen = got == null ? "null" : got.getClass().getName() + "=" + got;
            // Put a String back before anything else can meet the polluted
            // slot -- heap pollution is the probe's subject, not its state.
            u.putObject(strs, ob2 + 0L, "x");
            return seen;
        });

        // A narrow write inside a byte[] must not disturb its neighbours.
        byte[] nb = new byte[8];
        java.util.Arrays.fill(nb, (byte) 0x7F);
        u.putByte(nb, bb + 3L, (byte) 0);
        p("byte[] narrow write neighbours", java.util.Arrays.toString(nb));
    }

    // ==================================================================
    // E. unaligned accessors, with and without the bigEndian flag
    // ==================================================================
    static void unaligned() {
        section("unaligned");
        int bb = u.arrayBaseOffset(byte[].class);
        byte[] a = new byte[16];
        for (int i = 0; i < a.length; i++) a[i] = (byte) (0x10 + i);

        p("getIntUnaligned aligned", Integer.toHexString(v.getIntUnaligned(a, bb)));
        p("getIntUnaligned off-by-1", Integer.toHexString(v.getIntUnaligned(a, bb + 1)));
        p("getIntUnaligned off-by-3", Integer.toHexString(v.getIntUnaligned(a, bb + 3)));
        p("getIntUnaligned BE", Integer.toHexString(v.getIntUnaligned(a, bb + 1, true)));
        p("getIntUnaligned LE", Integer.toHexString(v.getIntUnaligned(a, bb + 1, false)));
        p("BE is the byte-reverse of LE",
          v.getIntUnaligned(a, bb + 1, true) == Integer.reverseBytes(v.getIntUnaligned(a, bb + 1, false)));

        p("getLongUnaligned off-by-1", Long.toHexString(v.getLongUnaligned(a, bb + 1)));
        p("getLongUnaligned BE", Long.toHexString(v.getLongUnaligned(a, bb + 1, true)));
        p("getLongUnaligned LE", Long.toHexString(v.getLongUnaligned(a, bb + 1, false)));
        p("long BE is the byte-reverse of LE",
          v.getLongUnaligned(a, bb + 1, true) == Long.reverseBytes(v.getLongUnaligned(a, bb + 1, false)));
        p("getShortUnaligned off-by-1", Integer.toHexString(v.getShortUnaligned(a, bb + 1) & 0xFFFF));
        p("getShortUnaligned BE", Integer.toHexString(v.getShortUnaligned(a, bb + 1, true) & 0xFFFF));
        p("getCharUnaligned off-by-1", Integer.toHexString(v.getCharUnaligned(a, bb + 1)));
        p("getCharUnaligned BE", Integer.toHexString(v.getCharUnaligned(a, bb + 1, true)));

        // An unaligned WRITE must land exactly on its own bytes.
        byte[] w = new byte[16];
        v.putIntUnaligned(w, bb + 1, 0x11223344);
        p("putIntUnaligned bytes", java.util.Arrays.toString(w));
        java.util.Arrays.fill(w, (byte) 0);
        v.putIntUnaligned(w, bb + 1, 0x11223344, true);
        p("putIntUnaligned BE bytes", java.util.Arrays.toString(w));
        java.util.Arrays.fill(w, (byte) 0);
        v.putIntUnaligned(w, bb + 1, 0x11223344, false);
        p("putIntUnaligned LE bytes", java.util.Arrays.toString(w));
        java.util.Arrays.fill(w, (byte) 0);
        v.putLongUnaligned(w, bb + 3, 0x0102030405060708L);
        p("putLongUnaligned bytes", java.util.Arrays.toString(w));
        java.util.Arrays.fill(w, (byte) 0);
        v.putShortUnaligned(w, bb + 5, (short) 0x1234);
        p("putShortUnaligned bytes", java.util.Arrays.toString(w));
        java.util.Arrays.fill(w, (byte) 0);
        v.putCharUnaligned(w, bb + 7, '\u4142');
        p("putCharUnaligned bytes", java.util.Arrays.toString(w));

        // Round-trip through the same endianness must be the identity.
        java.util.Arrays.fill(w, (byte) 0);
        v.putLongUnaligned(w, bb + 1, 0xF0E0D0C0B0A09080L, true);
        p("long BE round-trip", Long.toHexString(v.getLongUnaligned(w, bb + 1, true)));
        p("long BE seen as LE", Long.toHexString(v.getLongUnaligned(w, bb + 1, false)));
    }

    // ==================================================================
    // F. volatile / acquire / release / opaque / plain
    // ==================================================================
    static void memoryOrder() throws Exception {
        section("memory-order");
        Holder h = new Holder();
        long oi = u.objectFieldOffset(Holder.class.getDeclaredField("i"));
        long ol = u.objectFieldOffset(Holder.class.getDeclaredField("l"));
        long oo = u.objectFieldOffset(Holder.class.getDeclaredField("o"));
        long ob = u.objectFieldOffset(Holder.class.getDeclaredField("b"));
        long oc = u.objectFieldOffset(Holder.class.getDeclaredField("c"));
        long os = u.objectFieldOffset(Holder.class.getDeclaredField("s"));
        long oz = u.objectFieldOffset(Holder.class.getDeclaredField("z"));
        long of = u.objectFieldOffset(Holder.class.getDeclaredField("f"));
        long od = u.objectFieldOffset(Holder.class.getDeclaredField("d"));

        p("sun getIntVolatile agrees", u.getIntVolatile(h, oi) == u.getInt(h, oi));
        u.putIntVolatile(h, oi, 21);        p("sun putIntVolatile", h.i);
        u.putOrderedInt(h, oi, 22);         p("sun putOrderedInt", h.i);
        u.putLongVolatile(h, ol, -1L);      p("sun putLongVolatile", h.l);
        u.putOrderedLong(h, ol, -2L);       p("sun putOrderedLong", h.l);
        u.putObjectVolatile(h, oo, "ov");   p("sun putObjectVolatile", h.o);
        u.putOrderedObject(h, oo, "oo");    p("sun putOrderedObject", h.o);
        p("sun getObjectVolatile", u.getObjectVolatile(h, oo));
        u.putByteVolatile(h, ob, (byte) -3);      p("sun putByteVolatile", h.b);
        p("sun getByteVolatile", u.getByteVolatile(h, ob));
        u.putCharVolatile(h, oc, '\u00FF');       p("sun putCharVolatile", (int) h.c);
        p("sun getCharVolatile", (int) u.getCharVolatile(h, oc));
        u.putShortVolatile(h, os, (short) -7);    p("sun putShortVolatile", h.s);
        p("sun getShortVolatile", u.getShortVolatile(h, os));
        u.putBooleanVolatile(h, oz, false);       p("sun putBooleanVolatile", h.z);
        p("sun getBooleanVolatile", u.getBooleanVolatile(h, oz));
        u.putFloatVolatile(h, of, -0.0f);         p("sun putFloatVolatile raw", Float.floatToRawIntBits(h.f));
        p("sun getFloatVolatile raw", Float.floatToRawIntBits(u.getFloatVolatile(h, of)));
        u.putDoubleVolatile(h, od, Double.NaN);   p("sun putDoubleVolatile NaN", Double.isNaN(h.d));
        p("sun getDoubleVolatile NaN", Double.isNaN(u.getDoubleVolatile(h, od)));

        Holder g = new Holder();
        p("internal getIntAcquire", v.getIntAcquire(g, oi));
        v.putIntRelease(g, oi, 41);   p("internal putIntRelease", g.i);
        p("internal getIntOpaque", v.getIntOpaque(g, oi));
        v.putIntOpaque(g, oi, 42);    p("internal putIntOpaque", g.i);
        p("internal getLongAcquire", v.getLongAcquire(g, ol));
        v.putLongRelease(g, ol, 43L); p("internal putLongRelease", g.l);
        p("internal getReferenceAcquire", v.getReferenceAcquire(g, oo));
        v.putReferenceRelease(g, oo, "rel"); p("internal putReferenceRelease", g.o);
        p("internal getReferenceOpaque", v.getReferenceOpaque(g, oo));
        v.putReferenceOpaque(g, oo, "opa");  p("internal putReferenceOpaque", g.o);
        p("internal getReferenceVolatile", v.getReferenceVolatile(g, oo));
        v.putReferenceVolatile(g, oo, "vol"); p("internal putReferenceVolatile", g.o);

        t("loadFence", () -> u.loadFence());
        t("storeFence", () -> u.storeFence());
        t("fullFence", () -> u.fullFence());
        t("internal loadFence", () -> v.loadFence());
        t("internal storeFence", () -> v.storeFence());
        t("internal fullFence", () -> v.fullFence());
    }

    // ==================================================================
    // G. CAS, exchange, weak CAS, getAndAdd / getAndSet
    // ==================================================================
    static void cas() throws Exception {
        section("cas");
        Holder h = new Holder();
        long oi = u.objectFieldOffset(Holder.class.getDeclaredField("i"));
        long ol = u.objectFieldOffset(Holder.class.getDeclaredField("l"));
        long oo = u.objectFieldOffset(Holder.class.getDeclaredField("o"));
        long ob = u.objectFieldOffset(Holder.class.getDeclaredField("b"));
        long os = u.objectFieldOffset(Holder.class.getDeclaredField("s"));

        p("sun CAS int wrong witness", u.compareAndSwapInt(h, oi, 999, 5));
        p("sun CAS int left the slot alone", h.i);
        p("sun CAS int right witness", u.compareAndSwapInt(h, oi, 3, 5));
        p("sun CAS int result", h.i);
        p("sun CAS long wrong witness", u.compareAndSwapLong(h, ol, 999L, 6L));
        p("sun CAS long right witness", u.compareAndSwapLong(h, ol, 4L, 6L));
        p("sun CAS long result", h.l);
        p("sun CAS obj wrong witness", u.compareAndSwapObject(h, oo, "nope", "x"));
        p("sun CAS obj left the slot alone", h.o);
        p("sun CAS obj right witness", u.compareAndSwapObject(h, oo, "init", "cas"));
        p("sun CAS obj result", h.o);
        // Identity, not equality: an EQUAL but distinct String must not match.
        h.o = new String("eq");
        p("sun CAS obj equal-but-not-identical", u.compareAndSwapObject(h, oo, new String("eq"), "z"));
        p("sun CAS obj after equal-witness attempt", h.o);
        p("sun CAS obj null witness on a non-null slot", u.compareAndSwapObject(h, oo, null, "z"));
        h.o = null;
        p("sun CAS obj null witness on a null slot", u.compareAndSwapObject(h, oo, null, "fromnull"));
        p("sun CAS obj to null", u.compareAndSwapObject(h, oo, "fromnull", null));
        p("sun CAS obj result null", h.o);

        Holder g = new Holder();
        p("internal compareAndSetInt", v.compareAndSetInt(g, oi, 3, 8));
        p("internal compareAndSetInt result", g.i);
        p("internal compareAndExchangeInt returns the witness", v.compareAndExchangeInt(g, oi, 8, 9));
        p("internal compareAndExchangeInt on failure returns the current", v.compareAndExchangeInt(g, oi, 99, 10));
        p("internal compareAndExchangeInt left the slot alone", g.i);
        p("internal compareAndSetLong", v.compareAndSetLong(g, ol, 4L, 11L));
        p("internal compareAndExchangeLong", v.compareAndExchangeLong(g, ol, 11L, 12L));
        p("internal compareAndSetReference", v.compareAndSetReference(g, oo, "init", "cr"));
        p("internal compareAndExchangeReference", v.compareAndExchangeReference(g, oo, "cr", "ce"));
        p("internal compareAndExchangeReference on failure", v.compareAndExchangeReference(g, oo, "nope", "x"));

        // Weak CAS may fail spuriously by contract, so the row is "converged",
        // not "succeeded on the first try".
        p("weakCompareAndSetInt converges", spin(() -> v.weakCompareAndSetInt(g, oi, 10, 20)));
        p("weakCompareAndSetInt result", g.i);
        p("weakCompareAndSetIntPlain converges", spin(() -> v.weakCompareAndSetIntPlain(g, oi, 20, 21)));
        p("weakCompareAndSetIntAcquire converges", spin(() -> v.weakCompareAndSetIntAcquire(g, oi, 21, 22)));
        p("weakCompareAndSetIntRelease converges", spin(() -> v.weakCompareAndSetIntRelease(g, oi, 22, 23)));
        p("weakCompareAndSetInt result after four", g.i);
        p("weakCompareAndSetInt wrong witness stays false", v.weakCompareAndSetInt(g, oi, 999, 0));
        p("weakCompareAndSetLong converges", spin(() -> v.weakCompareAndSetLong(g, ol, 12L, 13L)));
        p("weakCompareAndSetLongPlain converges", spin(() -> v.weakCompareAndSetLongPlain(g, ol, 13L, 14L)));
        p("weakCompareAndSetLongAcquire converges", spin(() -> v.weakCompareAndSetLongAcquire(g, ol, 14L, 15L)));
        p("weakCompareAndSetLongRelease converges", spin(() -> v.weakCompareAndSetLongRelease(g, ol, 15L, 16L)));
        p("weakCompareAndSetLong result", g.l);
        p("weakCompareAndSetReference converges", spin(() -> v.weakCompareAndSetReference(g, oo, "ce", "w1")));
        p("weakCompareAndSetReferencePlain converges", spin(() -> v.weakCompareAndSetReferencePlain(g, oo, "w1", "w2")));
        p("weakCompareAndSetReferenceAcquire converges", spin(() -> v.weakCompareAndSetReferenceAcquire(g, oo, "w2", "w3")));
        p("weakCompareAndSetReferenceRelease converges", spin(() -> v.weakCompareAndSetReferenceRelease(g, oo, "w3", "w4")));
        p("weakCompareAndSetReference result", g.o);

        // The Acquire/Release spellings are separate registry rows and separate
        // JDK bytecode; only the plain one was asked above.
        p("compareAndExchangeIntAcquire", v.compareAndExchangeIntAcquire(g, oi, 10, 30));
        p("compareAndExchangeIntRelease", v.compareAndExchangeIntRelease(g, oi, 30, 31));
        p("compareAndExchangeIntAcquire on failure", v.compareAndExchangeIntAcquire(g, oi, 999, 0));
        p("compareAndExchangeInt slot after the ordered pair", g.i);
        p("compareAndExchangeLongAcquire", v.compareAndExchangeLongAcquire(g, ol, 12L, 32L));
        p("compareAndExchangeLongRelease", v.compareAndExchangeLongRelease(g, ol, 32L, 33L));
        p("compareAndExchangeReferenceAcquire", v.compareAndExchangeReferenceAcquire(g, oo, "w4", "a1"));
        p("compareAndExchangeReferenceRelease", v.compareAndExchangeReferenceRelease(g, oo, "a1", "a2"));
        p("compareAndSetIntPlain-equivalent slot", g.i);

        // getAndAdd / getAndSet return the OLD value.
        Holder k = new Holder();
        p("sun getAndAddInt old", u.getAndAddInt(k, oi, 5));
        p("sun getAndAddInt new", k.i);
        p("sun getAndAddLong old", u.getAndAddLong(k, ol, 5L));
        p("sun getAndAddLong new", k.l);
        p("sun getAndSetInt old", u.getAndSetInt(k, oi, 100));
        p("sun getAndSetInt new", k.i);
        p("sun getAndSetLong old", u.getAndSetLong(k, ol, 100L));
        p("sun getAndSetObject old", u.getAndSetObject(k, oo, "gs"));
        p("sun getAndSetObject new", k.o);
        p("internal getAndSetReference old", v.getAndSetReference(k, oo, "gsr"));
        p("internal getAndAddInt with a negative delta", v.getAndAddInt(k, oi, -1));

        // Byte and short getAndAdd must wrap in their own width.
        Holder w = new Holder();
        w.b = (byte) 127;
        p("internal getAndAddByte old", v.getAndAddByte(w, ob, (byte) 1));
        p("internal getAndAddByte wrapped", w.b);
        w.s = (short) 32767;
        p("internal getAndAddShort old", v.getAndAddShort(w, os, (short) 1));
        p("internal getAndAddShort wrapped", w.s);

        // The bitwise family is JDK bytecode over the CAS natives -- the
        // shortest path from a retired shadow to a wrong answer.
        Holder x = new Holder();
        x.i = 0b1010;
        p("getAndBitwiseOrInt old", v.getAndBitwiseOrInt(x, oi, 0b0101));
        p("getAndBitwiseOrInt new", x.i);
        p("getAndBitwiseAndInt old", v.getAndBitwiseAndInt(x, oi, 0b1100));
        p("getAndBitwiseAndInt new", x.i);
        p("getAndBitwiseXorInt old", v.getAndBitwiseXorInt(x, oi, 0b1111));
        p("getAndBitwiseXorInt new", x.i);

        Holder y = new Holder();
        y.l = 0b1010L;
        p("getAndBitwiseOrLong old", v.getAndBitwiseOrLong(y, ol, 0b0101L));
        p("getAndBitwiseAndLong old", v.getAndBitwiseAndLong(y, ol, 0b1100L));
        p("getAndBitwiseXorLong old", v.getAndBitwiseXorLong(y, ol, 0b1111L));
        p("getAndBitwiseLong new", y.l);
        // The SUB-WORD atomics (byte / short / char / boolean) are NOT here.
        // `getAndBitwiseOrByte` does not return on CratonVM, and one
        // non-returning row costs the whole sweep. They live in
        // `UnsafeSubwordProbe`, one operation per process behind a timeout,
        // where "never returned" is a printed outcome instead of a truncation.

        // CAS on an ARRAY element -- the `casTabAt` shape.
        int[] ia = { 1, 2, 3 };
        int ab = u.arrayBaseOffset(int[].class), as = u.arrayIndexScale(int[].class);
        p("array CAS wrong witness", u.compareAndSwapInt(ia, ab + 1L * as, 99, 7));
        p("array CAS right witness", u.compareAndSwapInt(ia, ab + 1L * as, 2, 7));
        p("array CAS result", java.util.Arrays.toString(ia));
        Object[] oa = { "a", "b" };
        int ob2 = u.arrayBaseOffset(Object[].class), os2 = u.arrayIndexScale(Object[].class);
        p("ref array CAS right witness", u.compareAndSwapObject(oa, ob2 + 1L * os2, "b", "B"));
        p("ref array CAS result", java.util.Arrays.toString(oa));
        p("array getAndAddInt old", u.getAndAddInt(ia, ab, 10));
        p("array getAndAddInt result", java.util.Arrays.toString(ia));

        // CAS on a STATIC, through staticFieldBase.
        Field fsi = Statics.class.getDeclaredField("si");
        Object sbase = u.staticFieldBase(fsi);
        long soff = u.staticFieldOffset(fsi);
        p("static CAS wrong witness", u.compareAndSwapInt(sbase, soff, 999, 1));
        p("static CAS right witness", u.compareAndSwapInt(sbase, soff, 3, 55));
        p("static CAS visible to Java", Statics.si);
        Statics.si = 3;
    }

    static boolean spin(java.util.function.BooleanSupplier b) {
        for (int i = 0; i < 1000; i++) if (b.getAsBoolean()) return true;
        return false;
    }

    // ==================================================================
    // H. off-heap allocation and the memory primitives
    // ==================================================================
    static void offHeap() {
        section("off-heap");
        // allocateMemory's contract edges. `allocateMemory(0)` returns 0 by
        // spec, and a negative size is an IllegalArgumentException, NOT an OOM.
        tv("sun allocateMemory(0)", () -> u.allocateMemory(0) == 0 ? "zero" : "non-zero");
        t("sun allocateMemory(-1)", () -> u.allocateMemory(-1));
        t("sun allocateMemory(Long.MIN_VALUE)", () -> u.allocateMemory(Long.MIN_VALUE));
        t("sun allocateMemory(Long.MAX_VALUE)", () -> {
            long q = u.allocateMemory(Long.MAX_VALUE);
            u.freeMemory(q);
        });
        tv("internal allocateMemory(0)", () -> v.allocateMemory(0) == 0 ? "zero" : "non-zero");
        t("internal allocateMemory(-1)", () -> v.allocateMemory(-1));
        t("sun freeMemory(0)", () -> u.freeMemory(0));
        t("internal freeMemory(0)", () -> v.freeMemory(0));

        final long m = u.allocateMemory(64);
        p("allocateMemory non-zero", m != 0);
        try {
            // Every width against a null base -- the off-heap door.
            u.setMemory(m, 64, (byte) 0);
            u.putLong(m, 0x1122334455667788L);
            p("off-heap long", Long.toHexString(u.getLong(m)));
            u.putInt(m + 8, 0x0A0B0C0D);
            p("off-heap int", Integer.toHexString(u.getInt(m + 8)));
            u.putShort(m + 12, (short) 0x1234);
            p("off-heap short", Integer.toHexString(u.getShort(m + 12) & 0xFFFF));
            u.putChar(m + 14, '\u4321');
            p("off-heap char", Integer.toHexString(u.getChar(m + 14)));
            u.putByte(m + 16, (byte) 0x5A);
            p("off-heap byte", Integer.toHexString(u.getByte(m + 16) & 0xFF));
            u.putFloat(m + 20, -0.0f);
            p("off-heap float raw", Float.floatToRawIntBits(u.getFloat(m + 20)));
            u.putDouble(m + 24, Double.NaN);
            p("off-heap double NaN", Double.isNaN(u.getDouble(m + 24)));
            u.putBoolean(null, m + 32, true);
            p("off-heap boolean via null base", u.getBoolean(null, m + 32));
            u.putFloat(null, m + 36, 1.5f);
            p("off-heap float via null base", u.getFloat(null, m + 36));
            u.putDouble(null, m + 40, 2.5);
            p("off-heap double via null base", u.getDouble(null, m + 40));
            u.putInt(null, m + 48, 7);
            p("off-heap int via null base", u.getInt(null, m + 48));
            u.putLong(null, m + 52, 8L);
            p("off-heap long via null base", u.getLong(null, m + 52));

            // THE TWO OFF-HEAP DOORS MUST NAME THE SAME BYTE. `getInt(long)` is
            // literally `getInt(null, address)` in the JDK, so a write through
            // one spelling has to be visible through the other. A VM that
            // routes the null-base form to a private side map passes every
            // round-trip test inside one door and still loses the write.
            u.setMemory(m, 64, (byte) 0);
            u.putInt(null, m + 56, 0x0BADF00D);
            p("null-base int write, 1-arg read", Integer.toHexString(u.getInt(m + 56)));
            u.putInt(m + 56, 0x11112222);
            p("1-arg int write, null-base read", Integer.toHexString(u.getInt(null, m + 56)));
            u.putLong(null, m + 40, 0x0102030405060708L);
            p("null-base long write, 1-arg read", Long.toHexString(u.getLong(m + 40)));
            u.putLong(m + 40, 0x1122334455667788L);
            p("1-arg long write, null-base read", Long.toHexString(u.getLong(null, m + 40)));
            u.putFloat(null, m + 32, 1.5f);
            p("null-base float write, 1-arg read", u.getFloat(m + 32));
            u.putFloat(m + 32, 2.5f);
            p("1-arg float write, null-base read", u.getFloat(null, m + 32));
            u.putDouble(null, m + 24, 3.5);
            p("null-base double write, 1-arg read", u.getDouble(m + 24));
            u.putDouble(m + 24, 4.5);
            p("1-arg double write, null-base read", u.getDouble(null, m + 24));
            u.putByte(null, m + 20, (byte) 0x5A);
            p("null-base byte write, 1-arg read", Integer.toHexString(u.getByte(m + 20) & 0xFF));
            u.putShort(null, m + 16, (short) 0x1234);
            p("null-base short write, 1-arg read", Integer.toHexString(u.getShort(m + 16) & 0xFFFF));
            // ... and a write through EITHER door must be visible to the byte
            // reader, which is what `setMemory`/`copyMemory` and the socket
            // layer use.
            u.setMemory(m, 64, (byte) 0);
            u.putInt(null, m + 8, 0x04030201);
            byte[] seenLe = new byte[4];
            for (int i = 0; i < 4; i++) seenLe[i] = u.getByte(m + 8 + i);
            p("null-base int write, byte-wise read", java.util.Arrays.toString(seenLe));

            // A narrow off-heap write must touch exactly its own bytes.
            u.setMemory(m, 16, (byte) -1);
            u.putByte(m + 2, (byte) 0);
            p("off-heap narrow write", Long.toHexString(u.getLong(m)));
            u.setMemory(m, 16, (byte) -1);
            u.putShort(m + 2, (short) 0);
            p("off-heap short write", Long.toHexString(u.getLong(m)));

            // setMemory / copyMemory edges.
            t("setMemory length 0", () -> u.setMemory(m, 0, (byte) 1));
            t("setMemory negative length", () -> u.setMemory(m, -1, (byte) 1));
            t("internal setMemory negative length", () -> v.setMemory(m, -1, (byte) 1));
            final long m2 = u.allocateMemory(64);
            try {
                u.setMemory(m2, 64, (byte) 0);
                u.setMemory(m, 8, (byte) 0x11);
                u.copyMemory(m, m2, 8);
                p("copyMemory off-heap to off-heap", Long.toHexString(u.getLong(m2)));
                t("copyMemory length 0", () -> u.copyMemory(m, m2, 0));
                t("copyMemory negative length", () -> u.copyMemory(m, m2, -1));
                t("internal copyMemory negative length",
                  () -> v.copyMemory(null, m, null, m2, -1));
                // Overlapping copy must behave like memmove.
                u.setMemory(m, 16, (byte) 0);
                for (int i = 0; i < 8; i++) u.putByte(m + i, (byte) (i + 1));
                u.copyMemory(m, m + 4, 8);
                byte[] seen = new byte[16];
                for (int i = 0; i < 16; i++) seen[i] = u.getByte(m + i);
                p("overlapping copyMemory", java.util.Arrays.toString(seen));
            } finally { u.freeMemory(m2); }

            // heap <-> off-heap copies through the Object-base form.
            final byte[] src = { 1, 2, 3, 4, 5, 6, 7, 8 };
            final byte[] dst = new byte[8];
            final int bb = u.arrayBaseOffset(byte[].class);
            u.setMemory(m, 8, (byte) 0);
            u.copyMemory(src, bb, null, m, 8);
            p("copyMemory heap to off-heap", Long.toHexString(u.getLong(m)));
            u.copyMemory(null, m, dst, bb, 8);
            p("copyMemory off-heap to heap", java.util.Arrays.toString(dst));
            u.copyMemory(src, bb, dst, bb + 4, 4);
            p("copyMemory heap to heap", java.util.Arrays.toString(dst));
            t("copyMemory heap with a negative length",
              () -> u.copyMemory(src, bb, dst, bb, -1));
            final byte[] fill = new byte[8];
            u.setMemory(fill, bb + 2, 4, (byte) 0x7E);
            p("setMemory on a heap array", java.util.Arrays.toString(fill));
            t("setMemory on a heap array, negative length",
              () -> u.setMemory(fill, bb, -1, (byte) 1));

            // copySwapMemory -- internal only, element sizes 2/4/8.
            final long sw = u.allocateMemory(16);
            try {
                for (int i = 0; i < 8; i++) u.putByte(sw + i, (byte) (i + 1));
                tv("copySwapMemory elemSize 2", () -> {
                    v.copySwapMemory(null, sw, null, sw + 8, 8, 2);
                    byte[] o = new byte[8];
                    for (int i = 0; i < 8; i++) o[i] = u.getByte(sw + 8 + i);
                    return java.util.Arrays.toString(o);
                });
                tv("copySwapMemory elemSize 4", () -> {
                    v.copySwapMemory(null, sw, null, sw + 8, 8, 4);
                    byte[] o = new byte[8];
                    for (int i = 0; i < 8; i++) o[i] = u.getByte(sw + 8 + i);
                    return java.util.Arrays.toString(o);
                });
                tv("copySwapMemory elemSize 8", () -> {
                    v.copySwapMemory(null, sw, null, sw + 8, 8, 8);
                    byte[] o = new byte[8];
                    for (int i = 0; i < 8; i++) o[i] = u.getByte(sw + 8 + i);
                    return java.util.Arrays.toString(o);
                });
                t("copySwapMemory elemSize 3",
                  () -> v.copySwapMemory(null, sw, null, sw + 8, 8, 3));
                t("copySwapMemory length not a multiple of elemSize",
                  () -> v.copySwapMemory(null, sw, null, sw + 8, 7, 2));
                t("copySwapMemory negative length",
                  () -> v.copySwapMemory(null, sw, null, sw + 8, -8, 2));
            } finally { u.freeMemory(sw); }
        } finally {
            u.freeMemory(m);
        }

        // reallocateMemory: content is preserved on grow; size 0 frees.
        long r = u.allocateMemory(16);
        u.setMemory(r, 16, (byte) 0);
        u.putLong(r, 0x0102030405060708L);
        final long r2 = u.reallocateMemory(r, 64);
        p("reallocateMemory preserved the content", Long.toHexString(u.getLong(r2)));
        tv("reallocateMemory to 0 returns zero", () -> u.reallocateMemory(r2, 0) == 0 ? "zero" : "non-zero");
        t("reallocateMemory(0, 16) then free", () -> {
            long q = u.reallocateMemory(0, 16);
            if (q != 0) u.freeMemory(q);
        });
        t("reallocateMemory negative size", () -> u.reallocateMemory(0, -1));
        // A GROW MUST NOT SWALLOW A LATER ALLOCATION. Two live allocations are
        // independent whatever addresses a VM picks, so this is behaviour and
        // not an address diff. CratonVM's arena resolved an address to the
        // greatest block base at or below it and bumped its handle counter by
        // the ORIGINAL request, so growing the first block made it answer for
        // the second: every access through the later handle landed silently in
        // the earlier block, and neither one reported anything.
        //
        // The load-bearing row is the SCAN of the grown block. Checking that
        // the later allocation reads back what it wrote passes even when the
        // two alias -- the write and the read hit the same aliased cell. Only
        // the earlier block can show the damage, and only by looking at all of
        // it, because the later block lands somewhere in the middle.
        {
            long g = u.allocateMemory(64);
            long h = u.allocateMemory(64);
            long g2 = u.reallocateMemory(g, 4096);
            u.setMemory(g2, 4096, (byte) 0x11);
            u.setMemory(h, 64, (byte) 0x77);
            boolean intact = true;
            for (long i = 0; i < 4096; i++) {
                if (u.getByte(g2 + i) != (byte) 0x11) { intact = false; break; }
            }
            p("a grown block is not overwritten by a later allocation", intact);
            p("and the later allocation keeps its own bytes", u.getByte(h) == (byte) 0x77);
            u.freeMemory(g2);
            u.freeMemory(h);
        }

        t("internal reallocateMemory negative size", () -> v.reallocateMemory(0, -1));

        p("addressSize", u.addressSize());
        p("internal addressSize agrees", v.addressSize() == u.addressSize());
        p("pageSize is a power of two", Integer.bitCount(u.pageSize()) == 1);
        p("internal pageSize agrees", v.pageSize() == u.pageSize());
    }

    // ==================================================================
    // I. allocateInstance
    // ==================================================================
    static void allocate() {
        section("allocate-instance");
        tv("allocateInstance of a normal class", () -> {
            NoDefaultCtor n = (NoDefaultCtor) u.allocateInstance(NoDefaultCtor.class);
            return n.getClass().getName() + " a=" + n.a + " s=" + n.s;
        });
        tv("allocateInstance did not run the constructor", () -> {
            NoDefaultCtor n = (NoDefaultCtor) u.allocateInstance(NoDefaultCtor.class);
            return n.s == null;
        });
        t("allocateInstance of an interface", () -> u.allocateInstance(Iface.class));
        t("allocateInstance of an abstract class", () -> u.allocateInstance(Abstract.class));
        t("allocateInstance of a primitive class", () -> u.allocateInstance(int.class));
        t("allocateInstance of void", () -> u.allocateInstance(void.class));
        t("allocateInstance of an array class", () -> u.allocateInstance(int[].class));
        t("allocateInstance of an enum", () -> u.allocateInstance(En.class));
        tv("allocateInstance of a record", () -> {
            Object o = u.allocateInstance(Rec.class);
            return o.getClass().getName();
        });
        tv("allocateInstance of String", () -> u.allocateInstance(String.class).getClass().getName());
        // allocateInstance must force initialization of the class.
        tv("allocateInstance initializes the class", () -> {
            u.allocateInstance(Clinit1.class);
            return Clinit1.ran;
        });
        tv("internal allocateInstance", () -> {
            Object o = v.allocateInstance(NoDefaultCtor.class);
            return o.getClass().getName();
        });
        t("internal allocateInstance of an interface", () -> v.allocateInstance(Iface.class));
    }

    // ==================================================================
    // J. class initialization
    // ==================================================================
    static void classInit() {
        section("class-init");
        // `sun.misc.Unsafe.ensureClassInitialized` / `shouldBeInitialized` are
        // NOT declared by this JDK image -- the reflective section records
        // that, and the two registrations that shadow them. Everything here
        // therefore goes through the internal spelling.
        p("shouldBeInitialized before", v.shouldBeInitialized(Clinit2.class));
        t("ensureClassInitialized", () -> v.ensureClassInitialized(Clinit2.class));
        p("clinit ran", Clinit2.ran);
        p("shouldBeInitialized after", v.shouldBeInitialized(Clinit2.class));
        p("internal shouldBeInitialized before", v.shouldBeInitialized(Clinit3.class));
        t("internal ensureClassInitialized", () -> v.ensureClassInitialized(Clinit3.class));
        p("internal clinit ran", Clinit3.ran);
        p("internal shouldBeInitialized after", v.shouldBeInitialized(Clinit3.class));
        t("ensureClassInitialized(int.class)", () -> v.ensureClassInitialized(int.class));
        t("shouldBeInitialized(int.class)", () -> p("  value", v.shouldBeInitialized(int.class)));
        t("ensureClassInitialized(int[].class)", () -> v.ensureClassInitialized(int[].class));
        t("shouldBeInitialized(Iface.class)", () -> p("  value", v.shouldBeInitialized(Iface.class)));
    }

    // ==================================================================
    // K. throwException, park/unpark, getLoadAverage, invokeCleaner
    // ==================================================================
    static void misc() {
        section("misc");
        tv("throwException delivers the throwable undeclared", () -> {
            try { u.throwException(new java.io.IOException("x")); return "no-throw"; }
            catch (Throwable e) { return e.getClass().getName() + ":" + e.getMessage(); }
        });
        tv("internal throwException", () -> {
            try { v.throwException(new IllegalStateException("y")); return "no-throw"; }
            catch (Throwable e) { return e.getClass().getName(); }
        });

        t("unpark then park returns", () -> {
            u.unpark(Thread.currentThread());
            u.park(false, 0L);
        });
        t("park with a relative timeout returns", () -> u.park(false, 1_000_000L));
        t("park with an absolute deadline in the past returns", () -> u.park(true, 1L));
        t("internal park relative", () -> v.park(false, 1_000_000L));

        final double[] la = new double[3];
        tv("getLoadAverage result is in range", () -> {
            int n = u.getLoadAverage(la, 3);
            return n >= -1 && n <= 3;
        });

        // invokeCleaner: a non-direct buffer and a view are both refusals.
        t("invokeCleaner on a heap buffer", () -> u.invokeCleaner(ByteBuffer.allocate(8)));
        t("invokeCleaner on a slice of a direct buffer",
          () -> u.invokeCleaner(ByteBuffer.allocateDirect(8).slice()));
        t("invokeCleaner on a duplicate of a direct buffer",
          () -> u.invokeCleaner(ByteBuffer.allocateDirect(8).duplicate()));
        t("invokeCleaner on a fresh direct buffer",
          () -> u.invokeCleaner(ByteBuffer.allocateDirect(8)));

        // getUnsafe from the application class loader.
        t("sun getUnsafe from the app loader", () -> sun.misc.Unsafe.getUnsafe());
        t("internal getUnsafe from the app loader", () -> jdk.internal.misc.Unsafe.getUnsafe());
    }

    // ==================================================================
    // L. what this JDK image actually declares -- reached reflectively so a
    //    method the image does not have is a row, not a link error.
    // ==================================================================
    static void reflective() {
        section("reflective");
        String[][] probes = {
            { "sun.misc.Unsafe", "monitorEnter", "java.lang.Object" },
            { "sun.misc.Unsafe", "monitorExit", "java.lang.Object" },
            { "sun.misc.Unsafe", "tryMonitorEnter", "java.lang.Object" },
            { "sun.misc.Unsafe", "defineAnonymousClass", "java.lang.Class,[B,[Ljava.lang.Object;" },
            { "sun.misc.Unsafe", "defineClass",
              "java.lang.String,[B,int,int,java.lang.ClassLoader,java.security.ProtectionDomain" },
            { "jdk.internal.misc.Unsafe", "monitorEnter", "java.lang.Object" },
            { "jdk.internal.misc.Unsafe", "monitorExit", "java.lang.Object" },
            { "jdk.internal.misc.Unsafe", "defineAnonymousClass", "java.lang.Class,[B,[Ljava.lang.Object;" },
            { "jdk.internal.misc.Unsafe", "defineClass",
              "java.lang.String,[B,int,int,java.lang.ClassLoader,java.security.ProtectionDomain" },
            { "jdk.internal.misc.Unsafe", "defineClass0",
              "java.lang.String,[B,int,int,java.lang.ClassLoader,java.security.ProtectionDomain" },
            { "jdk.internal.misc.Unsafe", "getUncompressedObject", "long" },
            { "jdk.internal.misc.Unsafe", "writeback0", "long" },
            { "jdk.internal.misc.Unsafe", "writebackPreSync0", "" },
            { "jdk.internal.misc.Unsafe", "writebackPostSync0", "" },
            { "jdk.internal.misc.Unsafe", "getReferencePlain", "java.lang.Object,long" },
            { "jdk.internal.misc.Unsafe", "putReferencePlain", "java.lang.Object,long,java.lang.Object" },
            { "jdk.internal.misc.Unsafe", "weakCompareAndSetObject",
              "java.lang.Object,long,java.lang.Object,java.lang.Object" },
            { "sun.misc.Unsafe", "getLoadAverage", "[D,int" },
            { "sun.misc.Unsafe", "ensureClassInitialized", "java.lang.Class" },
            { "sun.misc.Unsafe", "shouldBeInitialized", "java.lang.Class" },
            { "sun.misc.Unsafe", "putOrderedInt", "java.lang.Object,long,int" },
            { "sun.misc.Unsafe", "getObject", "java.lang.Object,long" },
            { "sun.misc.Unsafe", "invokeCleaner", "java.nio.ByteBuffer" },
            { "sun.misc.Unsafe", "throwException", "java.lang.Throwable" },
            { "sun.misc.Unsafe", "staticFieldBase", "java.lang.reflect.Field" },
            { "sun.misc.Unsafe", "allocateInstance", "java.lang.Class" },
            { "sun.misc.Unsafe", "park", "boolean,long" },
            { "sun.misc.Unsafe", "reallocateMemory", "long,long" },
            { "sun.misc.Unsafe", "setMemory", "java.lang.Object,long,long,byte" },
            { "jdk.internal.misc.Unsafe", "getLoadAverage0", "[D,int" },
            { "jdk.internal.misc.Unsafe", "shouldBeInitialized0", "java.lang.Class" },
            { "jdk.internal.misc.Unsafe", "objectFieldOffset1", "java.lang.Class,java.lang.String" },
            { "jdk.internal.misc.Unsafe", "staticFieldBase0", "java.lang.reflect.Field" },
            { "jdk.internal.misc.Unsafe", "allocateMemory0", "long" },
            { "jdk.internal.misc.Unsafe", "freeMemory0", "long" },
            { "jdk.internal.misc.Unsafe", "setMemory0", "java.lang.Object,long,long,byte" },
            { "jdk.internal.misc.Unsafe", "copyMemory0", "java.lang.Object,long,java.lang.Object,long,long" },
            { "jdk.internal.misc.Unsafe", "arrayBaseOffset0", "java.lang.Class" },
            { "jdk.internal.misc.Unsafe", "arrayIndexScale0", "java.lang.Class" },
            { "jdk.internal.misc.Unsafe", "ensureClassInitialized0", "java.lang.Class" },
            { "jdk.internal.misc.Unsafe", "objectFieldOffset0", "java.lang.reflect.Field" },
            { "jdk.internal.misc.Unsafe", "staticFieldOffset0", "java.lang.reflect.Field" },
            { "jdk.internal.misc.Unsafe", "getCharUnaligned", "java.lang.Object,long,boolean" },
            { "jdk.internal.misc.Unsafe", "park", "boolean,long" },
            { "jdk.internal.misc.Unsafe", "copySwapMemory0",
              "java.lang.Object,long,java.lang.Object,long,long,long" },
        };
        for (String[] q : probes) {
            String key = q[0] + "." + q[1] + "(" + q[2] + ")";
            try {
                Class<?> c = Class.forName(q[0]);
                Class<?>[] ps = parse(q[2]);
                Method m = c.getDeclaredMethod(q[1], ps);
                p("declares " + key,
                  (java.lang.reflect.Modifier.isNative(m.getModifiers()) ? "native" : "bytecode")
                  + " returns " + m.getReturnType().getName());
            } catch (NoSuchMethodException e) {
                p("declares " + key, "ABSENT");
            } catch (Throwable e) {
                p("declares " + key, "THREW " + e.getClass().getName());
            }
        }
        tv("isBigEndian", () -> v.isBigEndian());
        tv("unalignedAccess", () -> v.unalignedAccess());
    }

    static Class<?>[] parse(String spec) throws Exception {
        if (spec.isEmpty()) return new Class<?>[0];
        String[] parts = spec.split(",");
        List<Class<?>> out = new ArrayList<>();
        for (String s : parts) {
            switch (s) {
                case "int": out.add(int.class); break;
                case "long": out.add(long.class); break;
                case "byte": out.add(byte.class); break;
                case "boolean": out.add(boolean.class); break;
                case "[B": out.add(byte[].class); break;
                case "[D": out.add(double[].class); break;
                case "[Ljava.lang.Object;": out.add(Object[].class); break;
                default: out.add(Class.forName(s)); break;
            }
        }
        return out.toArray(new Class<?>[0]);
    }

    // ==================================================================
    public static void main(String[] args) throws Exception {
        Field f = Class.forName("sun.misc.Unsafe").getDeclaredField("theUnsafe");
        f.setAccessible(true);
        u = (sun.misc.Unsafe) f.get(null);
        Field g = Class.forName("sun.misc.Unsafe").getDeclaredField("theInternalUnsafe");
        g.setAccessible(true);
        v = (jdk.internal.misc.Unsafe) g.get(null);
        p("sun Unsafe non-null", u != null);
        p("internal Unsafe non-null", v != null);

        offsets();
        arrayShape();
        heapFields();
        arrayElements();
        unaligned();
        memoryOrder();
        cas();
        offHeap();
        allocate();
        classInit();
        misc();
        reflective();
        System.out.println("DONE UnsafeShadowSweep rows " + emitted);
    }
}
