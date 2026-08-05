import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.Iterator;
import java.util.List;

/**
 * Reduces the two H2 divergences in
 * docs/known-issues/h2/h2-testindex-testmvstore-unmasked-20260802.md to
 * their VM-level questions. Every line is a strict HotSpot/CratonVM diff.
 */
public final class UnmaskProbe {

    // ── 1. TestIndex.testFunctionIndex ───────────────────────────────
    // The test counts a call only when Thread.currentThread().getStackTrace()
    // contains a frame whose getClassName() startsWith "org.h2.command.query.Select".

    static class Outer {
        static class Inner {
            static String[] walk() {
                List<String> names = new ArrayList<>();
                for (StackTraceElement e : Thread.currentThread().getStackTrace()) {
                    names.add(e.getClassName() + "#" + e.getMethodName());
                }
                return names.toArray(new String[0]);
            }
        }
    }

    static void stackTraceShape() {
        String[] frames = Outer.Inner.walk();
        System.out.println("STACK frames=" + frames.length);
        boolean sawDot = false;
        boolean sawSlash = false;
        boolean sawSelf = false;
        for (String f : frames) {
            if (f.indexOf('/') >= 0) {
                sawSlash = true;
            }
            if (f.startsWith("UnmaskProbe")) {
                sawSelf = true;
            }
            if (f.startsWith("UnmaskProbe$Outer$Inner")) {
                sawDot = true;
            }
        }
        System.out.println("STACK any-slash-in-classname=" + sawSlash);
        System.out.println("STACK has-nested-binary-name=" + sawDot);
        System.out.println("STACK has-caller-frame=" + sawSelf);
        for (String f : frames) {
            System.out.println("STACK  | " + f);
        }
    }

    /** The exact predicate the H2 test uses, against a caller two frames up. */
    static int counter = 0;

    static void countIfCallerVisible() {
        for (StackTraceElement e : Thread.currentThread().getStackTrace()) {
            if (e.getClassName().startsWith(UnmaskProbe.class.getName() + "$Driver")) {
                counter++;
                break;
            }
        }
    }

    static class Driver {
        static void run() {
            countIfCallerVisible();
        }
    }

    // ── 2. TestMVStore.testIterate ───────────────────────────────────
    // Method.invoke must wrap whatever the target throws in an
    // InvocationTargetException. The H2 case goes through the JDK's DEFAULT
    // method java.util.Iterator.remove(), whose body is
    // `throw new UnsupportedOperationException("remove")`.

    interface Thrower {
        void boom();

        default void defaultBoom() {
            throw new IllegalStateException("default");
        }
    }

    static class ThrowerImpl implements Thrower {
        @Override
        public void boom() {
            throw new IllegalStateException("virtual");
        }
    }

    /** An Iterator that inherits the JDK's default `remove()`. */
    static class NoRemove implements Iterator<Integer> {
        @Override
        public boolean hasNext() {
            return true;
        }

        @Override
        public Integer next() {
            return 1;
        }
    }

    static void reflectWrap(String label, Object target, Method m) {
        try {
            m.invoke(target);
            System.out.println("INVOKE " + label + " = NO-THROW (wrong)");
        } catch (InvocationTargetException e) {
            System.out.println("INVOKE " + label + " = ITE(" + e.getTargetException().getClass().getName() + ")");
        } catch (Throwable t) {
            System.out.println("INVOKE " + label + " = RAW(" + t.getClass().getName() + ") <-- unwrapped");
        }
    }

    public static void main(String[] args) throws Exception {
        stackTraceShape();
        Driver.run();
        System.out.println("STACK caller-predicate-counter=" + counter + " (expect 1)");

        reflectWrap("declared-virtual", new ThrowerImpl(), Thrower.class.getMethod("boom"));
        reflectWrap("interface-default", new ThrowerImpl(), Thrower.class.getMethod("defaultBoom"));
        reflectWrap("jdk-default-Iterator.remove", new NoRemove(), Iterator.class.getMethod("remove"));
        reflectWrap("jdk-default-on-impl-class", new NoRemove(), NoRemove.class.getMethod("remove"));

        // Same again through the reflective path H2 actually uses: a Proxy whose
        // handler catches InvocationTargetException.
        Iterator<?> it = (Iterator<?>) java.lang.reflect.Proxy.newProxyInstance(
                UnmaskProbe.class.getClassLoader(),
                new Class<?>[] { Iterator.class },
                (proxy, method, a) -> {
                    Object real = new NoRemove();
                    try {
                        return method.invoke(real, a);
                    } catch (InvocationTargetException e) {
                        System.out.println("PROXY caught ITE("
                                + e.getTargetException().getClass().getName() + ")");
                        return null;
                    }
                });
        try {
            it.remove();
            System.out.println("PROXY remove() returned normally");
        } catch (Throwable t) {
            System.out.println("PROXY remove() escaped with " + t.getClass().getName()
                    + ": " + t.getMessage() + " <-- unwrapped");
        }
    }
}
