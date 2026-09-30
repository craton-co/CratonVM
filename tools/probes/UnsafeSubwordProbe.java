import java.lang.reflect.Field;

/** The sub-word atomic family, one operation per process.
 *
 *  WHY THIS IS NOT IN `UnsafeShadowSweep`: `getAndBitwiseOrByte` does not
 *  return on CratonVM. It is JDK BYTECODE --
 *
 *      do { current = getByteVolatile(o, offset); }
 *      while (!weakCompareAndSetByte(o, offset, current, (byte)(current|mask)));
 *
 *  -- over `compareAndSetByte`, which the JDK itself emulates by masking a
 *  32-bit word: `wordOffset = offset & ~3`, `shift = (offset & 3) << 3`. That
 *  arithmetic is only meaningful if the offset is a BYTE offset. CratonVM's
 *  `objectFieldOffset` returns a SLOT INDEX, so `offset & ~3` names a different
 *  field, the masked compare never matches, and the caller's loop never exits.
 *
 *  One hang inside a 400-row sweep costs the whole sweep, and `diff` reports
 *  the missing tail as ordinary differences. Here it costs one row, and the
 *  driver's `timeout` turns "never returned" into a printed outcome.
 *
 *  Usage: UnsafeSubwordProbe <index> | UnsafeSubwordProbe count
 */
public class UnsafeSubwordProbe {

    static sun.misc.Unsafe u;
    static jdk.internal.misc.Unsafe v;

    static class H {
        boolean z = true;
        byte b = 10;
        char c = 'A';
        short s = 20;
        int i = 30;
        long l = 40L;
    }

    static long off(String n) throws Exception {
        return u.objectFieldOffset(H.class.getDeclaredField(n));
    }

    static final String[] TAGS = {
        /*  0 */ "compareAndSetByte right witness",
        /*  1 */ "compareAndSetByte wrong witness",
        /*  2 */ "weakCompareAndSetByte right witness",
        /*  3 */ "compareAndExchangeByte",
        /*  4 */ "getAndSetByte",
        /*  5 */ "getAndAddByte (a registered native, the control)",
        /*  6 */ "getAndBitwiseOrByte",
        /*  7 */ "getAndBitwiseAndByte",
        /*  8 */ "getAndBitwiseXorByte",
        /*  9 */ "compareAndSetShort right witness",
        /* 10 */ "compareAndSetShort wrong witness",
        /* 11 */ "getAndSetShort",
        /* 12 */ "getAndAddShort (a registered native, the control)",
        /* 13 */ "getAndBitwiseOrShort",
        /* 14 */ "compareAndSetBoolean right witness",
        /* 15 */ "getAndSetBoolean",
        /* 16 */ "getAndBitwiseAndBoolean",
        /* 17 */ "compareAndSetChar right witness",
        /* 18 */ "getAndSetChar",
        /* 19 */ "compareAndSetInt right witness (a registered native, the control)",
        /* 20 */ "getAndBitwiseOrInt (bytecode over a registered CAS, the control)",
        /* 21 */ "compareAndSetFloat right witness",
        /* 22 */ "compareAndSetDouble right witness",
        /* 23 */ "getAndBitwiseOrLong (bytecode over a registered CAS, the control)",
        /* 24 */ "compareAndSetByte on a byte[] element",
        /* 25 */ "getAndBitwiseOrByte on a byte[] element",
    };

    static Object run(int n) throws Throwable {
        H h = new H();
        switch (n) {
            case 0: {
                boolean r = v.compareAndSetByte(h, off("b"), (byte) 10, (byte) 11);
                return r + " b=" + h.b;
            }
            case 1: {
                boolean r = v.compareAndSetByte(h, off("b"), (byte) 99, (byte) 11);
                return r + " b=" + h.b;
            }
            case 2: {
                boolean r = v.weakCompareAndSetByte(h, off("b"), (byte) 10, (byte) 11);
                return r + " b=" + h.b;
            }
            case 3: {
                byte r = v.compareAndExchangeByte(h, off("b"), (byte) 10, (byte) 11);
                return r + " b=" + h.b;
            }
            case 4: {
                byte r = v.getAndSetByte(h, off("b"), (byte) 12);
                return r + " b=" + h.b;
            }
            case 5: {
                byte r = v.getAndAddByte(h, off("b"), (byte) 1);
                return r + " b=" + h.b;
            }
            case 6: {
                byte r = v.getAndBitwiseOrByte(h, off("b"), (byte) 0x05);
                return r + " b=" + h.b;
            }
            case 7: {
                byte r = v.getAndBitwiseAndByte(h, off("b"), (byte) 0x0C);
                return r + " b=" + h.b;
            }
            case 8: {
                byte r = v.getAndBitwiseXorByte(h, off("b"), (byte) 0x0F);
                return r + " b=" + h.b;
            }
            case 9: {
                boolean r = v.compareAndSetShort(h, off("s"), (short) 20, (short) 21);
                return r + " s=" + h.s;
            }
            case 10: {
                boolean r = v.compareAndSetShort(h, off("s"), (short) 99, (short) 21);
                return r + " s=" + h.s;
            }
            case 11: {
                short r = v.getAndSetShort(h, off("s"), (short) 22);
                return r + " s=" + h.s;
            }
            case 12: {
                short r = v.getAndAddShort(h, off("s"), (short) 1);
                return r + " s=" + h.s;
            }
            case 13: {
                short r = v.getAndBitwiseOrShort(h, off("s"), (short) 0x05);
                return r + " s=" + h.s;
            }
            case 14: {
                boolean r = v.compareAndSetBoolean(h, off("z"), true, false);
                return r + " z=" + h.z;
            }
            case 15: {
                boolean r = v.getAndSetBoolean(h, off("z"), false);
                return r + " z=" + h.z;
            }
            case 16: {
                boolean r = v.getAndBitwiseAndBoolean(h, off("z"), false);
                return r + " z=" + h.z;
            }
            case 17: {
                boolean r = v.compareAndSetChar(h, off("c"), 'A', 'B');
                return r + " c=" + h.c;
            }
            case 18: {
                char r = v.getAndSetChar(h, off("c"), 'C');
                return r + " c=" + h.c;
            }
            case 19: {
                boolean r = v.compareAndSetInt(h, off("i"), 30, 31);
                return r + " i=" + h.i;
            }
            case 20: {
                int r = v.getAndBitwiseOrInt(h, off("i"), 0x05);
                return r + " i=" + h.i;
            }
            case 21: {
                boolean r = v.compareAndSetFloat(h, off("i") /* unused */, 0f, 0f);
                return "n/a " + r;
            }
            case 22: {
                boolean r = v.compareAndSetDouble(h, off("l") /* unused */, 0d, 0d);
                return "n/a " + r;
            }
            case 23: {
                long r = v.getAndBitwiseOrLong(h, off("l"), 0x05L);
                return r + " l=" + h.l;
            }
            case 24: {
                byte[] a = { 1, 2, 3, 4, 5, 6, 7, 8 };
                int base = u.arrayBaseOffset(byte[].class);
                boolean r = v.compareAndSetByte(a, base + 1L, (byte) 2, (byte) 9);
                return r + " a=" + java.util.Arrays.toString(a);
            }
            case 25: {
                byte[] a = { 1, 2, 3, 4, 5, 6, 7, 8 };
                int base = u.arrayBaseOffset(byte[].class);
                byte r = v.getAndBitwiseOrByte(a, base + 1L, (byte) 0x10);
                return r + " a=" + java.util.Arrays.toString(a);
            }
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
