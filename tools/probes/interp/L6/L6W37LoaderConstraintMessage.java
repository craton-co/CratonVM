// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 37, lane L6 (review of wave 31): the MESSAGE of
// the JVMS 5.3.4 loader-constraint `LinkageError` that `--jdk-only` raises at
// member resolution since wave 31. `L5/L5W29LoaderConstraintMethod` checks
// only the exception class; this is its shape with two NAMED loaders
// (`b` defines `$Api` and `$Shared`, its child `a` defines `$User` and its own
// `$Shared`), printing each message with the identity hashes masked.
//
// At wave 36 CratonVM's message (read from `field_access.rs`
// `loader_constraint_violation`) formatted the loaders with `{:?}` of
// `ClassLoaderId` and printed neither the "used in the signature" clause nor
// the module clauses. Wave 37's lane L5 moved the message to
// `vm/src/runtime/resolve/loader_constraints.rs`; the wave-37 integration
// matches HotSpot in default and --nojit (host run):
// docs/internal/fixed-bugs/interpreter-L6-loader-constraint-message-is-not-hotspots-FIXED-20261001.md.
//
// Run: javac -d out L6W37LoaderConstraintMessage.java && cratonvm --java-home <jdk25> [--nojit] -cp out L6W37LoaderConstraintMessage
//
// Expected HotSpot 25 output (default and -Xint):
//   shared distinct=true
//   take=java.lang.LinkageError: loader constraint violation: when resolving method 'int L6W37LoaderConstraintMessage$Api.take(L6W37LoaderConstraintMessage$Shared)' the class loader 'a' @<hash> of the current class, L6W37LoaderConstraintMessage$User, and the class loader 'b' @<hash> for the method's defining class, L6W37LoaderConstraintMessage$Api, have different Class objects for the type L6W37LoaderConstraintMessage$Shared used in the signature (L6W37LoaderConstraintMessage$User is in unnamed module of loader 'a' @<hash>, parent loader 'b' @<hash>; L6W37LoaderConstraintMessage$Api is in unnamed module of loader 'b' @<hash>, parent loader 'app')
//   field=java.lang.LinkageError: loader constraint violation: when resolving field "held" of type L6W37LoaderConstraintMessage$Shared, the class loader 'a' @<hash> of the current class, L6W37LoaderConstraintMessage$User, and the class loader 'b' @<hash> for the field's defining class, L6W37LoaderConstraintMessage$Api, have different Class objects for type L6W37LoaderConstraintMessage$Shared (L6W37LoaderConstraintMessage$User is in unnamed module of loader 'a' @<hash>, parent loader 'b' @<hash>; L6W37LoaderConstraintMessage$Api is in unnamed module of loader 'b' @<hash>, parent loader 'app')
// `--compatible` admits both accesses by design (wave 31): take=7, field=7.
// Without the `shared distinct` preload HotSpot fails `take` at the later
// load of `b`'s `$Shared` instead ("loader 'b' @<hash> wants to load class
// ...") and prints `field=7`: the open half of the i29-L5 page.
import java.io.IOException;
import java.io.InputStream;
import java.util.Set;

public class L6W37LoaderConstraintMessage {
    static final String SHARED = "L6W37LoaderConstraintMessage$Shared";

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
        ClassLoader app = L6W37LoaderConstraintMessage.class.getClassLoader();
        ChildFirst b = new ChildFirst("b", app, Set.of("L6W37LoaderConstraintMessage$Api", SHARED));
        ChildFirst a = new ChildFirst("a", b, Set.of("L6W37LoaderConstraintMessage$User", SHARED));
        // Both loaders' `$Shared` loaded BEFORE either access resolves, so the
        // violation is found at resolution (the shape wave 31 enforces; a
        // `$Shared` only one side has loaded is the open half of
        // `i29-L5-loader-constraints-are-not-imposed-at-member-resolution`).
        System.out.println("shared distinct=" + (a.loadClass(SHARED) != b.loadClass(SHARED)));
        Class<?> user = a.loadClass("L6W37LoaderConstraintMessage$User");
        for (String what : new String[] {"take", "field"}) {
            try {
                Object r = user.getMethod(what).invoke(null);
                System.out.println(what + "=" + r);
            } catch (java.lang.reflect.InvocationTargetException e) {
                Throwable c = e.getCause();
                String m = String.valueOf(c.getMessage()).replaceAll("@[0-9a-f]+", "@<hash>");
                System.out.println(what + "=" + c.getClass().getName() + ": " + m);
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
