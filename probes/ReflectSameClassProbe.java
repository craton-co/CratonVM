import java.lang.reflect.Method;

/**
 * Reflective invoke of a non-public method WITHOUT setAccessible(true).
 *
 * Legal when the caller is the declaring class itself — the same rule that
 * lets a class read its own private field reflectively. Both halves are here,
 * because a carve-out that also let outsiders in would pass a test that only
 * checked the positive case.
 *
 * The negative control is a separate TOP-LEVEL class on purpose. A nested class
 * would be a nestmate (JEP 181) and HotSpot grants it private access, so it
 * proves nothing about the boundary.
 */
public class ReflectSameClassProbe {

    private static int privateStatic(int x) { return x * 3; }
    static int packagePrivateStatic(int x) { return x * 5; }
    private int privateInstance(int x) { return x * 7; }

    public static void main(String[] args) throws Exception {
        // Same class, no setAccessible: must work.
        Method ps = ReflectSameClassProbe.class.getDeclaredMethod("privateStatic", int.class);
        System.out.println("privateStatic=" + ps.invoke(null, 4));

        Method pp = ReflectSameClassProbe.class.getDeclaredMethod("packagePrivateStatic", int.class);
        System.out.println("packagePrivateStatic=" + pp.invoke(null, 4));

        Method pi = ReflectSameClassProbe.class.getDeclaredMethod("privateInstance", int.class);
        System.out.println("privateInstance=" + pi.invoke(new ReflectSameClassProbe(), 4));

        // A different top-level class — not a nestmate — no setAccessible:
        // must still be refused.
        Method op = ReflectOtherTop.class.getDeclaredMethod("otherPrivate", int.class);
        String outcome;
        try {
            op.invoke(null, 4);
            outcome = "ALLOWED";
        } catch (IllegalAccessException e) {
            outcome = "IllegalAccessException";
        }
        System.out.println("otherClassPrivate=" + outcome);

        // ...and allowed once the override is set.
        op.setAccessible(true);
        System.out.println("otherClassPrivateAccessible=" + op.invoke(null, 4));
        System.out.println("OK");
    }
}

class ReflectOtherTop {
    private static int otherPrivate(int x) { return x * 11; }
}
