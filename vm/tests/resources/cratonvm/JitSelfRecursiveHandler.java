package cratonvm;

import java.lang.reflect.Method;
import java.lang.reflect.Modifier;

/**
 * RBC.6 sibling regression: a callee-thrown exception must be caught by the
 * activation whose own try covers the CALL SITE, even when that activation is
 * one of several activations of the SAME method in a compiled chain.
 *
 * A compiled frame never dispatches to its own handler — the interpreter's
 * post-return drain does, using the bci `jit_set_throw_bci` stamped. There is
 * one such slot per thread and it carries no activation identity, so in a
 * SELF-RECURSIVE chain the outermost frame's stamp (its own recursive call
 * site) overwrites the inner frame's (the real throw site). The drain then
 * range-checks the outermost call site against the method's exception table,
 * finds it outside every protected range, and propagates — past a `catch` that
 * covers the throw.
 *
 * The shape below is `org.codehaus.groovy.reflection.stdclasses.CachedSAMClass
 * .hasUsableImplementation`, which walks a superclass chain with a tail
 * self-call and wraps `Class.getMethod` in `catch (NoSuchMethodException)`.
 * Groovy uses its answer to pick the SAM method a Closure coerces to, so the
 * escaped exception surfaced far away as a null Groovy receiver and finally a
 * `WrongMethodTypeException` out of `Selector.setCallSiteTarget` —
 * `GroovyMarkupViewTests` and `ViewResolutionIntegrationTests` in the Spring
 * Framework suite.
 *
 * The reflective callee is not incidental: a hand-written `throw` in a
 * same-package callee does NOT reproduce (measured — `jit_throw_exception`
 * stamps its own bci and takes the already-correct path). It needs an
 * exception raised inside a dispatched callee, so the stamp is the caller's
 * CALL SITE.
 *
 * Golden value computed by running this fixture under a real JDK.
 */
public class JitSelfRecursiveHandler {

    public interface Visitor { void visit(String s); Object other(int i); }
    public static abstract class A implements Visitor { }
    public static abstract class B extends A { public void visit(String s) { } }
    public static abstract class C extends B { }
    public static class D extends C { public Object other(int i) { return null; } }

    public interface OnlyAbstract { void run(String s); }
    public static abstract class E implements OnlyAbstract { }
    public static abstract class F extends E { }

    private static final int ABSTRACT_STATIC_PRIVATE =
            Modifier.ABSTRACT | Modifier.STATIC | Modifier.PRIVATE;
    private static final int PUBLIC_OR_PROTECTED = Modifier.PUBLIC | Modifier.PROTECTED;

    /** Transcribed from Groovy's body: catch around a reflective callee, tail self-call. */
    private static boolean hasUsableImplementation(Class<?> c, Method m) {
        if (c == m.getDeclaringClass()) return false;
        try {
            Method found = c.getMethod(m.getName(), m.getParameterTypes());
            int modifiers = found.getModifiers();
            int asp = modifiers & ABSTRACT_STATIC_PRIVATE;
            int visible = modifiers & PUBLIC_OR_PROTECTED;
            if (visible != 0 && asp == 0) return true;
        } catch (NoSuchMethodException e) {
            // Fall through to the tail self-call below.
        }
        if (c == Object.class) return false;
        return hasUsableImplementation(c.getSuperclass(), m);
    }

    private static int scoreOf(Class<?> c) {
        int score = 0;
        Class<?>[] ifaces = c.getInterfaces().length > 0
                ? c.getInterfaces()
                : new Class<?>[]{Visitor.class};
        for (Class<?> i : ifaces) {
            for (Method m : i.getDeclaredMethods()) {
                // Name-keyed, so the unspecified `getDeclaredMethods()` order
                // cannot move the checksum.
                if (hasUsableImplementation(c, m)) score += m.getName().length();
            }
        }
        return score;
    }

    /**
     * Called enough times to JIT-compile `hasUsableImplementation`; before the
     * fix this threw `NoSuchMethodException` out of a `catch` that covers it.
     */
    public static int selfRecursiveCalleeThrowChecksum() {
        Class<?>[] targets = { A.class, B.class, C.class, D.class, E.class, F.class };
        int sum = 0;
        for (int i = 0; i < 4000; i++) {
            for (Class<?> t : targets) {
                sum += scoreOf(t);
                sum %= 1_000_003;
            }
        }
        return sum;
    }

    public static void main(String[] args) {
        System.out.println("selfRecursiveCalleeThrowChecksum="
                + selfRecursiveCalleeThrowChecksum());
    }
}
