/*
 * Interpreter round i1, wave 6, lane L5: a catch type that cannot be loaded
 * fails its class at LINK time (HotSpot's verifier loads every catch type to
 * check it is a Throwable), not at the first throw that reaches the handler.
 *
 * SETUP: run CratonVM with --jdk-only. Under --compatible CratonVM keeps the
 * lazy behaviour on purpose (the handler search raises the error when an
 * exception first reaches the row), so line 2 differs there by design,
 * expected: "run -> threw java.lang.NoClassDefFoundError: CatchTypeLinkProbe$Mxssing".
 * No files to delete: a child loader defines `Holder` from the probe's own
 * class file with the catch type's name rewritten (same length) to a class
 * that exists nowhere.
 *
 * HotSpot 25 prints exactly (and CratonVM --jdk-only must, with and without
 * --nojit):
 *
 *   loaded CatchTypeLinkProbe$Holder by child=true
 *   link failed java.lang.NoClassDefFoundError: CatchTypeLinkProbe$Mxssing
 *   second init: java.lang.NoClassDefFoundError: CatchTypeLinkProbe$Mxssing
 *   control -> caught ISE
 *   superclass initializers run: 0
 *
 * The third line is the wave-7 fix: a LINK failure leaves the class unlinked
 * (not erroneous), so HotSpot links again and repeats the same error; CratonVM
 * used to answer "Could not initialize class CatchTypeLinkProbe$Holder"
 * (docs/internal/fixed-bugs/
 * interpreter-L5-link-failure-parks-class-in-initialization-error-FIXED-20260924.md).
 * The last line: HotSpot links Holder BEFORE running its superclass's
 * initializer, so a link failure leaves Noisy uninitialized (CratonVM used to
 * run it first and print 1).
 * Under --compatible lines 2, 3 and 5 differ by design (lazy catch-type check:
 * Holder links, so "second init: linked" and 1).
 */
public class CatchTypeLinkProbe {
    public static class Missing extends RuntimeException {}

    /** Holder's superclass: HotSpot links Holder before running this. */
    public static class Noisy {
        static {
            superInits++;
        }
    }

    static int superInits;

    public static class Holder extends Noisy {
        public static String run() {
            try {
                throw new IllegalStateException("x");
            } catch (Missing m) {
                return "caught Missing";
            } catch (IllegalStateException e) {
                return "caught ISE";
            }
        }
    }

    /** Same shape, but every catch type loads. */
    public static class Control {
        public static String run() {
            try {
                throw new IllegalStateException("x");
            } catch (UnsupportedOperationException m) {
                return "caught UOE";
            } catch (IllegalStateException e) {
                return "caught ISE";
            }
        }
    }

    static final String FROM = "CatchTypeLinkProbe$Missing";
    static final String TO = "CatchTypeLinkProbe$Mxssing";

    /** Replace every occurrence of `from` by the same-length `to`. */
    static byte[] rename(byte[] b, String from, String to) {
        byte[] f = from.getBytes(java.nio.charset.StandardCharsets.UTF_8);
        byte[] t = to.getBytes(java.nio.charset.StandardCharsets.UTF_8);
        byte[] out = b.clone();
        outer:
        for (int i = 0; i + f.length <= out.length; i++) {
            for (int j = 0; j < f.length; j++) {
                if (out[i + j] != f[j]) {
                    continue outer;
                }
            }
            System.arraycopy(t, 0, out, i, t.length);
        }
        return out;
    }

    /** Child-first for Holder (renamed catch type) and Control; parent for the rest. */
    static final class ChildLoader extends ClassLoader {
        ChildLoader(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                if (name.equals("CatchTypeLinkProbe$Holder") || name.equals("CatchTypeLinkProbe$Control")) {
                    Class<?> c = findLoadedClass(name);
                    if (c == null) {
                        byte[] b;
                        try (java.io.InputStream in =
                                CatchTypeLinkProbe.class.getResourceAsStream(name + ".class")) {
                            if (in == null) {
                                throw new ClassNotFoundException(name + " (no class file)");
                            }
                            b = rename(in.readAllBytes(), FROM, TO);
                        } catch (java.io.IOException e) {
                            throw new ClassNotFoundException(name, e);
                        }
                        c = defineClass(name, b, 0, b.length);
                    }
                    return c;
                }
                return super.loadClass(name, resolve);
            }
        }
    }

    public static void main(String[] args) throws Exception {
        ClassLoader child = new ChildLoader(CatchTypeLinkProbe.class.getClassLoader());
        Class<?> h = Class.forName("CatchTypeLinkProbe$Holder", false, child);
        System.out.println("loaded " + h.getName() + " by child=" + (h.getClassLoader() == child));
        try {
            Class.forName("CatchTypeLinkProbe$Holder", true, child);
            Object r = h.getMethod("run").invoke(null);
            System.out.println("run -> " + r);
        } catch (java.lang.reflect.InvocationTargetException e) {
            Throwable t = e.getCause();
            System.out.println("run -> threw " + t.getClass().getName() + ": " + t.getMessage());
        } catch (LinkageError t) {
            System.out.println("link failed " + t.getClass().getName() + ": " + t.getMessage());
        }
        try {
            Class.forName("CatchTypeLinkProbe$Holder", true, child);
            System.out.println("second init: linked");
        } catch (LinkageError t) {
            System.out.println("second init: " + t.getClass().getName() + ": " + t.getMessage());
        }
        Class<?> c = Class.forName("CatchTypeLinkProbe$Control", true, child);
        System.out.println("control -> " + c.getMethod("run").invoke(null));
        System.out.println("superclass initializers run: " + superInits);
    }
}
