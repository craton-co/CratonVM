// RBIGDEC.1 layout-introspection probe.  Dumps the runtime values of the
// BigDecimal/BigInteger instance fields for the boot constants so we can
// compare HotSpot vs. CratonVM byte for byte and verify the post-clinit
// fixup populates the real-JDK layout correctly.
import java.lang.reflect.*;
import java.math.*;

public class BdLayout {
    public static void main(String[] a) throws Exception {
        for (String name : new String[]{"intVal","scale","precision","stringCache","intCompact"}) {
            Field f = BigDecimal.class.getDeclaredField(name);
            f.setAccessible(true);
            Object v = f.get(BigDecimal.ONE);
            System.out.println("BD.ONE." + name + " = " + v + " (type=" + (v==null?"null":v.getClass().getName()) + ")");
        }
        for (String name : new String[]{"signum","mag"}) {
            Field f = BigInteger.class.getDeclaredField(name);
            f.setAccessible(true);
            Object v = f.get(BigInteger.ONE);
            System.out.println("BI.ONE." + name + " = " + (v instanceof int[] ? java.util.Arrays.toString((int[])v) : v));
        }
    }
}
