// Interpreter round i1, wave 38, lane L5 -- JVMS §5.3.4 loader constraints at
// an initiating load made by `Class.forName(name, init, loader)`. HotSpot
// routes `forName` through `SystemDictionary`, which checks the constraints
// recorded for the initiating loader exactly as it does for a VM-initiated
// resolution (`L5W37LoaderConstraintPending`, `initiating`).
//
// Loader `b` defines `$Api` and `$Shared` itself; loader `a` (its child)
// defines `$User` and hands `$Shared` requests to a third loader: `c` (which
// defines its OWN `$Shared`) or `b`. `User.take2()` resolves `Api.take2(Shared)`
// with a null, so a constraint `Shared^a = Shared^b` is recorded (b's side
// is loaded, a's is not yet). Then `Class.forName("$Shared", false, a)`:
//
//  forName-conflict   `a` answers `c`'s `Shared`: LinkageError.
//  forName-agree      `a` answers `b`'s `Shared`: the class, no error.
//  forName-free       no constraint was recorded for `a` (no `take2` call):
//                     `c`'s `Shared` is returned.
//
// Run (no setup):
//   javac -d out L5W38ForNameConstraint.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W38ForNameConstraint
//
// Positive control: `CRATONVM_DBG=access` prints
// `[ACCESS-DBG] LOADER-CONSTRAINT DENY #<n> at initiating load: ...` for
// `forName-conflict` (the `forName` native's
// `NativeContext::check_initiating_load`).
//
// Before wave 38 (from the code): `forName-conflict=loaded by c` (the
// `forName` native checked nothing). `--compatible` records no constraint by
// design (AGENTS.md), so it prints `forName-conflict=loaded by c` too.
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   forName-conflict take2=1
//   forName-conflict=java.lang.LinkageError
//   forName-conflict msg=loader constraint violation: loader 'a' @H wants to load class L5W38ForNameConstraint$Shared. A different class with the same name was previously loaded by 'b' @H. (L5W38ForNameConstraint$Shared is in unnamed module of loader 'b' @H, parent loader 'app')
//   forName-agree take2=1
//   forName-agree=loaded by b
//   forName-free=loaded by c

import java.io.IOException;
import java.io.InputStream;
import java.util.Set;

public class L5W38ForNameConstraint {
    static final String P = "L5W38ForNameConstraint$";

    static final class ChildFirst extends ClassLoader {
        private final Set<String> own;
        private final ClassLoader sharedFrom;

        ChildFirst(String name, ClassLoader parent, Set<String> own, ClassLoader sharedFrom) {
            super(name, parent);
            this.own = own;
            this.sharedFrom = sharedFrom;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                if (sharedFrom != null && name.equals(P + "Shared")) {
                    return sharedFrom.loadClass(name);
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

    static String norm(String s) {
        return s == null ? "null" : s.replaceAll("@[0-9a-f]+", "@H");
    }

    static void forName(String label, ClassLoader a) {
        try {
            Class<?> k = Class.forName(P + "Shared", false, a);
            System.out.println(label + "=loaded by " + k.getClassLoader().getName());
        } catch (Throwable t) {
            System.out.println(label + "=" + t.getClass().getName());
            System.out.println(label + " msg=" + norm(t.getMessage()));
        }
    }

    static ChildFirst b() throws Exception {
        ChildFirst b = new ChildFirst("b", L5W38ForNameConstraint.class.getClassLoader(),
                Set.of(P + "Api", P + "Shared"), null);
        b.loadClass(P + "Shared");
        return b;
    }

    static ChildFirst c() {
        return new ChildFirst("c", L5W38ForNameConstraint.class.getClassLoader(),
                Set.of(P + "Shared"), null);
    }

    static void take2(String label, ClassLoader a) throws Exception {
        Object r = a.loadClass(P + "User").getMethod("take2").invoke(null);
        System.out.println(label + " take2=" + r);
    }

    public static void main(String[] args) throws Exception {
        {
            ChildFirst b = b();
            ChildFirst a = new ChildFirst("a", b, Set.of(P + "User"), c());
            take2("forName-conflict", a);
            forName("forName-conflict", a);
        }
        {
            ChildFirst b = b();
            ChildFirst a = new ChildFirst("a", b, Set.of(P + "User"), b);
            take2("forName-agree", a);
            forName("forName-agree", a);
        }
        {
            ChildFirst b = b();
            ChildFirst a = new ChildFirst("a", b, Set.of(P + "User"), c());
            a.loadClass(P + "User");
            forName("forName-free", a);
        }
    }

    public static class Shared {
        public int v = 7;
    }

    public static class Api {
        public static int take2(Shared s) {
            return s == null ? 1 : 2;
        }
    }

    public static class User {
        public static int take2() {
            return Api.take2(null);
        }
    }
}
