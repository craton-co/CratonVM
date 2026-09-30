// Interpreter round i1, wave 37, lane L5 -- JVMS §5.3.4 for an OVERRIDE
// across loaders: `Sub` (loader `a`) extends `Base` (loader `b`) and
// overrides `take(Shared)`, while `a` and `b` each define their own `Shared`.
// HotSpot imposes the constraint when it builds `Sub`'s vtable
// (`klassVtable::check_loader_constraints`) and refuses the link.
//
// Evidence for `docs/known-issues/interpreter/i37-L5-proposal-loader-constraints-for-overrides-20261001.md`
// (not implemented in wave 37). Fixed under `--jdk-only` in wave 39
// (`runtime/resolve/loader_constraints.rs` `check_override_constraints_at_link`;
// see also `L5W39OverrideConstraintPending`).
//
// Before wave 39, and under `--compatible` by design: no constraint is
// imposed at vtable construction, so `Sub` links and the call runs:
// `link=ok` and `call=7`.
//
// Run (no setup):
//   javac -d out L5W37LoaderConstraintOverride.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W37LoaderConstraintOverride
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   link=java.lang.LinkageError
//   msg=loader constraint violation for class L5W37LoaderConstraintOverride$Sub: when selecting overriding method 'int L5W37LoaderConstraintOverride$Sub.take(L5W37LoaderConstraintOverride$Shared)' the class loader 'a' @H of the selected method's type L5W37LoaderConstraintOverride$Sub, and the class loader 'b' @H for its super type L5W37LoaderConstraintOverride$Base have different Class objects for the type L5W37LoaderConstraintOverride$Shared used in the signature (L5W37LoaderConstraintOverride$Sub is in unnamed module of loader 'a' @H, parent loader 'b' @H; L5W37LoaderConstraintOverride$Base is in unnamed module of loader 'b' @H, parent loader 'app')

import java.io.IOException;
import java.io.InputStream;
import java.util.Set;

public class L5W37LoaderConstraintOverride {
    static final String P = "L5W37LoaderConstraintOverride$";

    static final class ChildFirst extends ClassLoader {
        private final Set<String> own;

        ChildFirst(String name, ClassLoader parent, Set<String> own) {
            super(name, parent);
            this.own = own;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                if (!own.contains(name)) {
                    return super.loadClass(name, resolve);
                }
                try (InputStream in =
                        ClassLoader.getSystemResourceAsStream(name.replace('.', '/') + ".class")) {
                    byte[] b = in.readAllBytes();
                    return defineClass(name, b, 0, b.length);
                } catch (IOException e) {
                    throw new ClassNotFoundException(name, e);
                }
            }
        }
    }

    public static void main(String[] args) throws Exception {
        ClassLoader app = L5W37LoaderConstraintOverride.class.getClassLoader();
        ChildFirst b = new ChildFirst("b", app, Set.of(P + "Base", P + "Shared", P + "Driver"));
        ChildFirst a = new ChildFirst("a", b, Set.of(P + "Sub", P + "Shared"));
        a.loadClass(P + "Shared");
        b.loadClass(P + "Shared");
        try {
            // Linking `Sub` (its vtable) is where HotSpot refuses.
            Object sub = a.loadClass(P + "Sub").getConstructor().newInstance();
            // `Driver` is `b`'s, like `Base`: the call site imposes nothing.
            Object r = b.loadClass(P + "Driver")
                    .getMethod("run", b.loadClass(P + "Base"))
                    .invoke(null, sub);
            System.out.println("link=ok");
            System.out.println("call=" + r);
        } catch (Throwable t) {
            if (t instanceof java.lang.reflect.InvocationTargetException) {
                t = t.getCause();
            }
            System.out.println("link=" + t.getClass().getName());
            System.out.println("msg=" + String.valueOf(t.getMessage()).replaceAll("@[0-9a-f]+", "@H"));
        }
    }

    public static class Shared {
        public int v = 7;
    }

    public static class Base {
        public int take(Shared s) {
            return -1;
        }
    }

    public static class Sub extends Base {
        @Override
        public int take(Shared s) {
            return s == null ? 7 : s.v;
        }
    }

    public static class Driver {
        public static int run(Base base) {
            return base.take(null);
        }
    }
}
