import java.lang.reflect.Field;

/** L1 R5: did `sun.misc.Unsafe.<clinit>` run, and how far?
 *
 *  `javap` puts the two latch constants at bci 185 and 195 of `<clinit>`,
 *  inside a try/catch(Exception) that rethrows as ExceptionInInitializerError.
 *  The class initialises cleanly on this VM, so the catch did not fire -- yet
 *  both constants hold their defaults. Fields written BEFORE and AFTER them
 *  separate "clinit never ran" from "these two putstatics did not land".
 *
 *    bci  40 theInternalUnsafe   (before)
 *    bci 157 ARRAY_OBJECT_INDEX_SCALE (before)
 *    bci 166 ADDRESS_SIZE        (immediately before the try block)
 *    bci 185 MEMORY_ACCESS_WARNED_BASE    <-- default here
 *    bci 195 MEMORY_ACCESS_WARNED_OFFSET  <-- default here
 *    bci 214 MEMORY_ACCESS_OPTION (AFTER the try block; set only if it fell
 *                                  through rather than throwing)
 *
 *  Diff on stdout against HotSpot.
 */
public class ClinitProbe {

    static void p(String tag, Object v) {
        System.out.println(tag + " |" + v + "|");
    }

    static Object peek(Class<?> c, String name) {
        try {
            Field f = c.getDeclaredField(name);
            f.setAccessible(true);
            return f.get(null);
        } catch (Throwable t) {
            return "<" + t.getClass().getName() + ">";
        }
    }

    public static void main(String[] args) throws Exception {
        Class<?> su = Class.forName("sun.misc.Unsafe");

        p("bci40  theInternalUnsafe set", peek(su, "theInternalUnsafe") != null);
        p("bci34  theUnsafe set", peek(su, "theUnsafe") != null);
        p("bci157 ARRAY_OBJECT_INDEX_SCALE non-zero",
          !Integer.valueOf(0).equals(peek(su, "ARRAY_OBJECT_INDEX_SCALE")));
        p("bci166 ADDRESS_SIZE non-zero",
          !Integer.valueOf(0).equals(peek(su, "ADDRESS_SIZE")));
        p("bci185 MEMORY_ACCESS_WARNED_BASE set",
          peek(su, "MEMORY_ACCESS_WARNED_BASE") != null);
        p("bci195 MEMORY_ACCESS_WARNED_OFFSET non-zero",
          !Long.valueOf(0L).equals(peek(su, "MEMORY_ACCESS_WARNED_OFFSET")));
        p("bci214 MEMORY_ACCESS_OPTION set",
          peek(su, "MEMORY_ACCESS_OPTION") != null);

        // Can `<clinit>`'s own two calls be made from here, right now? If they
        // succeed on demand but the constants are default, the calls were
        // never MADE rather than having failed.
        Field tiu = su.getDeclaredField("theInternalUnsafe");
        tiu.setAccessible(true);
        jdk.internal.misc.Unsafe u = (jdk.internal.misc.Unsafe) tiu.get(null);
        Field warned = su.getDeclaredField("memoryAccessWarned");
        p("replay staticFieldBase non-null", u.staticFieldBase(warned) != null);
        p("replay staticFieldOffset non-zero", u.staticFieldOffset(warned) != 0L);
        p("replay getDeclaredField works", warned != null);

        System.out.println("DONE ClinitProbe");
    }
}
