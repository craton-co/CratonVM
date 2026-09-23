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

    // ------------------------------------------------------------------
    // The OTHER half of the same slot's ambiguity, added 2026-09-22.
    //
    // `hasUsableImplementation` above is the shape where the stamp must NOT be
    // taken as a refusal. This is the shape where it must: a self-recursive
    // method with BOTH a covered and an uncovered self-call site, entered
    // through the UNCOVERED one, so every activation's stamp names a bci that
    // no `try` covers and the throw is owed to the caller.
    //
    // The 2026-08-20 remedy for the first shape — downgrade an "outside every
    // protected range" stamp to the pc-unknown search whenever it names a
    // self-call — broke this one, because that search matches a TYPED handler
    // on exception class ALONE, ignoring the handler's protected range. So
    // `catch (IllegalStateException)` over `[27,37)` swallowed a throw at bci
    // 23 and `recDeep(4, 4)` returned 1004. Measured at 19,930 of 20,000 calls
    // on `dev` 5a6d5073b, in all three JIT arms, 0 under `--nojit`.
    //
    // The two must be green together: fixing either one by itself is what the
    // last two rounds each did. See
    // `docs/internal/retired/r10-selfrec-deep-handler-leaks-once-per-million-20260922-RETIRED-20260922.md`.

    static long recDeep(int n, int floor) {
        if (n <= 0) throw new IllegalStateException("bottom");
        if (n <= floor) return recDeep(n - 1, floor);   // UNCOVERED self-call
        try {
            return recDeep(n - 1, floor) + n;           // COVERED self-call
        } catch (IllegalStateException e) {
            return 1000 + n;
        }
    }

    /**
     * 2 per call when `recDeep(4, 4)` throws as the bytecode says, 1 when the
     * handler swallows it. Called enough times to compile `recDeep`.
     *
     * Golden value 40000 == 2 * 20000. Before the fix this returns 20001-ish
     * (a wrong NUMBER, not a throw), which is why it is asserted rather than
     * merely run.
     */
    public static int selfRecursiveUncoveredEntryChecksum() {
        int sum = 0;
        for (int i = 0; i < 20000; i++) {
            try {
                recDeep(4, 4);
                sum += 1;          // swallowed: the defect
            } catch (IllegalStateException e) {
                sum += 2;          // correct
            }
        }
        return sum;
    }

    public static void main(String[] args) {
        System.out.println("selfRecursiveCalleeThrowChecksum="
                + selfRecursiveCalleeThrowChecksum());
        System.out.println("selfRecursiveUncoveredEntryChecksum="
                + selfRecursiveUncoveredEntryChecksum());
    }
}
