// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 117, lane L5 -- JVMS §5.3.4 at a BUILT-IN loader's
// define (`i46-L5-proposal-a-flat-store-define-is-told-from-a-built-in-loaders-own`,
// "Wave 117"; switch `CRATONVM_DEFINE_BUILTIN_CONSTRAINT_REFUSAL`, default ON since wave 134
// by user decision (OFF in waves 118-133); `=0` restores the old admit).
// `L5W37LoaderConstraintPending` with the declaring side the APPLICATION
// loader instead of a user loader `b`:
//
//  app-accessor-first  child-first `a` (parent app) has its own `Shared`; its
//                      `User` resolves the app class `Api.take(Shared)` (the
//                      constraint `Shared`: a == app is recorded, pinned to
//                      a's class); `Api.take`'s `s.v` makes the app loader
//                      load its own `Shared` for the first time -> HotSpot
//                      refuses that define with LinkageError. CratonVM admitted
//                      it (the app's `Shared` and a's both live, one name across
//                      a constraint).
//  app-retry           the same call again: refused again.
//  app-field-first     the same through `Api.held` (a static field typed Shared),
//                      in a fresh run of the case with its own `Shared2`.
//  app-agree           `a` has no `Shared3` of its own: it delegates to app; one
//                      class, no error.
//
// Every case uses its own `Shared*` class (the app loader can load a name only
// once per run). Loader identity hashes are printed as `@H`.
//
// Run (no setup; both modes, JIT and --nojit):
//   javac -d out L5W117BuiltinDefineConstraint.java
//   java -cp out L5W117BuiltinDefineConstraint                                  (HotSpot)
//   CRATONVM_DEFINE_BUILTIN_CONSTRAINT_REFUSAL=1 cratonvm --jdk-only -cp out L5W117BuiltinDefineConstraint
//   CRATONVM_DEFINE_BUILTIN_CONSTRAINT_REFUSAL=1 cratonvm --compatible -cp out L5W117BuiltinDefineConstraint   (records constraints only with
//       CRATONVM_COMPAT_LOADER_CONSTRAINTS; without it every row prints a value, as L5W37 there)
//   CRATONVM_DEFINE_BUILTIN_CONSTRAINT_REFUSAL=0 cratonvm --jdk-only ...   (the old admit:
//       app-accessor-first=7, app-retry=7, app-field-first=7)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// app-accessor-first=java.lang.LinkageError
// app-accessor-first msg=loader constraint violation: loader 'app' wants to load class L5W117BuiltinDefineConstraint$Shared. A different class with the same name was previously loaded by 'a' @H. (L5W117BuiltinDefineConstraint$Shared is in unnamed module of loader 'a' @H, parent loader 'app')
// app-retry=java.lang.LinkageError
// app-retry msg=loader constraint violation: loader 'app' wants to load class L5W117BuiltinDefineConstraint$Shared. A different class with the same name was previously loaded by 'a' @H. (L5W117BuiltinDefineConstraint$Shared is in unnamed module of loader 'a' @H, parent loader 'app')
// app-field-first=java.lang.LinkageError
// app-field-first msg=loader constraint violation: loader 'app' wants to load class L5W117BuiltinDefineConstraint$Shared2. A different class with the same name was previously loaded by 'a' @H. (L5W117BuiltinDefineConstraint$Shared2 is in unnamed module of loader 'a' @H, parent loader 'app')
// app-agree=17
// HOTSPOT_EXPECTED_END

import java.io.IOException;
import java.io.InputStream;
import java.util.Set;

public class L5W117BuiltinDefineConstraint {
    static final String P = "L5W117BuiltinDefineConstraint$";

    static final class ChildFirst extends ClassLoader {
        private final Set<String> own;

        ChildFirst(Set<String> own) {
            super("a", L5W117BuiltinDefineConstraint.class.getClassLoader());
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

    static String norm(String s) {
        return s == null ? "null" : s.replaceAll("@[0-9a-f]+", "@H");
    }

    static void run(String label, ClassLoader a, String method) throws Exception {
        Class<?> user = a.loadClass(P + "User");
        try {
            Object r = user.getMethod(method).invoke(null);
            System.out.println(label + "=" + r);
        } catch (java.lang.reflect.InvocationTargetException e) {
            Throwable t = e.getCause();
            System.out.println(label + "=" + t.getClass().getName());
            System.out.println(label + " msg=" + norm(t.getMessage()));
        }
    }

    public static void main(String[] args) throws Exception {
        ChildFirst a = new ChildFirst(Set.of(P + "User", P + "Shared", P + "Shared2"));
        run("app-accessor-first", a, "take");
        run("app-retry", a, "take");
        run("app-field-first", a, "field");
        run("app-agree", a, "agree");
    }

    public static class Shared {
        public int v = 7;
    }

    public static class Shared2 {
        public int v = 7;
    }

    public static class Shared3 {
        public int v = 17;
    }

    /** Loaded by the application loader (`a` delegates it). */
    public static class Api {
        public static Shared2 held;

        public static int take(Shared s) {
            return s.v;
        }

        public static int readHeld() {
            return held.v;
        }

        public static int take3(Shared3 s) {
            return s.v;
        }
    }

    /** Defined by `a`. */
    public static class User {
        public static int take() {
            return Api.take(new Shared());
        }

        public static int field() {
            Api.held = new Shared2();
            return Api.readHeld();
        }

        public static int agree() {
            return Api.take3(new Shared3());
        }
    }
}
