// j27 wave 2 (lane REDEFINE): a class defined by a fresh ClassLoader
// (ClassLoader.defineClass -> the VM's define_class_full) after a call site
// was warmed with one receiver class must run its own override, at a cold
// call and in a loop that is already compiled; and (performance half) the
// warmed bodies are withdrawn so they recompile with the new receiver.
// Page: docs/feature-designs/class-resolution-and-caching.md
//
// Every nested class the child loader defines is named by STRING only, so the
// application loader never loads it. All members are public: the child
// loader's classes sit in another runtime package (JVMS 5.4.5).
//
// Expected HotSpot 25 output (facts only):
//   fact base-before 200000
//   fact sub-loader-distinct true
//   fact sub-after 400000
//   fact mixed-after 300000
//   fact default-before 2000000
//   fact default-after 4000000
//   fact abstract-before 600000
//   fact abstract-after 800000
//   fact deep-after 1000000
//   fact mid-loop 800000
// -Dj27.timing=true adds `time sub-after-ns-per-op <n>` (not compared).
import java.io.IOException;
import java.io.InputStream;

public class J27ChaFreshLoader {
    public static class Base {
        public int v() {
            return 1;
        }
    }

    public static class Sub extends Base {
        @Override
        public int v() {
            return 2;
        }
    }

    public static class Mid extends Base {}

    public static class Deep extends Mid {
        @Override
        public int v() {
            return 5;
        }
    }

    public static class Sub2 extends Base {
        @Override
        public int v() {
            return 7;
        }
    }

    public interface Dflt {
        default int k() {
            return 10;
        }
    }

    public static class DfltA implements Dflt {}

    public static class DfltB implements Dflt {
        @Override
        public int k() {
            return 20;
        }
    }

    public abstract static class Abs {
        public abstract int f();
    }

    public static class One extends Abs {
        @Override
        public int f() {
            return 3;
        }
    }

    public static class Two extends Abs {
        @Override
        public int f() {
            return 4;
        }
    }

    /** Defines exactly one class itself, from the parent's class file. */
    static final class Child extends ClassLoader {
        private final String only;

        Child(ClassLoader parent, String only) {
            super(parent);
            this.only = only;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!name.equals(only)) {
                return super.loadClass(name, resolve);
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                String path = name.replace('.', '/') + ".class";
                try (InputStream in = getParent().getResourceAsStream(path)) {
                    if (in == null) {
                        throw new ClassNotFoundException(name);
                    }
                    byte[] b = in.readAllBytes();
                    return defineClass(name, b, 0, b.length);
                } catch (IOException e) {
                    throw new ClassNotFoundException(name, e);
                }
            }
        }
    }

    static final int N = 200_000;

    static Object fresh(String simpleName) throws Exception {
        String name = J27ChaFreshLoader.class.getName() + "$" + simpleName;
        ClassLoader parent = J27ChaFreshLoader.class.getClassLoader();
        Class<?> c = Class.forName(name, true, new Child(parent, name));
        return c.getDeclaredConstructor().newInstance();
    }

    static long sumV(Base b, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += b.v();
        }
        return s;
    }

    static long sumMixed(Base a, Base b, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += ((i & 1) == 0 ? a : b).v();
        }
        return s;
    }

    static long callK(Dflt d, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += d.k();
        }
        return s;
    }

    static long callF(Abs a, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += a.f();
        }
        return s;
    }

    /** The subclass is defined while this loop runs (OSR-compiled by then). */
    static long midLoop(int n) throws Exception {
        Base b = new Base();
        long s = 0;
        for (int i = 0; i < n; i++) {
            if (i == n / 2) {
                b = (Base) fresh("Sub2");
            }
            s += b.v();
        }
        return s;
    }

    public static void main(String[] args) throws Exception {
        Base base = new Base();
        long before = 0;
        for (int r = 0; r < 10; r++) {
            before = sumV(base, N);
        }
        System.out.println("fact base-before " + before);
        Base sub = (Base) fresh("Sub");
        System.out.println("fact sub-loader-distinct "
                + (sub.getClass().getClassLoader() != Base.class.getClassLoader()));
        System.out.println("fact sub-after " + sumV(sub, N));
        System.out.println("fact mixed-after " + sumMixed(base, sub, N));

        Dflt a = new DfltA();
        long dBefore = 0;
        for (int r = 0; r < 10; r++) {
            dBefore = callK(a, N);
        }
        System.out.println("fact default-before " + dBefore);
        System.out.println("fact default-after " + callK((Dflt) fresh("DfltB"), N));

        Abs one = new One();
        long aBefore = 0;
        for (int r = 0; r < 10; r++) {
            aBefore = callF(one, N);
        }
        System.out.println("fact abstract-before " + aBefore);
        System.out.println("fact abstract-after " + callF((Abs) fresh("Two"), N));

        // `Mid` is loaded by the application loader; `Deep` below it by a
        // fresh one: the new override sits two levels under the site's type.
        Base mid = new Mid();
        sumV(mid, N);
        System.out.println("fact deep-after " + sumV((Base) fresh("Deep"), N));

        System.out.println("fact mid-loop " + midLoop(N));

        if (Boolean.getBoolean("j27.timing")) {
            long best = Long.MAX_VALUE;
            for (int r = 0; r < 5; r++) {
                long t0 = System.nanoTime();
                sumV(sub, 5_000_000);
                best = Math.min(best, System.nanoTime() - t0);
            }
            System.out.println("time sub-after-ns-per-op " + (best / 5_000_000.0));
        }
    }
}
