import java.lang.reflect.Method;
import java.lang.reflect.Modifier;

/**
 * The shape of `org.codehaus.groovy.reflection.stdclasses.CachedSAMClass
 * .hasUsableImplementation` — a static SELF-RECURSIVE method whose recursive
 * call is in tail position, wrapped around a `try`/`catch` whose handler stores
 * the caught exception into a local slot that holds an `int` on the normal
 * path.
 *
 * Groovy walks a class's superclass chain with it to decide whether an abstract
 * method already has a usable implementation; the answer decides which method
 * `getSAMMethod` reports, which decides what a Closure coerces to. A wrong
 * boolean here surfaces far away as a NULL receiver at a Groovy call site, and
 * then as
 *
 *   WrongMethodTypeException: cannot explicitly cast
 *     MethodHandle(Object,Object,String,Object[])Object to (Object,Object)Object
 *
 * out of `Selector.setCallSiteTarget` — i.e. `GroovyMarkupViewTests` and
 * `ViewResolutionIntegrationTests` in the Spring Framework suite.
 */
public class SelfRecCatchProbe {

    // ---- a hierarchy with abstract methods, overrides, and gaps -----------
    public interface Visitor { void visit(String s); Object other(int i); }
    public static abstract class A implements Visitor { }
    public static abstract class B extends A { public void visit(String s) { } }
    public static abstract class C extends B { }
    public static class D extends C { public Object other(int i) { return null; } }

    public interface OnlyAbstract { void run(String s); }
    public static abstract class E implements OnlyAbstract { }
    public static abstract class F extends E { }

    static final int ABSTRACT_STATIC_PRIVATE = Modifier.ABSTRACT | Modifier.STATIC | Modifier.PRIVATE;
    static final int PUBLIC_OR_PROTECTED = Modifier.PUBLIC | Modifier.PROTECTED;

    /** Transcribed from Groovy's body, including the catch and the tail self-call. */
    private static boolean hasUsableImplementation(Class<?> c, Method m) {
        if (c == m.getDeclaringClass()) return false;
        try {
            Method found = c.getMethod(m.getName(), m.getParameterTypes());
            int modifiers = found.getModifiers();
            int asp = modifiers & ABSTRACT_STATIC_PRIVATE;
            int visible = modifiers & PUBLIC_OR_PROTECTED;
            if (visible != 0 && asp == 0) return true;
        } catch (NoSuchMethodException e) {
            // The handler stores `e` into the slot `modifiers` used above.
        }
        if (c == Object.class) return false;
        return hasUsableImplementation(c.getSuperclass(), m);
    }

    static String answersFor(Class<?> c) throws Exception {
        // Sorted: `getDeclaredMethods()` order is unspecified and the two VMs
        // legitimately differ on it, which would make this file's oracle
        // compare unequal for a reason that is not the defect.
        java.util.List<String> rows = new java.util.ArrayList<>();
        Class<?>[] ifaces = c.getInterfaces().length > 0
                ? c.getInterfaces()
                : new Class<?>[]{Visitor.class};
        for (Class<?> i : ifaces) {
            for (Method m : i.getDeclaredMethods()) {
                rows.add(m.getName() + "=" + hasUsableImplementation(c, m));
            }
        }
        java.util.Collections.sort(rows);
        return String.join(" ", rows);
    }

    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 60_000;
        Class<?>[] targets = { A.class, B.class, C.class, D.class, E.class, F.class };

        // Warm: the defect needs the method JIT-compiled.
        for (int i = 0; i < iterations; i++) {
            for (Class<?> t : targets) answersFor(t);
        }

        // Report: one line per class, stable and order-independent.
        for (Class<?> t : targets) {
            System.out.println(t.getSimpleName() + " -> " + answersFor(t));
        }

        // A second, arity-free summary so a single flipped boolean is obvious.
        int trues = 0;
        for (Class<?> t : targets) {
            for (String kv : answersFor(t).split(" ")) if (kv.endsWith("=true")) trues++;
        }
        System.out.println("TRUECOUNT=" + trues);
    }
}
