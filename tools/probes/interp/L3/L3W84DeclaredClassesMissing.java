/*
 * Interpreter round i4, wave 84, lane L3: Class.getDeclaredClasses() of a
 * class whose member class its own loader cannot load.
 *
 * HotSpot's JVM_GetDeclaredClasses resolves every InnerClasses member entry
 * with `cp->klass_at(ioff, CHECK_NULL)`, so the loader's ClassNotFoundException
 * becomes the resolution's NoClassDefFoundError (internal class name, the CNFE
 * chained as its cause) and is thrown out of getDeclaredClasses(). CratonVM
 * dropped the entry and answered the remaining members (and could answer the
 * application loader's copy of a member the isolating loader refused).
 *
 * Run (both modes must print the HotSpot lines):
 *   javac -d out L3W84DeclaredClassesMissing.java
 *   cratonvm --jdk-only   -cp out L3W84DeclaredClassesMissing
 *   cratonvm --compatible -cp out L3W84DeclaredClassesMissing
 *   java                  -cp out L3W84DeclaredClassesMissing   (HotSpot 25)
 *
 * Expected HotSpot 25 output:
 *   app: [L3W84DeclaredClassesMissing$Full$A, L3W84DeclaredClassesMissing$Full$B]
 *   isolated: [L3W84DeclaredClassesMissing$Full$A, L3W84DeclaredClassesMissing$Full$B] same-loader=true
 *   missing: java.lang.NoClassDefFoundError: L3W84DeclaredClassesMissing$Part$Gone
 *   cause: java.lang.ClassNotFoundException: refused L3W84DeclaredClassesMissing$Part$Gone
 *   app-part: [L3W84DeclaredClassesMissing$Part$Gone, L3W84DeclaredClassesMissing$Part$Kept]
 */
import java.io.IOException;
import java.io.InputStream;
import java.util.Arrays;

public class L3W84DeclaredClassesMissing {
    public static class Full {
        public static class A {}
        public static class B {}
    }

    public static class Part {
        public static class Kept {}
        public static class Gone {}
    }

    /** Defines this probe's nested classes itself; refuses every `$Gone`. */
    static final class Isolating extends ClassLoader {
        Isolating(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                if (name.endsWith("$Gone")) {
                    throw new ClassNotFoundException("refused " + name);
                }
                if (name.startsWith("L3W84DeclaredClassesMissing$")) {
                    String res = name.replace('.', '/') + ".class";
                    try (InputStream in = getParent().getResourceAsStream(res)) {
                        byte[] b = in.readAllBytes();
                        return defineClass(name, b, 0, b.length);
                    } catch (IOException e) {
                        throw new ClassNotFoundException(name, e);
                    }
                }
                return super.loadClass(name, resolve);
            }
        }
    }

    static String names(Class<?>[] cs) {
        String[] n = new String[cs.length];
        for (int i = 0; i < cs.length; i++) {
            n[i] = cs[i].getName();
        }
        Arrays.sort(n);
        return Arrays.toString(n);
    }

    public static void main(String[] args) throws Exception {
        System.out.println("app: " + names(Full.class.getDeclaredClasses()));

        Isolating loader = new Isolating(L3W84DeclaredClassesMissing.class.getClassLoader());
        Class<?> full = loader.loadClass("L3W84DeclaredClassesMissing$Full");
        Class<?>[] members = full.getDeclaredClasses();
        boolean same = true;
        for (Class<?> m : members) {
            same &= m.getClassLoader() == loader;
        }
        System.out.println("isolated: " + names(members) + " same-loader=" + same);

        Class<?> part = loader.loadClass("L3W84DeclaredClassesMissing$Part");
        try {
            System.out.println("missing: answered " + names(part.getDeclaredClasses()));
        } catch (Throwable t) {
            System.out.println("missing: " + t);
            System.out.println("cause: " + t.getCause());
        }

        System.out.println("app-part: " + names(Part.class.getDeclaredClasses()));
    }
}
