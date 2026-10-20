import java.lang.reflect.Field;
import java.nio.ByteBuffer;

/** One `Unsafe` call with a null or out-of-bounds argument, per process.
 *
 *  These rows cannot live in `UnsafeShadowSweep`. Two of them take the ORACLE
 *  down rather than throwing:
 *
 *    * `sun.misc.Unsafe.allocateInstance(null)` -- SIGSEGV in
 *      `Unsafe_AllocateInstance`, HotSpot 25.0.4+7. Measured, not assumed.
 *    * `getLoadAverage(a, n)` with `n > a.length` -- nothing on the JDK path
 *      bounds-checks `n` before the native writes `n` doubles.
 *
 *  A crash inside a 300-row sweep truncates it, and `diff` reports the missing
 *  tail as ordinary `<` lines -- so one bad row would read as two hundred
 *  differences. Here a crash costs exactly its own row, and the row it costs is
 *  still a MEASUREMENT: the driver prints the exit status, so "the VM died" and
 *  "the VM threw" are different transcripts rather than the same silence.
 *
 *  Usage: UnsafeNullArgProbe <index>   -- prints `<index> <tag> |<outcome>|`.
 *         UnsafeNullArgProbe count     -- prints the number of cases.
 */
public class UnsafeNullArgProbe {

    static sun.misc.Unsafe u;
    static jdk.internal.misc.Unsafe v;

    static class Holder { int i = 3; }
    static class Statics { static int si = 3; }
    static class Clinit { static boolean ran; static { ran = true; } }

    static final String[] TAGS = {
        /*  0 */ "sun objectFieldOffset(null)",
        /*  1 */ "internal objectFieldOffset(null)",
        /*  2 */ "internal objectFieldOffset(null Class, name)",
        /*  3 */ "internal objectFieldOffset(Class, null name)",
        /*  4 */ "sun staticFieldOffset(null)",
        /*  5 */ "sun staticFieldBase(null)",
        /*  6 */ "internal staticFieldOffset(null)",
        /*  7 */ "internal staticFieldBase(null)",
        /*  8 */ "sun arrayIndexScale(null)",
        /*  9 */ "sun arrayBaseOffset(null)",
        /* 10 */ "internal arrayIndexScale(null)",
        /* 11 */ "internal arrayBaseOffset(null)",
        /* 12 */ "sun allocateInstance(null)",
        /* 13 */ "internal allocateInstance(null)",
        /* 14 */ "internal ensureClassInitialized(null)",
        /* 15 */ "internal shouldBeInitialized(null)",
        /* 16 */ "sun throwException(null)",
        /* 17 */ "sun unpark(null)",
        /* 18 */ "internal unpark(null)",
        /* 19 */ "sun getLoadAverage(null, 1)",
        /* 20 */ "sun getLoadAverage(double[1], 3)",
        /* 21 */ "sun invokeCleaner(null)",
        /* 22 */ "sun getInt(null object, a real field offset)",
        /* 23 */ "sun putInt(null object, a real field offset)",
        /* 24 */ "sun compareAndSwapInt(null object, a real field offset)",
        /* 25 */ "sun getObject(null object, a real field offset)",
        /* 26 */ "internal getReference(null object, a real field offset)",
        /* 27 */ "sun setMemory(null, 0 address, 8, 0)",
        /* 28 */ "sun copyMemory(null, 0, null, 0, 8)",
        /* 29 */ "sun getInt(int[], an offset past the end)",
        /* 30 */ "sun putInt(int[], an offset past the end)",
        /* 31 */ "sun getInt(int[], a negative offset)",
        /* 32 */ "sun putInt(int[], a negative offset)",
    };

    static Object run(int n) throws Throwable {
        Field fi = Holder.class.getDeclaredField("i");
        long oi = u.objectFieldOffset(fi);
        switch (n) {
            case 0: return u.objectFieldOffset(null);
            case 1: return v.objectFieldOffset(null);
            case 2: return v.objectFieldOffset(null, "i");
            case 3: return v.objectFieldOffset(Holder.class, null);
            case 4: return u.staticFieldOffset(null);
            case 5: return u.staticFieldBase(null);
            case 6: return v.staticFieldOffset(null);
            case 7: return v.staticFieldBase(null);
            case 8: return u.arrayIndexScale(null);
            case 9: return u.arrayBaseOffset(null);
            case 10: return v.arrayIndexScale(null);
            case 11: return v.arrayBaseOffset(null);
            case 12: return u.allocateInstance(null) == null ? "null" : "an instance";
            case 13: return v.allocateInstance(null) == null ? "null" : "an instance";
            case 14: v.ensureClassInitialized(null); return "no-throw";
            case 15: return v.shouldBeInitialized(null);
            case 16: u.throwException(null); return "no-throw";
            case 17: u.unpark(null); return "no-throw";
            case 18: v.unpark(null); return "no-throw";
            case 19: return u.getLoadAverage(null, 1);
            case 20: {
                double[] a = new double[1];
                int r = u.getLoadAverage(a, 3);
                return "returned " + (r >= -1 && r <= 3);
            }
            case 21: u.invokeCleaner(null); return "no-throw";
            case 22: return u.getInt(null, oi);
            case 23: u.putInt(null, oi, 1); return "no-throw";
            case 24: return u.compareAndSwapInt(null, oi, 0, 1);
            case 25: return u.getObject(null, oi) == null ? "null" : "an object";
            case 26: return v.getReference(null, oi) == null ? "null" : "an object";
            case 27: u.setMemory(null, 0L, 8L, (byte) 0); return "no-throw";
            case 28: u.copyMemory(null, 0L, null, 0L, 8L); return "no-throw";
            // 29-32 are NOT RUN, and the row says so rather than leaving a
            // silent hole. An out-of-bounds heap offset reads or writes
            // whatever the allocator put next to the array: the READ is
            // nondeterministic within one VM, so it cannot be diffed across
            // two, and the WRITE corrupts the probe's own heap, so every later
            // row becomes evidence about the corruption instead of about
            // Unsafe. Bounds behaviour is measured where it is decidable --
            // the width and neighbour checks in `UnsafeShadowSweep`.
            case 29:
            case 30:
            case 31:
            case 32:
                return "NOT RUN (nondeterministic or self-corrupting by construction)";
            default: return "NO SUCH CASE";
        }
    }

    public static void main(String[] args) throws Throwable {
        if (args.length == 1 && args[0].equals("count")) {
            System.out.println(TAGS.length);
            return;
        }
        Field f = Class.forName("sun.misc.Unsafe").getDeclaredField("theUnsafe");
        f.setAccessible(true);
        u = (sun.misc.Unsafe) f.get(null);
        Field g = Class.forName("sun.misc.Unsafe").getDeclaredField("theInternalUnsafe");
        g.setAccessible(true);
        v = (jdk.internal.misc.Unsafe) g.get(null);

        int n = Integer.parseInt(args[0]);
        String tag = n < TAGS.length ? TAGS[n] : "?";
        String out;
        try { out = String.valueOf(run(n)); }
        catch (Throwable e) { out = "THREW " + e.getClass().getName(); }
        System.out.println(n + " " + tag + " |" + out + "|");
        System.out.flush();
    }
}
