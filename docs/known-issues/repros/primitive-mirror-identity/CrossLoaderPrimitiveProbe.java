import java.io.InputStream;
import java.lang.reflect.Method;
import java.util.IdentityHashMap;
import java.util.Map;

/**
 * Is a primitive class mirror the same object when two different loaders each
 * resolve it?
 *
 * The AOT sweep saw Spring's {@code ClassUtils.resolvePrimitiveIfNecessary}
 * return null, which is only possible if its {@code IdentityHashMap<Class,Class>}
 * — keyed on the {@code X.class} literals resolved *inside ClassUtils* — misses
 * a mirror that {@code Method.getReturnType()} handed back. In a process where
 * two loaders define every name, the obvious candidate is a per-loader
 * primitive mirror. This checks that directly.
 */
public class CrossLoaderPrimitiveProbe {

    /** Loaded twice: once by the app loader, once by the child below. */
    public static class Probe {
        public static Class<?>[] primitives() {
            return new Class<?>[] {
                boolean.class, byte.class, char.class, double.class,
                float.class, int.class, long.class, short.class, void.class,
            };
        }

        public static Class<?> returnTypeOf(String name) throws Exception {
            return Probe.class.getMethod(name).getReturnType();
        }

        public static boolean flag() { return true; }
        public static int n() { return 0; }
        public static void v() {}
    }

    /** Defines its own copy of everything it can, parent-last. */
    static final class Isolating extends ClassLoader {
        private final ClassLoader source;

        Isolating(ClassLoader source) {
            super(null);
            this.source = source;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null && name.startsWith("CrossLoaderPrimitiveProbe")) {
                    String res = name.replace('.', '/') + ".class";
                    try (InputStream in = source.getResourceAsStream(res)) {
                        if (in != null) {
                            byte[] b = in.readAllBytes();
                            c = defineClass(name, b, 0, b.length);
                        }
                    } catch (Exception e) {
                        throw new ClassNotFoundException(name, e);
                    }
                }
                if (c == null) {
                    c = Class.forName(name, false, ClassLoader.getPlatformClassLoader());
                }
                if (resolve) {
                    resolveClass(c);
                }
                return c;
            }
        }
    }

    private static int failures = 0;

    private static void same(String what, Object a, Object b) {
        if (a != b) {
            System.out.println("  IDENTITY MISS " + what
                    + " app=" + a + "@" + System.identityHashCode(a)
                    + " child=" + b + "@" + System.identityHashCode(b));
            failures++;
        }
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 500;
        String[] names = {"boolean", "byte", "char", "double", "float", "int", "long", "short", "void"};

        for (int r = 0; r < rounds; r++) {
            Isolating child = new Isolating(CrossLoaderPrimitiveProbe.class.getClassLoader());
            Class<?> childProbe = child.loadClass("CrossLoaderPrimitiveProbe$Probe");
            if (childProbe.getClassLoader() == CrossLoaderPrimitiveProbe.class.getClassLoader()) {
                System.out.println("child loader did not get its own copy — probe is vacuous");
                System.exit(2);
            }

            Class<?>[] mine = Probe.primitives();
            Class<?>[] theirs = (Class<?>[]) childProbe.getMethod("primitives").invoke(null);
            for (int i = 0; i < mine.length; i++) {
                same("literal " + names[i] + " (round " + r + ")", mine[i], theirs[i]);
            }

            // getReturnType() from each loader's own copy of the same method.
            for (String m : new String[] {"flag", "n", "v"}) {
                Class<?> a = Probe.returnTypeOf(m);
                Class<?> b = (Class<?>) childProbe.getMethod("returnTypeOf", String.class).invoke(null, m);
                same("getReturnType " + m + " (round " + r + ")", a, b);
            }

            // The exact predicate Spring relies on.
            Map<Class<?>, Class<?>> map = new IdentityHashMap<>(9);
            for (Class<?> p : mine) {
                map.put(p, p);
            }
            for (Method m : childProbe.getMethods()) {
                Class<?> rt = m.getReturnType();
                if (rt.isPrimitive() && map.get(rt) == null) {
                    System.out.println("  MAP MISS " + childProbe.getName() + "." + m.getName()
                            + " -> " + rt + "@" + System.identityHashCode(rt)
                            + " absent from a map keyed on this loader's literals (round " + r + ")");
                    failures++;
                }
            }

            if ((r & 0x3F) == 0x3F) {
                System.gc();
            }
        }

        System.out.println(failures == 0
                ? "PROBE PASS (" + rounds + " loader pairs)"
                : "PROBE FAIL " + failures + " cross-loader primitive identity misses");
        if (failures != 0) {
            System.exit(1);
        }
    }
}
