import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

/**
 * F29-1's stated Acceptance, run as written:
 *   vh.fieldLong      must be a java.lang.Long carrying 5, and == Long.valueOf(5)
 *   vh.getAndSetLong  the same
 *   vh.fieldDouble    must carry 1.5, and be != Double.valueOf(1.5)
 *
 * The identity rows are the point: Long.valueOf caches -128..127 so a correctly
 * boxed 5 is the cached instance, while Double.valueOf caches nothing, so a
 * correctly boxed 1.5 is a fresh object. A VarHandle that returns a raw
 * primitive-shaped value, or boxes via the wrong wrapper class, moves one of
 * these without moving the other.
 */
public class VhBoxProbe {
    static class Holder {
        long fieldLong = 5L;
        double fieldDouble = 1.5d;
    }

    static void show(String label, Object v) {
        System.out.println(label + ".class = " + (v == null ? "null" : v.getClass().getName()));
        System.out.println(label + ".value = " + v);
    }

    public static void main(String[] a) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        Holder h = new Holder();

        VarHandle vhL = l.findVarHandle(Holder.class, "fieldLong", long.class);
        Object gotL = vhL.get(h);
        show("vh.fieldLong", gotL);
        System.out.println("vh.fieldLong.isLong = " + (gotL instanceof Long));
        System.out.println("vh.fieldLong.carries5 = " + Long.valueOf(5L).equals(gotL));
        System.out.println("vh.fieldLong.identityCached = " + (gotL == Long.valueOf(5L)));

        Object gasL = vhL.getAndSet(h, 7L);
        show("vh.getAndSetLong", gasL);
        System.out.println("vh.getAndSetLong.isLong = " + (gasL instanceof Long));
        System.out.println("vh.getAndSetLong.carries5 = " + Long.valueOf(5L).equals(gasL));
        System.out.println("vh.getAndSetLong.identityCached = " + (gasL == Long.valueOf(5L)));

        VarHandle vhD = l.findVarHandle(Holder.class, "fieldDouble", double.class);
        Object gotD = vhD.get(h);
        show("vh.fieldDouble", gotD);
        System.out.println("vh.fieldDouble.isDouble = " + (gotD instanceof Double));
        System.out.println("vh.fieldDouble.carries1_5 = " + Double.valueOf(1.5d).equals(gotD));
        System.out.println("vh.fieldDouble.notIdentity = " + (gotD != Double.valueOf(1.5d)));

        System.out.println("RESULT done");
    }
}
