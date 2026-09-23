import java.lang.reflect.Field;

/** L1 R5, closing measurement: what does this VM answer for the
 *  `staticFieldBase`/`staticFieldOffset` PAIR?
 *
 *  The full frame chain named the producer exactly:
 *
 *    sun/misc/Unsafe.invokeCleaner+18
 *      -> beforeMemoryAccess+19 -> isMemoryAccessWarned+9
 *      -> beforeMemoryAccessSlow+104 -> trySetMemoryAccessWarned+11
 *
 *  and `javap` on JDK 25's `sun.misc.Unsafe` shows both of those read
 *  `MEMORY_ACCESS_WARNED_BASE` (an Object) and `MEMORY_ACCESS_WARNED_OFFSET`
 *  (a long) and pass them straight to `getBooleanVolatile(Object,J)` /
 *  `compareAndSetBoolean(Object,JZZ)`. Those two constants are `<clinit>`'s
 *  `staticFieldBase`/`staticFieldOffset` of the static `boolean
 *  memoryAccessWarned`.
 *
 *  So R5's `base=null, offset=0` is a prediction about THIS pair, and this
 *  probe tests it directly -- on a field of its own, so it works on both VMs
 *  and needs no access to a JDK-internal private field.
 *
 *  Diff on stdout against HotSpot. Prints no addresses (they differ per run by
 *  construction); prints only what is comparable.
 */
public class StaticBaseProbe {

    static int si = 7;
    static boolean sb = false;
    static long sl = 11L;

    static void p(String tag, Object v) {
        System.out.println(tag + " |" + v + "|");
    }

    public static void main(String[] args) throws Exception {
        Field f = Class.forName("sun.misc.Unsafe").getDeclaredField("theInternalUnsafe");
        f.setAccessible(true);
        jdk.internal.misc.Unsafe u = (jdk.internal.misc.Unsafe) f.get(null);

        Field fi = StaticBaseProbe.class.getDeclaredField("si");
        Field fb = StaticBaseProbe.class.getDeclaredField("sb");
        Field fl = StaticBaseProbe.class.getDeclaredField("sl");

        Object bi = u.staticFieldBase(fi);
        Object bb = u.staticFieldBase(fb);
        long oi = u.staticFieldOffset(fi);
        long ob = u.staticFieldOffset(fb);
        long ol = u.staticFieldOffset(fl);

        // THE prediction: HotSpot answers a non-null base for a static field.
        p("static base is non-null (int)", bi != null);
        p("static base is non-null (boolean)", bb != null);
        p("static bases agree across fields of one class", bi == bb);
        // Offsets must be DISTINCT per field -- an all-zero answer aliases
        // every static field of every class onto one slot, which is the shape
        // R5 predicts.
        p("static offsets distinct (int vs boolean)", oi != ob);
        p("static offsets distinct (int vs long)", oi != ol);
        p("int static offset is zero", oi == 0);
        p("boolean static offset is zero", ob == 0);

        // Round-trips THROUGH the pair, which is what the JDK's own latch does.
        p("getInt via pair", u.getIntVolatile(bi, oi));
        u.putIntVolatile(bi, oi, 99);
        p("putInt via pair then read field", si);
        p("getBoolean via pair", u.getBooleanVolatile(bb, ob));

        // The latch's exact shape: false -> true exactly once.
        p("first CAS false->true", u.compareAndSetBoolean(bb, ob, false, true));
        p("second CAS false->true", u.compareAndSetBoolean(bb, ob, false, true));
        p("field observes the CAS", sb);

        System.out.println("DONE StaticBaseProbe");
    }
}
