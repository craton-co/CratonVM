// Interpreter round i1, wave 2, lane L2 — JVMS §5.4.3 resolution-error record.
//
// A class defined by a custom loader executes `ldc Opt.class`, `new Opt` and
// `instanceof Opt` twice each. The loader refuses the FIRST loadClass("..$Opt")
// and would define it on any later call. JVMS §5.4.3: the first failure
// (NoClassDefFoundError) is recorded per constant-pool entry, and every later
// execution of the same instruction throws the same error WITHOUT asking the
// loader again.
//
// SETUP (both VMs): after `javac`, rename `L2ResolutionErrorRecorded$Opt.class`
// to `L2ResolutionErrorRecorded$Opt.bin` in the output directory. The flaky
// loader reads the `.bin`; the application loader must NOT be able to find
// `Opt`, or CratonVM's global-resolution fallback after a declining user
// `loadClass` answers with the app loader's copy and the probe measures that
// instead (a separate divergence).
//
// Expected HotSpot 25 output (compare verbatim):
//   ldc#1: java.lang.NoClassDefFoundError: L2ResolutionErrorRecorded$Opt
//   ldc#2: java.lang.NoClassDefFoundError: L2ResolutionErrorRecorded$Opt
//   new#1: java.lang.NoClassDefFoundError: L2ResolutionErrorRecorded$Opt
//   new#2: java.lang.NoClassDefFoundError: L2ResolutionErrorRecorded$Opt
//   instanceof#1: java.lang.NoClassDefFoundError: L2ResolutionErrorRecorded$Opt
//   instanceof#2: java.lang.NoClassDefFoundError: L2ResolutionErrorRecorded$Opt
//   loadClass(Opt) calls=1
//
// (One loader call: javac emits ONE `CONSTANT_Class` per class name per class
// file, `ldc`, `new` and `instanceof` in `Holder` all name that one entry, and
// its first failure is what every later execution rethrows.)
//
// Before 2026-09-23 CratonVM printed `ldc#2: ok class L2ResolutionErrorRecorded$Opt`
// (the second attempt re-asked the loader, which then succeeded) and a higher
// call count. Run with CRATONVM_DISABLE_JIT=1 as well as the default.
import java.io.InputStream;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L2ResolutionErrorRecorded {
    public static class Opt {}

    public static class Holder {
        public static Object ldc() { return Opt.class; }
        public static Object alloc() { return new Opt(); }
        public static Object test(Object o) { return o instanceof Opt; }
    }

    static int optCalls;

    static final class FlakyLoader extends ClassLoader {
        FlakyLoader(ClassLoader parent) { super(parent); }

        private Class<?> defineFromParent(String name) throws ClassNotFoundException {
            String res = name.replace('.', '/')
                    + (name.endsWith("$Opt") ? ".bin" : ".class");
            try (InputStream in = getParent().getResourceAsStream(res)) {
                if (in == null) throw new ClassNotFoundException(name);
                byte[] b = in.readAllBytes();
                return defineClass(name, b, 0, b.length);
            } catch (java.io.IOException e) {
                throw new ClassNotFoundException(name, e);
            }
        }

        @Override
        protected synchronized Class<?> loadClass(String name, boolean resolve)
                throws ClassNotFoundException {
            Class<?> c = findLoadedClass(name);
            if (c != null) return c;
            if (name.equals("L2ResolutionErrorRecorded$Holder")) {
                return defineFromParent(name);
            }
            if (name.equals("L2ResolutionErrorRecorded$Opt")) {
                optCalls++;
                if (optCalls == 1) throw new ClassNotFoundException(name);
                return defineFromParent(name);
            }
            return super.loadClass(name, resolve);
        }
    }

    static void run(Method m, String label, Object... args) throws Exception {
        for (int i = 1; i <= 2; i++) {
            try {
                Object r = m.invoke(null, args);
                System.out.println(label + "#" + i + ": ok " + r);
            } catch (InvocationTargetException e) {
                Throwable t = e.getCause();
                System.out.println(label + "#" + i + ": " + t.getClass().getName() + ": " + t.getMessage());
            }
        }
    }

    public static void main(String[] a) throws Exception {
        FlakyLoader l = new FlakyLoader(L2ResolutionErrorRecorded.class.getClassLoader());
        Class<?> h = Class.forName("L2ResolutionErrorRecorded$Holder", true, l);
        run(h.getMethod("ldc"), "ldc");
        run(h.getMethod("alloc"), "new");
        run(h.getMethod("test", Object.class), "instanceof", "x");
        System.out.println("loadClass(Opt) calls=" + optCalls);
    }
}
