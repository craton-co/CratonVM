// Interpreter round i1, wave 29, lane L5 -- JVMS §5.3.4 loader constraints at
// member resolution: a class of loader A calls a static method (and stores a
// static field) of a class of loader B whose descriptor names a type both
// loaders define their own copy of. The two loaders must agree on that type;
// they do not, so HotSpot fails the resolution with `LinkageError` ("loader
// constraint violation"), and the access never runs.
//
// Loader B (child-first for `$Api`, `$Shared`) and loader A (its child;
// child-first for `$User`, `$Shared`, parent-delegating `$Api` to B).
// `User.take()` does `Api.take(new Shared())`: A's `Shared` handed to a method
// B verified against B's `Shared`. `User.field()` stores A's `Shared` in B's
// `Api.held` (typed with B's `Shared`).
//
// CratonVM (from the code, not run): `classloading/src/loader_constraints.rs`
// records constraints only for a class's SUPERCLASS at define time and never
// throws (`ClassManager` supertype link: "Recorded, not thrown"); no method or
// field resolution imposes one. So both accesses run and read A's object
// through B's `Shared` layout: `take=7`, `field=7` (the layouts agree here, so
// the type confusion is silent). Filed as
// `docs/internal/fixed-bugs/interpreter-L5-loader-constraints-are-not-imposed-at-member-resolution-FIXED-20261010.md`.
//
// Wave 31: `--jdk-only` (the default) refuses both resolutions, as HotSpot
// does; `--compatible` counts them (`CRATONVM_DBG=access`,
// `loader-constraint-violations=2`) and still prints `take=7`, `field=7`.
//
// Run (no setup):
//   javac -d out L5W29LoaderConstraintMethod.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W29LoaderConstraintMethod
//
// Expected HotSpot 25 output (compare verbatim):
//   shared distinct=true
//   take=java.lang.LinkageError
//   field=java.lang.LinkageError

import java.io.IOException;
import java.io.InputStream;
import java.util.Set;

public class L5W29LoaderConstraintMethod {
    static final String SHARED = "L5W29LoaderConstraintMethod$Shared";

    static final class ChildFirst extends ClassLoader {
        private final Set<String> own;

        ChildFirst(ClassLoader parent, Set<String> own) {
            super(parent);
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
        ClassLoader app = L5W29LoaderConstraintMethod.class.getClassLoader();
        ChildFirst b = new ChildFirst(app, Set.of("L5W29LoaderConstraintMethod$Api", SHARED));
        ChildFirst a = new ChildFirst(b, Set.of("L5W29LoaderConstraintMethod$User", SHARED));
        System.out.println("shared distinct=" + (a.loadClass(SHARED) != b.loadClass(SHARED)));
        Class<?> user = a.loadClass("L5W29LoaderConstraintMethod$User");
        for (String what : new String[] {"take", "field"}) {
            try {
                Object r = user.getMethod(what).invoke(null);
                System.out.println(what + "=" + r);
            } catch (java.lang.reflect.InvocationTargetException e) {
                System.out.println(what + "=" + e.getCause().getClass().getName());
            }
        }
    }

    public static class Shared {
        public int v = 7;
    }

    public static class Api {
        public static Shared held;

        public static int take(Shared s) {
            return s.v;
        }
    }

    public static class User {
        public static int take() {
            return Api.take(new Shared());
        }

        public static int field() {
            Api.held = new Shared();
            return Api.held.v;
        }
    }
}
