import java.lang.reflect.Field;
import java.lang.reflect.Method;

/**
 * L5 residual §9.2 -- a workload for `sun.misc.Unsafe`, whose 82 registered
 * rows no instrument in this tree reaches.
 *
 * The retired lane page holds those rows with the blocker "precondition 1
 * fails by measurement": all 121 probes report the dial VACUOUS on that scope
 * and the 132 `--jdk-only` corpus reports reach 18 of the 82. Its own §9 says
 * that until a workload exists, **the count is not evidence of anything** --
 * not of the rows being right, not of their being retirable, and not of their
 * being dead.
 *
 * This is that workload. It calls the surface directly and records, per
 * method, whether the call is even POSSIBLE on this JDK:
 *
 * <pre>
 *   name -&gt; ok=&lt;property&gt;      the call returned and the property holds
 *   name -&gt; EX:&lt;SimpleName&gt;    the call threw, and this is what it threw
 * </pre>
 *
 * # Why an exception is a result here rather than a failure
 *
 * `sun.misc.Unsafe`'s memory-access methods are terminally deprecated and the
 * JDK has been degrading them release by release. A row that throws
 * `UnsupportedOperationException` on JDK 25 for BOTH VMs is not a defect and
 * not a retirement candidate either -- it is a registration that can never be
 * dispatched on a supported image, which is a DELETION under lane 0 §1, the
 * same verdict `AbstractExecutorService`'s four rows got. A row that answers on
 * HotSpot and throws here is the opposite: a live defect. The two are
 * indistinguishable from a census, which is why this file exists.
 *
 * # What is deliberately NOT printed
 *
 * No offset, no address, no `arrayBaseOffset`, no raw `addressSize`. Those are
 * a VM's own numbering -- CratonVM answers `objectFieldOffset` with a SLOT
 * INDEX where HotSpot answers a byte offset -- so printing one makes every row
 * differ for a reason that is not a defect. What is printed is the property
 * that has to hold whatever the numbering is: a value written through the
 * offset is the value read back through it, and plain bytecode agrees.
 *
 * Check the `rows` trailer before believing a clean diff.
 */
public class L5SunMiscUnsafe {
    static int rows;
    static Object U;
    static Class<?> UC;

    static void say(String s) {
        rows++;
        System.out.println(s);
    }

    /** Every call goes through here so a throw is a ROW rather than an exit. */
    static void probe(String name, Call c) {
        String out;
        try {
            out = "ok=" + c.run();
        } catch (Throwable t) {
            Throwable r = t;
            // Reflection wraps; the interesting class is the cause.
            while (r instanceof java.lang.reflect.InvocationTargetException && r.getCause() != null) {
                r = r.getCause();
            }
            out = "EX:" + r.getClass().getSimpleName();
        }
        say(name + " -> " + out);
    }

    interface Call {
        Object run() throws Throwable;
    }

    /** A carrier with one field of each shape the accessors cover. */
    static class Holder {
        boolean z = false;
        byte b = 0;
        char c = 0;
        short s = 0;
        int i = 0;
        long j = 0;
        float f = 0;
        double d = 0;
        Object o = null;
        static int sInt = 0;
    }

    static Method m(String name, Class<?>... types) throws Exception {
        return UC.getMethod(name, types);
    }

    static long off(String field) throws Exception {
        Field f = Holder.class.getDeclaredField(field);
        return (Long) m("objectFieldOffset", Field.class).invoke(U, f);
    }

    public static void main(String[] args) throws Exception {
        // `theUnsafe` rather than `getUnsafe()`: the latter checks the caller's
        // class loader and throws for an application class on both VMs, so it
        // would measure the check rather than the surface. The check itself IS
        // a row below.
        try {
            UC = Class.forName("sun.misc.Unsafe");
            Field tu = UC.getDeclaredField("theUnsafe");
            tu.setAccessible(true);
            U = tu.get(null);
        } catch (Throwable t) {
            say("theUnsafe -> EX:" + t.getClass().getSimpleName());
            System.out.println("rows " + rows);
            System.out.println("DONE L5SunMiscUnsafe");
            return;
        }
        say("theUnsafe -> ok=" + (U != null));
        say("class -> ok=" + UC.getName());

        probe("getUnsafe", () -> {
            try {
                return m("getUnsafe").invoke(null) != null;
            } catch (java.lang.reflect.InvocationTargetException e) {
                throw e.getCause();
            }
        });

        // ---- the shape-independent queries ----
        probe("addressSize", () -> ((Integer) m("addressSize").invoke(U)) > 0);
        probe("pageSize", () -> ((Integer) m("pageSize").invoke(U)) > 0);
        probe("arrayIndexScalePositive", () ->
                ((Integer) m("arrayIndexScale", Class.class).invoke(U, int[].class)) > 0);
        probe("arrayBaseOffsetNonNegative", () ->
                ((Integer) m("arrayBaseOffset", Class.class).invoke(U, int[].class)) >= 0);

        // ---- the fences: no observable value, so the row is "it returned" ----
        probe("loadFence", () -> {
            m("loadFence").invoke(U);
            return "returned";
        });
        probe("storeFence", () -> {
            m("storeFence").invoke(U);
            return "returned";
        });
        probe("fullFence", () -> {
            m("fullFence").invoke(U);
            return "returned";
        });

        // ---- field accessors, as round-trips ----
        Holder h = new Holder();
        probe("int roundTrip", () -> {
            long o = off("i");
            m("putInt", Object.class, long.class, int.class).invoke(U, h, o, 0x5eed);
            int viaUnsafe = (Integer) m("getInt", Object.class, long.class).invoke(U, h, o);
            return viaUnsafe == 0x5eed && h.i == 0x5eed;
        });
        probe("intVolatile roundTrip", () -> {
            long o = off("i");
            m("putIntVolatile", Object.class, long.class, int.class).invoke(U, h, o, 0x1234);
            int viaUnsafe = (Integer) m("getIntVolatile", Object.class, long.class).invoke(U, h, o);
            return viaUnsafe == 0x1234 && h.i == 0x1234;
        });
        probe("long roundTrip", () -> {
            long o = off("j");
            m("putLong", Object.class, long.class, long.class).invoke(U, h, o, 0x1122334455667788L);
            long viaUnsafe = (Long) m("getLong", Object.class, long.class).invoke(U, h, o);
            return viaUnsafe == 0x1122334455667788L && h.j == 0x1122334455667788L;
        });
        probe("object roundTrip", () -> {
            long o = off("o");
            m("putObject", Object.class, long.class, Object.class).invoke(U, h, o, "held");
            Object viaUnsafe = m("getObject", Object.class, long.class).invoke(U, h, o);
            return "held".equals(viaUnsafe) && "held".equals(h.o);
        });
        probe("objectVolatile roundTrip", () -> {
            long o = off("o");
            m("putObjectVolatile", Object.class, long.class, Object.class).invoke(U, h, o, "vol");
            Object viaUnsafe = m("getObjectVolatile", Object.class, long.class).invoke(U, h, o);
            return "vol".equals(viaUnsafe) && "vol".equals(h.o);
        });
        probe("boolean roundTrip", () -> {
            long o = off("z");
            m("putBoolean", Object.class, long.class, boolean.class).invoke(U, h, o, true);
            boolean viaUnsafe = (Boolean) m("getBoolean", Object.class, long.class).invoke(U, h, o);
            return viaUnsafe && h.z;
        });
        probe("byte roundTrip", () -> {
            long o = off("b");
            m("putByte", Object.class, long.class, byte.class).invoke(U, h, o, (byte) 0x5a);
            byte viaUnsafe = (Byte) m("getByte", Object.class, long.class).invoke(U, h, o);
            return viaUnsafe == (byte) 0x5a && h.b == (byte) 0x5a;
        });
        probe("char roundTrip", () -> {
            long o = off("c");
            m("putChar", Object.class, long.class, char.class).invoke(U, h, o, 'Q');
            char viaUnsafe = (Character) m("getChar", Object.class, long.class).invoke(U, h, o);
            return viaUnsafe == 'Q' && h.c == 'Q';
        });
        probe("short roundTrip", () -> {
            long o = off("s");
            m("putShort", Object.class, long.class, short.class).invoke(U, h, o, (short) 4242);
            short viaUnsafe = (Short) m("getShort", Object.class, long.class).invoke(U, h, o);
            return viaUnsafe == (short) 4242 && h.s == (short) 4242;
        });
        probe("float roundTrip", () -> {
            long o = off("f");
            m("putFloat", Object.class, long.class, float.class).invoke(U, h, o, 2.5f);
            float viaUnsafe = (Float) m("getFloat", Object.class, long.class).invoke(U, h, o);
            return viaUnsafe == 2.5f && h.f == 2.5f;
        });
        probe("double roundTrip", () -> {
            long o = off("d");
            m("putDouble", Object.class, long.class, double.class).invoke(U, h, o, 6.25d);
            double viaUnsafe = (Double) m("getDouble", Object.class, long.class).invoke(U, h, o);
            return viaUnsafe == 6.25d && h.d == 6.25d;
        });

        // ---- the atomics `AtomicInteger` and friends were built on ----
        probe("compareAndSwapInt", () -> {
            long o = off("i");
            m("putInt", Object.class, long.class, int.class).invoke(U, h, o, 7);
            boolean hit = (Boolean) m("compareAndSwapInt", Object.class, long.class, int.class, int.class)
                    .invoke(U, h, o, 7, 8);
            boolean miss = (Boolean) m("compareAndSwapInt", Object.class, long.class, int.class, int.class)
                    .invoke(U, h, o, 7, 9);
            return hit + "/" + miss + "/" + h.i;
        });
        probe("compareAndSwapLong", () -> {
            long o = off("j");
            m("putLong", Object.class, long.class, long.class).invoke(U, h, o, 7L);
            boolean hit = (Boolean) m("compareAndSwapLong", Object.class, long.class, long.class, long.class)
                    .invoke(U, h, o, 7L, 8L);
            boolean miss = (Boolean) m("compareAndSwapLong", Object.class, long.class, long.class, long.class)
                    .invoke(U, h, o, 7L, 9L);
            return hit + "/" + miss + "/" + h.j;
        });
        probe("compareAndSwapObject", () -> {
            long o = off("o");
            m("putObject", Object.class, long.class, Object.class).invoke(U, h, o, "a");
            boolean hit = (Boolean) m("compareAndSwapObject", Object.class, long.class, Object.class, Object.class)
                    .invoke(U, h, o, "a", "b");
            return hit + "/" + h.o;
        });
        probe("getAndAddInt", () -> {
            long o = off("i");
            m("putInt", Object.class, long.class, int.class).invoke(U, h, o, 10);
            int prev = (Integer) m("getAndAddInt", Object.class, long.class, int.class).invoke(U, h, o, 5);
            return prev + "/" + h.i;
        });
        probe("getAndSetInt", () -> {
            long o = off("i");
            m("putInt", Object.class, long.class, int.class).invoke(U, h, o, 11);
            int prev = (Integer) m("getAndSetInt", Object.class, long.class, int.class).invoke(U, h, o, 12);
            return prev + "/" + h.i;
        });
        probe("getAndSetObject", () -> {
            long o = off("o");
            m("putObject", Object.class, long.class, Object.class).invoke(U, h, o, "x");
            Object prev = m("getAndSetObject", Object.class, long.class, Object.class).invoke(U, h, o, "y");
            return prev + "/" + h.o;
        });

        // ---- the static-field pair ----
        probe("staticFieldRoundTrip", () -> {
            Field f = Holder.class.getDeclaredField("sInt");
            Object base = m("staticFieldBase", Field.class).invoke(U, f);
            long o = (Long) m("staticFieldOffset", Field.class).invoke(U, f);
            m("putInt", Object.class, long.class, int.class).invoke(U, base, o, 0xabc);
            int viaUnsafe = (Integer) m("getInt", Object.class, long.class).invoke(U, base, o);
            return viaUnsafe == 0xabc && Holder.sInt == 0xabc;
        });

        // ---- off-heap ----
        probe("allocate/put/get/free", () -> {
            long addr = (Long) m("allocateMemory", long.class).invoke(U, 16L);
            m("putLong", long.class, long.class).invoke(U, addr, 0x0fedcba987654321L);
            long back = (Long) m("getLong", long.class).invoke(U, addr);
            m("freeMemory", long.class).invoke(U, addr);
            return back == 0x0fedcba987654321L;
        });
        probe("setMemory", () -> {
            long addr = (Long) m("allocateMemory", long.class).invoke(U, 8L);
            m("setMemory", long.class, long.class, byte.class).invoke(U, addr, 8L, (byte) 0x7f);
            byte b0 = (Byte) m("getByte", long.class).invoke(U, addr);
            m("freeMemory", long.class).invoke(U, addr);
            return b0 == (byte) 0x7f;
        });

        // ---- object and class services ----
        probe("allocateInstance", () -> {
            Object o = m("allocateInstance", Class.class).invoke(U, Holder.class);
            return o != null && o.getClass() == Holder.class && ((Holder) o).i == 0;
        });
        probe("ensureClassInitialized", () -> {
            m("ensureClassInitialized", Class.class).invoke(U, Holder.class);
            return "returned";
        });
        probe("shouldBeInitialized", () -> m("shouldBeInitialized", Class.class).invoke(U, Holder.class));
        probe("getLoadAverage", () -> {
            double[] a = new double[1];
            return m("getLoadAverage", double[].class, int.class).invoke(U, a, 1);
        });

        // ---- park/unpark: unpark THEN park cannot block ----
        probe("unparkThenPark", () -> {
            m("unpark", Object.class).invoke(U, Thread.currentThread());
            m("park", boolean.class, long.class).invoke(U, false, 0L);
            return "returned";
        });

        // ---- throwException: the one that must NOT return ----
        probe("throwException", () -> {
            m("throwException", Throwable.class).invoke(U, new IllegalStateException("probe"));
            return "RETURNED-WITHOUT-THROWING";
        });

        System.out.println("rows " + rows);
        System.out.println("DONE L5SunMiscUnsafe");
    }
}
