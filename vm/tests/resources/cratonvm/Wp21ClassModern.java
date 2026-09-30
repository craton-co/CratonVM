// JAVA21+
package cratonvm;

import java.lang.reflect.AnnotatedType;
import java.lang.reflect.Method;
import java.lang.reflect.RecordComponent;

/**
 * WP2.1-class-modern — exercises the modern {@code java.lang.Class} API
 * surface that ByteBuddy ({@code TypeDescription.forLoadedType}) and
 * Hibernate's record/sealed-class scanners depend on:
 *
 * <ul>
 *   <li>{@code getEnclosingClass()} — inner-class -> outer-class.</li>
 *   <li>{@code getNestHost()} — nested class -> nest-host.</li>
 *   <li>{@code getPermittedSubclasses()} — sealed class -> permitted set.</li>
 *   <li>{@code getRecordComponents()} — record class -> component array.</li>
 *   <li>{@code isSealed()} — sealed/regular boolean.</li>
 *   <li>{@code getDeclaredMethod(String, Class&lt;?&gt;...)} — singular accessor.</li>
 *   <li>{@code getAnnotatedSuperclass() / getAnnotatedInterfaces()} — type
 *       annotation surface (best-effort: the array is non-null even when
 *       no RUNTIME annotations are present).</li>
 * </ul>
 *
 * <p>Each probe returns an int the Rust harness asserts on:
 *   {@code 1} pass, {@code 0} fail.
 *
 * <p>Following the convention of the sibling {@code Wp21ClassReflectE2E}
 * fixture, every probe is a {@code public static int methodName()} so the
 * Rust harness can drive each one as a self-contained {@code ()I} invoke.
 */
public class Wp21ClassModern {

    // ------------------------------------------------------------------
    // Sealed-class declaration set — exercises isSealed() + getPermittedSubclasses().
    // ------------------------------------------------------------------

    public sealed interface Shape permits Circle, Square, Triangle {}
    public static final class Circle implements Shape {}
    public static final class Square implements Shape {}
    public static final class Triangle implements Shape {}

    // Regular (non-sealed) interface for the negative isSealed() probe.
    public interface PlainInterface {}

    // ------------------------------------------------------------------
    // Record declaration — exercises getRecordComponents().
    // ------------------------------------------------------------------

    public record Point(int x, int y) {}

    // ------------------------------------------------------------------
    // Inner / nested class — exercises getEnclosingClass() + getNestHost().
    // ------------------------------------------------------------------

    public static class Inner {
        public static class Deeper {
            public int answer() { return 42; }
        }
    }

    // ------------------------------------------------------------------
    // Method probe target — exercises getDeclaredMethod(String, Class[]).
    // ------------------------------------------------------------------

    public static String foo(String s) { return s; }

    // ==================================================================
    // Probes
    // ==================================================================

    /** {@code Class.getEnclosingClass()} on an inner class returns the outer. */
    public static int enclosingClassProbe() {
        try {
            Class<?> outer = Inner.class.getEnclosingClass();
            if (outer == Wp21ClassModern.class) return 1;
            // Failure-mode signals — surface what we got.
            if (outer == null) return -1;
            if (outer == Inner.class) return -2;
            return -3;
        } catch (Throwable t) {
            return -4;
        }
    }

    /**
     * {@code Class.getNestHost()} on a nested class returns the outermost
     * lexical enclosing class (the nest host). For {@code Inner.Deeper},
     * the nest host is {@code Wp21ClassModern}.
     */
    public static int nestHostProbe() {
        try {
            Class<?> host = Inner.Deeper.class.getNestHost();
            return host == Wp21ClassModern.class ? 1 : 0;
        } catch (Throwable t) {
            return 0;
        }
    }

    /**
     * {@code Class.getPermittedSubclasses()} on a sealed interface returns
     * the permitted-subclass array; the length must match the {@code permits}
     * clause (3: Circle, Square, Triangle).
     */
    public static int permittedSubclassesProbe() {
        try {
            Class<?>[] subs = Shape.class.getPermittedSubclasses();
            if (subs == null) return 0;
            return subs.length == 3 ? 1 : 0;
        } catch (Throwable t) {
            return 0;
        }
    }

    /**
     * {@code Class.getRecordComponents()} on a record returns the component
     * array; for {@code Point(int x, int y)} the length is 2.
     */
    public static int recordComponentsProbe() {
        try {
            RecordComponent[] comps = Point.class.getRecordComponents();
            if (comps == null) return 0;
            return comps.length == 2 ? 1 : 0;
        } catch (Throwable t) {
            return 0;
        }
    }

    /**
     * {@code Class.isSealed()} returns {@code true} on the sealed
     * {@code Shape} interface and {@code false} on a plain interface.
     */
    public static int isSealedProbe() {
        try {
            boolean shapeSealed = Shape.class.isSealed();
            boolean plainSealed = PlainInterface.class.isSealed();
            return (shapeSealed && !plainSealed) ? 1 : 0;
        } catch (Throwable t) {
            return 0;
        }
    }

    /**
     * {@code Class.getDeclaredMethod(String, Class&lt;?&gt;...)} — singular
     * accessor — must return a non-null Method whose
     * {@code getName().equals("foo")} for the {@code foo(String)} target.
     */
    public static int getDeclaredMethodProbe() {
        try {
            Method m = Wp21ClassModern.class.getDeclaredMethod("foo", String.class);
            if (m == null) return 0;
            return "foo".equals(m.getName()) ? 1 : 0;
        } catch (Throwable t) {
            return 0;
        }
    }

    /**
     * {@code Class.getAnnotatedInterfaces()} must return a non-null array.
     * Best-effort: the array may be empty if RUNTIME type-annotations
     * aren't fully wired (the WP2.7 annotation-proxy work). The non-null
     * guarantee alone is what frameworks (ByteBuddy, JMX OpenMBean
     * introspector) need for {@code <clinit>} to succeed.
     */
    public static int getTypeAnnotationsProbe() {
        try {
            // Probe a class with at least one declared interface so we
            // exercise the array-allocation path (Inner has none, but Shape
            // is a sealed interface with no super-interfaces, so use
            // Inner.Deeper's outer or pick a class with interfaces:
            // String implements Serializable, Comparable, CharSequence — 3+ ifaces).
            AnnotatedType[] ats = String.class.getAnnotatedInterfaces();
            if (ats == null) return 0;
            // Length must match getInterfaces() length.
            int expected = String.class.getInterfaces().length;
            if (ats.length != expected) return 0;
            // Each AnnotatedType must be non-null.
            for (AnnotatedType at : ats) {
                if (at == null) return 0;
            }
            // Also probe getAnnotatedSuperclass — must be non-null for
            // String (its superclass is Object, which is non-null).
            AnnotatedType sup = String.class.getAnnotatedSuperclass();
            if (sup == null) return 0;
            return 1;
        } catch (Throwable t) {
            return 0;
        }
    }
}
