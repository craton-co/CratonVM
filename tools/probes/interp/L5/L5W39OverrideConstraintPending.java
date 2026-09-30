// Interpreter round i1, wave 39, lane L5 -- JVMS §5.3.4 for an OVERRIDE
// across loaders when one side has not loaded the constrained type yet.
// `Sub` (loader `a`) extends `Base` (loader `b`) and overrides
// `take(Shared)`. Linking `Sub` imposes Shared^a = Shared^b (HotSpot's
// `klassVtable` records it through `SystemDictionary::check_signature_loaders`),
// so when `a` later defines its OWN `Shared` the define is refused.
//
// * `pending`: `b` has loaded `Shared`, `a` has not; `Sub` links, then `a`
//   defining `Shared` is a `LinkageError`.
// * `agree`: a loader that delegates `Shared` to `b`: links, and the call runs.
// * `both` (the wave-37 evidence probe `L5W37LoaderConstraintOverride`'s
//   case): both loaders have their own `Shared` first; the link is refused.
//
// Fix for `docs/internal/fixed-bugs/interpreter-L5-loader-constraints-are-not-imposed-at-member-resolution-FIXED-20261010.md`
// (overrides across loaders), built from part of
// `i37-L5-proposal-loader-constraints-for-overrides-20261001.md`:
// `runtime/resolve/loader_constraints.rs` `check_override_constraints_at_link`,
// called from `vm_util::link_claimed_class` (`--jdk-only`). Before wave 39 (and
// under `--compatible`, by design) nothing was imposed: `pending define=ok`,
// `both link=ok`.
//
// Positive control: `CRATONVM_DBG=access` prints
// `[ACCESS-DBG] LOADER-CONSTRAINT RECORD ...$Shared: ...` when `pending`'s
// `Sub` links, `[ACCESS-DBG] LOADER-CONSTRAINT DENY at define: ...` for its
// define, and `[ACCESS-DBG] LOADER-CONSTRAINT DENY #n at link: ...` for `both`.
//
// Run (no setup):
//   javac -d out L5W39OverrideConstraintPending.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W39OverrideConstraintPending
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim; `@H` replaces the identity hashes):
//   pending link=ok call=7
//   pending define=java.lang.LinkageError
//   pending msg=loader constraint violation: loader 'a' @H wants to load class L5W39OverrideConstraintPending$Shared. A different class with the same name was previously loaded by 'b' @H. (L5W39OverrideConstraintPending$Shared is in unnamed module of loader 'b' @H, parent loader 'app')
//   agree link=ok call=7
//   both link=java.lang.LinkageError
//   both msg=loader constraint violation for class L5W39OverrideConstraintPending$Sub: when selecting overriding method 'int L5W39OverrideConstraintPending$Sub.take(L5W39OverrideConstraintPending$Shared)' the class loader 'a' @H of the selected method's type L5W39OverrideConstraintPending$Sub, and the class loader 'b' @H for its super type L5W39OverrideConstraintPending$Base have different Class objects for the type L5W39OverrideConstraintPending$Shared used in the signature (L5W39OverrideConstraintPending$Sub is in unnamed module of loader 'a' @H, parent loader 'b' @H; L5W39OverrideConstraintPending$Base is in unnamed module of loader 'b' @H, parent loader 'app')

import java.io.IOException;
import java.io.InputStream;
import java.util.Set;

public class L5W39OverrideConstraintPending {
    static final String P = "L5W39OverrideConstraintPending$";

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

    static void link(String label, ChildFirst a, ChildFirst b) {
        try {
            Object sub = a.loadClass(P + "Sub").getConstructor().newInstance();
            Object r = b.loadClass(P + "Driver")
                    .getMethod("run", b.loadClass(P + "Base"))
                    .invoke(null, sub);
            System.out.println(label + " link=ok call=" + r);
        } catch (Throwable t) {
            t = unwrap(t);
            System.out.println(label + " link=" + t.getClass().getName());
            System.out.println(label + " msg=" + msg(t));
        }
    }

    public static void main(String[] args) throws Exception {
        ClassLoader app = L5W39OverrideConstraintPending.class.getClassLoader();
        Set<String> bOwn = Set.of(P + "Base", P + "Shared", P + "Driver");

        ChildFirst b1 = new ChildFirst("b", app, bOwn);
        ChildFirst a1 = new ChildFirst("a", b1, Set.of(P + "Sub", P + "Shared"));
        b1.loadClass(P + "Shared");
        link("pending", a1, b1);
        try {
            Class<?> own = a1.loadClass(P + "Shared");
            System.out.println("pending define=ok same=" + (own == b1.loadClass(P + "Shared")));
        } catch (Throwable t) {
            System.out.println("pending define=" + t.getClass().getName());
            System.out.println("pending msg=" + msg(t));
        }

        ChildFirst b2 = new ChildFirst("b", app, bOwn);
        ChildFirst a2 = new ChildFirst("a", b2, Set.of(P + "Sub"));
        link("agree", a2, b2);

        ChildFirst b3 = new ChildFirst("b", app, bOwn);
        ChildFirst a3 = new ChildFirst("a", b3, Set.of(P + "Sub", P + "Shared"));
        a3.loadClass(P + "Shared");
        b3.loadClass(P + "Shared");
        link("both", a3, b3);
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
