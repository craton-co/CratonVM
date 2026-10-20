// Interpreter round i1, wave 40, lane L5 -- JVMS §5.3.4 for a method that
// implements a SUPERINTERFACE method across loaders (HotSpot's itable check,
// `klassItable::initialize_itable_for_interface` ->
// `SystemDictionary::check_signature_loaders`). Each case builds fresh loaders
// `b` (the interface side) and `a` (the implementation side, parent `b`);
// both may define their own `Shared`.
//
// * `both`: `Impl` (a) implements `Api` (b) and declares `take(Shared)`; both
//   loaders have their own `Shared` first: the link of `Impl` is refused.
// * `inherited`: `Impl2` (a) extends `Mid` (a), which declares `take(Shared)`
//   without implementing `Api`; `Impl2 implements Api`. The selected method's
//   holder is `Mid`: refused, naming `Mid`.
// * `default`: `Impl3` (a) implements `Def` (a), an interface that extends
//   `Api` (b) and supplies a default `take(Shared)`. The selected method is the
//   default: refused, naming `Def`.
// * `pending`: only `b` has loaded `Shared`; `Impl` links and the call runs;
//   then `a` defining its own `Shared` is refused (the recorded constraint).
// * `agree`: `a` delegates `Shared` to `b`: links, the call runs.
//
// Fix for `docs/internal/fixed-bugs/interpreter-L5-loader-constraints-are-not-imposed-at-member-resolution-FIXED-20261010.md`
// (the itable half): `runtime/resolve/loader_constraints.rs`
// `check_override_constraints_at_link` (`--jdk-only`). Before wave 40, and
// under `--compatible` by design, every case links and runs:
// `both link=ok call=7`, `inherited link=ok call=7`, `default link=ok call=7`,
// `pending define=ok same=false`.
//
// Positive control: `CRATONVM_DBG=access` prints
// `[ACCESS-DBG] LOADER-CONSTRAINT DENY #n at link: loader constraint violation
// in interface itable initialization ...` for `both`, `inherited`, `default`,
// and `[ACCESS-DBG] LOADER-CONSTRAINT RECORD ...$Shared` then `... DENY at
// define:` for `pending`.
//
// Run (no setup):
//   javac -d out L5W40ItableConstraint.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W40ItableConstraint
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim; `@H` replaces the identity hashes):
//   both link=java.lang.LinkageError
//   both msg=loader constraint violation in interface itable initialization for class L5W40ItableConstraint$Impl: when selecting method 'int L5W40ItableConstraint$Api.take(L5W40ItableConstraint$Shared)' the class loader 'b' @H for super interface L5W40ItableConstraint$Api, and the class loader 'a' @H of the selected method's class, L5W40ItableConstraint$Impl have different Class objects for the type L5W40ItableConstraint$Shared used in the signature (L5W40ItableConstraint$Api is in unnamed module of loader 'b' @H, parent loader 'app'; L5W40ItableConstraint$Impl is in unnamed module of loader 'a' @H, parent loader 'b' @H)
//   inherited link=java.lang.LinkageError
//   inherited msg=loader constraint violation in interface itable initialization for class L5W40ItableConstraint$Impl2: when selecting method 'int L5W40ItableConstraint$Api.take(L5W40ItableConstraint$Shared)' the class loader 'b' @H for super interface L5W40ItableConstraint$Api, and the class loader 'a' @H of the selected method's class, L5W40ItableConstraint$Mid have different Class objects for the type L5W40ItableConstraint$Shared used in the signature (L5W40ItableConstraint$Api is in unnamed module of loader 'b' @H, parent loader 'app'; L5W40ItableConstraint$Mid is in unnamed module of loader 'a' @H, parent loader 'b' @H)
//   default link=java.lang.LinkageError
//   default msg=loader constraint violation in interface itable initialization for class L5W40ItableConstraint$Impl3: when selecting method 'int L5W40ItableConstraint$Api.take(L5W40ItableConstraint$Shared)' the class loader 'b' @H for super interface L5W40ItableConstraint$Api, and the class loader 'a' @H of the selected method's interface, L5W40ItableConstraint$Def have different Class objects for the type L5W40ItableConstraint$Shared used in the signature (L5W40ItableConstraint$Api is in unnamed module of loader 'b' @H, parent loader 'app'; L5W40ItableConstraint$Def is in unnamed module of loader 'a' @H, parent loader 'b' @H)
//   pending link=ok call=7
//   pending define=java.lang.LinkageError
//   pending msg=loader constraint violation: loader 'a' @H wants to load class L5W40ItableConstraint$Shared. A different class with the same name was previously loaded by 'b' @H. (L5W40ItableConstraint$Shared is in unnamed module of loader 'b' @H, parent loader 'app')
//   agree link=ok call=7

import java.io.IOException;
import java.io.InputStream;
import java.util.Set;

public class L5W40ItableConstraint {
    static final String P = "L5W40ItableConstraint$";

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

    static String msg(Throwable t) {
        return String.valueOf(t.getMessage()).replaceAll("@[0-9a-f]+", "@H");
    }

    static Throwable unwrap(Throwable t) {
        return t instanceof java.lang.reflect.InvocationTargetException ? t.getCause() : t;
    }

    /** Link `impl` through `a`, then call `Driver.run(Api)` through `b`. */
    static void linkAndCall(String row, ChildFirst a, ChildFirst b, String impl) {
        try {
            Object o = a.loadClass(P + impl).getConstructor().newInstance();
            Object r = b.loadClass(P + "Driver")
                    .getMethod("run", b.loadClass(P + "Api"))
                    .invoke(null, o);
            System.out.println(row + " link=ok call=" + r);
        } catch (Throwable t) {
            t = unwrap(t);
            System.out.println(row + " link=" + t.getClass().getName());
            System.out.println(row + " msg=" + msg(t));
        }
    }

    static ChildFirst b() {
        return new ChildFirst("b", L5W40ItableConstraint.class.getClassLoader(),
                Set.of(P + "Api", P + "Shared", P + "Driver"));
    }

    static ChildFirst a(ChildFirst b, boolean ownShared) {
        return ownShared
                ? new ChildFirst("a", b, Set.of(P + "Impl", P + "Mid", P + "Impl2", P + "Def",
                        P + "Impl3", P + "Shared"))
                : new ChildFirst("a", b, Set.of(P + "Impl", P + "Mid", P + "Impl2", P + "Def",
                        P + "Impl3"));
    }

    public static void main(String[] args) throws Exception {
        for (String[] c : new String[][] {{"both", "Impl"}, {"inherited", "Impl2"},
                {"default", "Impl3"}}) {
            ChildFirst b = b();
            ChildFirst a = a(b, true);
            a.loadClass(P + "Shared");
            b.loadClass(P + "Shared");
            linkAndCall(c[0], a, b, c[1]);
        }
        {
            ChildFirst b = b();
            ChildFirst a = a(b, true);
            b.loadClass(P + "Shared");
            linkAndCall("pending", a, b, "Impl");
            try {
                Class<?> s = a.loadClass(P + "Shared");
                System.out.println("pending define=ok same=" + (s == b.loadClass(P + "Shared")));
            } catch (Throwable t) {
                System.out.println("pending define=" + t.getClass().getName());
                System.out.println("pending msg=" + msg(t));
            }
        }
        {
            ChildFirst b = b();
            ChildFirst a = a(b, false);
            linkAndCall("agree", a, b, "Impl");
        }
    }

    public static class Shared {
        public int v = 7;
    }

    public interface Api {
        int take(Shared s);
    }

    public static class Impl implements Api {
        @Override
        public int take(Shared s) {
            return s == null ? 7 : -1;
        }
    }

    public static class Mid {
        public int take(Shared s) {
            return s == null ? 7 : -1;
        }
    }

    public static class Impl2 extends Mid implements Api {
    }

    public interface Def extends Api {
        @Override
        default int take(Shared s) {
            return s == null ? 7 : -1;
        }
    }

    public static class Impl3 implements Def {
    }

    public static class Driver {
        public static int run(Api api) {
            return api.take(null);
        }
    }
}
