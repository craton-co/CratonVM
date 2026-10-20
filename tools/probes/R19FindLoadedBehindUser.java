import java.io.InputStream;
import java.lang.reflect.Method;
import java.util.Set;

// r18-find-loaded-class-misses-builtin-copy-behind-user-copy: a user loader that
// defines an application class FIRST must not change what the built-in loaders
// report for that name afterwards. Each scenario uses its own class names so
// one scenario's loads cannot mask another's. HotSpot 25 output is the reference.
// Run with --add-opens java.base/java.lang=ALL-UNNAMED (findLoadedClass).
// Usage: R19FindLoadedBehindUser [scenario...]   (default: all)
public class R19FindLoadedBehindUser {
    static final class ChildFirst extends ClassLoader {
        private final Set<String> own;
        ChildFirst(ClassLoader parent, Set<String> own) { super(parent); this.own = own; }
        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!own.contains(name)) return super.loadClass(name, resolve);
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    try (InputStream in = R19FindLoadedBehindUser.class.getClassLoader().getResourceAsStream(name.replace('.', '/') + ".class")) {
                        byte[] b = in.readAllBytes();
                        c = defineClass(name, b, 0, b.length);
                    } catch (Exception e) {
                        throw new ClassNotFoundException(name, e);
                    }
                }
                return c;
            }
        }
    }
    static final class Sub extends ClassLoader {
        Sub(ClassLoader parent) { super(parent); }
    }

    static Method FLC;
    static Object flc(ClassLoader l, String n) throws Exception {
        Class<?> c = (Class<?>) FLC.invoke(l, n);
        return c == null ? "null" : c.getClassLoader() == null ? "boot" : c.getClassLoader().getClass().getSimpleName();
    }
    static String who(Class<?> c) {
        return c == null ? "null" : c.getClassLoader() == null ? "boot" : c.getClassLoader().getClass().getSimpleName();
    }

    public static void main(String[] a) throws Throwable {
        FLC = ClassLoader.class.getDeclaredMethod("findLoadedClass", String.class);
        FLC.setAccessible(true);
        ClassLoader app = R19FindLoadedBehindUser.class.getClassLoader();
        ClassLoader plat = ClassLoader.getPlatformClassLoader();
        Set<String> want = a.length == 0 ? null : Set.of(a);

        if (want == null || want.contains("forname")) {
            String n = "R19FlbuA";
            ClassLoader u = new ChildFirst(app, Set.of(n));
            Class<?> uc = Class.forName(n, false, u);
            System.out.println("forname user=" + who(uc) + " app-before=" + flc(app, n) + " plat-before=" + flc(plat, n));
            Class<?> ac = Class.forName(n, false, app);
            System.out.println("forname app=" + who(ac) + " distinct=" + (ac != uc) + " app-after=" + flc(app, n)
                + " same=" + (FLC.invoke(app, n) == ac) + " plat-after=" + flc(plat, n) + " user-after=" + flc(u, n));
            Class<?> again = Class.forName(n, true, app);
            System.out.println("forname again-same=" + (again == ac) + " sub-same=" + (Class.forName(n, false, new Sub(app)) == ac)
                + " user-still=" + (Class.forName(n, false, u) == uc));
        }
        if (want == null || want.contains("cpref")) {
            // app CODE resolves the name (constant pool) after the user copy exists
            String n = "R19FlbuB";
            ClassLoader u = new ChildFirst(app, Set.of(n));
            Class<?> uc = Class.forName(n, false, u);
            Class<?> viaCode = R19FlbuRefs.b();
            System.out.println("cpref code=" + who(viaCode) + " distinct=" + (viaCode != uc) + " app-flc-same=" + (FLC.invoke(app, n) == viaCode)
                + " forname-same=" + (Class.forName(n, false, app) == viaCode) + " new=" + who(R19FlbuRefs.newB().getClass())
                + " newsame=" + (R19FlbuRefs.newB().getClass() == viaCode));
        }
        if (want == null || want.contains("array")) {
            String n = "R19FlbuC";
            ClassLoader u = new ChildFirst(app, Set.of(n));
            Class<?> uc = Class.forName(n, false, u);
            Class<?> arr = R19FlbuRefs.cArray();
            Class<?> comp = arr.getComponentType();
            System.out.println("array comp=" + who(comp) + " distinct=" + (comp != uc) + " forname-same=" + (Class.forName(n, false, app) == comp)
                + " arrname-same=" + (Class.forName("[L" + n + ";", false, app) == arr) + " userarr-distinct=" + (Class.forName("[L" + n + ";", false, u) != arr));
        }
        if (want == null || want.contains("appfirst")) {
            String n = "R19FlbuD";
            Class<?> ac = Class.forName(n, false, app);
            ClassLoader u = new ChildFirst(app, Set.of(n));
            Class<?> uc = Class.forName(n, false, u);
            System.out.println("appfirst distinct=" + (ac != uc) + " user=" + who(uc) + " app-flc-same=" + (FLC.invoke(app, n) == ac)
                + " forname-same=" + (Class.forName(n, false, app) == ac) + " code-same=" + (R19FlbuRefs.d() == ac));
        }
        if (want == null || want.contains("twousers")) {
            String n = "R19FlbuE";
            ClassLoader u1 = new ChildFirst(app, Set.of(n));
            ClassLoader u2 = new ChildFirst(app, Set.of(n));
            Class<?> c1 = Class.forName(n, false, u1);
            Class<?> c2 = Class.forName(n, false, u2);
            System.out.println("twousers distinct=" + (c1 != c2) + " app-before=" + flc(app, n));
            Class<?> ac = Class.forName(n, false, app);
            System.out.println("twousers app=" + who(ac) + " distinct3=" + (ac != c1 && ac != c2) + " app-after-same=" + (FLC.invoke(app, n) == ac)
                + " u1-same=" + (FLC.invoke(u1, n) == c1) + " u2-same=" + (FLC.invoke(u2, n) == c2));
        }
        if (want == null || want.contains("nested")) {
            String n = "R19FlbuOuter$In";
            ClassLoader u = new ChildFirst(app, Set.of(n));
            Class<?> uc = Class.forName(n, false, u);
            Class<?> ac = Class.forName(n, false, app);
            System.out.println("nested distinct=" + (ac != uc) + " decl=" + who(ac.getDeclaringClass()) + " app-flc-same=" + (FLC.invoke(app, n) == ac)
                + " code-same=" + (R19FlbuOuter.in() == ac));
        }
    }
}

class R19FlbuA { }
class R19FlbuB { }
class R19FlbuC { }
class R19FlbuD { }
class R19FlbuE { }
class R19FlbuOuter { static class In { } static Class<?> in() { return In.class; } }
class R19FlbuRefs {
    static Class<?> b() { return R19FlbuB.class; }
    static Object newB() { return new R19FlbuB(); }
    static Class<?> cArray() { return R19FlbuC[].class; }
    static Class<?> d() { return R19FlbuD.class; }
}
