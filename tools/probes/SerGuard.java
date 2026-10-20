import java.lang.reflect.Field;
import sun.reflect.ReflectionFactory;

/**
 * The fail-open control for the `ReflectionFactory` caller-visibility fix.
 *
 * Exposing the `ReflectionFactory` frames means a `setAccessible(true)` made BY
 * ReflectionFactory is attributed to `java.base` and allowed. The danger of
 * that change is that it might also hand application code deep access it must
 * not have -- so this asks the encapsulation question DIRECTLY, from an
 * ordinary classpath class, with no ReflectionFactory anywhere on the stack.
 *
 * Every DENIED line below must stay denied, and must be denied identically on
 * HotSpot. A run where HotSpot allows one of them is a broken probe (someone
 * passed --add-opens); a run where CratonVM allows one that HotSpot denies is
 * the fix being fail-open, which is the thing to catch.
 */
public class SerGuard {

    static void mustBeDenied(String label, Class<?> owner, String field) {
        try {
            Field f = owner.getDeclaredField(field);
            f.setAccessible(true);
            System.out.println("  ALLOWED  " + label + "   <-- encapsulation NOT enforced");
        } catch (java.lang.reflect.InaccessibleObjectException e) {
            System.out.println("  DENIED   " + label + "   (InaccessibleObjectException)");
        } catch (Throwable t) {
            System.out.println("  OTHER    " + label + " -> " + t.getClass().getName());
        }
    }

    public static void main(String[] args) {
        System.out.println("java.version = " + System.getProperty("java.version"));
        System.out.println();
        System.out.println("== application code asking for deep access directly ==");
        System.out.println("   (no ReflectionFactory on the stack; all must be DENIED)");
        mustBeDenied("java.util.ArrayList.elementData", java.util.ArrayList.class, "elementData");
        mustBeDenied("java.lang.String.value", String.class, "value");
        mustBeDenied("java.util.HashMap.table", java.util.HashMap.class, "table");
        System.out.println();

        // And the thing the fix is FOR must still work.
        System.out.println("== the serialization entry point (must succeed) ==");
        try {
            Object o = ReflectionFactory.getReflectionFactory()
                    .newConstructorForSerialization(java.util.ArrayList.class)
                    .newInstance();
            System.out.println("  OK       newConstructorForSerialization(ArrayList) -> "
                    + o.getClass().getName());
        } catch (Throwable t) {
            System.out.println("  FAILED   newConstructorForSerialization -> " + t);
        }
        System.out.println();
        System.out.println("DONE");
    }
}
