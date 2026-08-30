import java.lang.reflect.Field;

/** L1 R5, the naming measurement: are `sun.misc.Unsafe`'s two latch constants
 *  INITIALISED on this VM, or are they still at their defaults?
 *
 *  `StaticBaseProbe` showed the general static-field pair is CORRECT here --
 *  non-null base, distinct non-zero offsets, a working false->true CAS latch,
 *  byte-identical to HotSpot. So R5's `base=null, offset=0` is not a
 *  `staticFieldBase`/`staticFieldOffset` defect.
 *
 *  Which leaves the other reading: nothing RESOLVED to null and 0 -- those are
 *  simply the DEFAULT values of `MEMORY_ACCESS_WARNED_BASE` (an Object) and
 *  `MEMORY_ACCESS_WARNED_OFFSET` (a long), i.e. `sun.misc.Unsafe.<clinit>`
 *  never assigned them. A zero read as an answer, when it is really an
 *  uninitialised field.
 *
 *  Diff on stdout against HotSpot. Prints no offset VALUE -- that legitimately
 *  differs between VMs -- only whether it is the default.
 */
public class WarnLatchProbe {

    static void p(String tag, Object v) {
        System.out.println(tag + " |" + v + "|");
    }

    public static void main(String[] args) throws Exception {
        Class<?> su = Class.forName("sun.misc.Unsafe");

        Field base = su.getDeclaredField("MEMORY_ACCESS_WARNED_BASE");
        Field off = su.getDeclaredField("MEMORY_ACCESS_WARNED_OFFSET");
        base.setAccessible(true);
        off.setAccessible(true);

        Object b = base.get(null);
        long o = off.getLong(null);

        p("MEMORY_ACCESS_WARNED_BASE is non-null", b != null);
        p("MEMORY_ACCESS_WARNED_OFFSET is non-zero", o != 0L);
        p("base is the Unsafe class mirror", b == su);

        // The field the pair is supposed to address. Its own offset, taken the
        // ordinary way, is the control: if THIS is non-zero while the constant
        // above is zero, the pair was never computed rather than computed wrong.
        Field warned = su.getDeclaredField("memoryAccessWarned");
        warned.setAccessible(true);
        p("memoryAccessWarned is readable", warned.getBoolean(null) || true);

        Field tiu = su.getDeclaredField("theInternalUnsafe");
        tiu.setAccessible(true);
        jdk.internal.misc.Unsafe u = (jdk.internal.misc.Unsafe) tiu.get(null);
        p("control: its own staticFieldOffset is non-zero",
          u.staticFieldOffset(warned) != 0L);
        p("control: its own staticFieldBase is non-null",
          u.staticFieldBase(warned) != null);

        System.out.println("DONE WarnLatchProbe");
    }
}
