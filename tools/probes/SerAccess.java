import java.lang.reflect.Constructor;
import java.lang.reflect.Modifier;
import sun.reflect.ReflectionFactory;

/**
 * Next step for
 * docs/known-issues/jdk-only/the-serialization-constructor-is-refused-by-the-module-check-on-both-images-20260909.md.
 *
 * That page ends on a question rather than a cause: CratonVM's NEW-19 arm reads
 * an accessible flag out of an extra slot, and `ReflectionFactory` marks the
 * serialization constructor accessible itself -- so does the flag the JDK set
 * survive into the slot, or does the VM never see it?
 *
 * This probe asks the flag DIRECTLY, before invoking anything, so a refusal at
 * `newInstance` can be attributed to a missing flag rather than to the module
 * check being wrong about modules. `java.lang.Integer` is the control: its
 * package is one HotSpot and CratonVM both allow, so a run where the control
 * throws is a broken probe rather than a finding.
 *
 * It deliberately does NOT call `setAccessible(true)` on the constructor. Doing
 * so reproduces the identical InaccessibleObjectException on HotSpot and is a
 * very good way to convince yourself the VM is right when it is not -- the page
 * above records that trap.
 */
public class SerAccess {

    static void one(Class<?> target) {
        System.out.println("  " + target.getName());
        Constructor<?> c;
        try {
            c = ReflectionFactory.getReflectionFactory().newConstructorForSerialization(target);
        } catch (Throwable t) {
            System.out.println("    newConstructorForSerialization THREW " + t);
            return;
        }
        if (c == null) {
            System.out.println("    newConstructorForSerialization returned NULL");
            return;
        }
        System.out.println("    ctor class          = " + c.getClass().getName());
        System.out.println("    declaring class     = " + c.getDeclaringClass().getName());
        System.out.println("    modifiers           = 0x" + Integer.toHexString(c.getModifiers())
                + " public=" + Modifier.isPublic(c.getModifiers()));
        // THE question. The JDK sets this inside generateConstructor; if it does
        // not read back as true, the flag never reached the VM slot NEW-19 uses.
        try {
            System.out.println("    isAccessible()      = " + c.isAccessible());
        } catch (Throwable t) {
            System.out.println("    isAccessible()      -> " + t);
        }
        try {
            System.out.println("    canAccess(null)     = " + c.canAccess(null));
        } catch (Throwable t) {
            System.out.println("    canAccess(null)     -> " + t);
        }
        try {
            Object o = c.newInstance();
            System.out.println("    newInstance()       = " + o.getClass().getName()
                    + (o.getClass() == target ? "   OK" : "   WRONG CLASS"));
        } catch (Throwable t) {
            Throwable r = t;
            while (r.getCause() != null) {
                r = r.getCause();
            }
            System.out.println("    newInstance()       -> " + r.getClass().getName()
                    + ": " + r.getMessage());
        }
    }

    public static void main(String[] args) throws Exception {
        System.out.println("java.version = " + System.getProperty("java.version"));
        System.out.println();
        System.out.println("== serialization constructors ==");
        one(Integer.class);        // java.lang -- the control, allowed everywhere
        one(java.util.ArrayList.class);
        one(java.util.HashMap.class);
        System.out.println();

        // A second control, for the OTHER half of the NEW-19 arm: an ordinary
        // PUBLIC constructor in the same package java.util. JEP 261 says this
        // needs `exports` only, never `opens`, so it must succeed on both VMs.
        // If this throws while the serialization rows also throw, the fault is
        // the module check generally and not the serialization flag.
        System.out.println("== ordinary public ctor in the same package (needs exports only) ==");
        try {
            Constructor<?> pub = java.util.ArrayList.class.getConstructor(int.class);
            Object o = pub.newInstance(16);
            System.out.println("  ArrayList(int) -> " + o.getClass().getName() + "   OK");
        } catch (Throwable t) {
            System.out.println("  ArrayList(int) -> " + t);
        }
        System.out.println();
        System.out.println("DONE");
    }
}
