// Interpreter round i1, wave 37, lane L5 -- JVMS §5.3.4 loader constraints on a
// type only ONE side (or neither side) has loaded when the member is resolved.
// The resolution itself succeeds; the constraint is RECORDED, and the later
// load of the name by the other loader fails with `LinkageError` (HotSpot's
// `SystemDictionary::check_constraints`, at define and at an initiating load).
//
// Loader `b` (child-first for `$Api`, `$Shared`), loader `a` (its child;
// child-first for `$User`, and for `$Shared` unless the case says otherwise).
// Every case builds fresh loaders. Loader identity hashes are printed as `@H`.
//
// Cases:
//  accessor-first   `a` has its own `Shared` (the `new`), `b` has none; `a`
//                   resolves `Api.take(Shared)`; `Api.take`'s `s.v` makes `b`
//                   define its own `Shared` -> LinkageError at b's define.
//  declaring-first  `b` has defined `Shared`, `a` has none; `a` resolves
//                   `Api.take2(Shared)` with a null; then `new Shared()` makes
//                   `a` define its own -> LinkageError at a's define.
//  field-first      as accessor-first, through B's static field `Api.held`.
//  initiating       `b` has defined `Shared`; `a` resolves `Api.take2`, then
//                   `new Shared()` makes `a` delegate `Shared` to a third
//                   loader `c`, which defines its own: `c`'s define is legal,
//                   `a`'s initiating record is not -> LinkageError.
//  agree            neither side has `Shared`; `a` resolves `Api.take2`, then
//                   `a` delegates `Shared` to `b`: one class, no error.
//  both-loaded      both already have their own `Shared` (wave 31's case):
//                   the resolution itself fails.
//  retry            accessor-first's `Api.take` called a second time: the
//                   define is attempted again and fails again.
//
// Run (no setup):
//   javac -d out L5W37LoaderConstraintPending.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W37LoaderConstraintPending
//
// Positive control (the recording and both checks): run with
// `CRATONVM_DBG=access`; stderr carries `[ACCESS-DBG] LOADER-CONSTRAINT RECORD`
// lines (accessor-first, declaring-first, field-first, initiating, agree),
// `[ACCESS-DBG] LOADER-CONSTRAINT DENY at define:` (accessor-first,
// declaring-first, field-first, retry-1/2) and
// `[ACCESS-DBG] LOADER-CONSTRAINT DENY #<n> at initiating load:` (initiating).
//
// Before wave 37 (from the code, not run): the resolution recorded nothing,
// so every row but `both-loaded` printed a value (`accessor-first=7`,
// `declaring-first=17`, `field-first=7`, `initiating=17`, `retry-1=7`,
// `retry-2=7`), and `both-loaded msg` named loaders by CratonVM id.
//
// `--compatible` (by design, AGENTS.md): nothing is recorded and a violation
// is only counted, so every row prints a value (`both-loaded=7`).
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   accessor-first=java.lang.LinkageError
//   accessor-first msg=loader constraint violation: loader 'b' @H wants to load class L5W37LoaderConstraintPending$Shared. A different class with the same name was previously loaded by 'a' @H. (L5W37LoaderConstraintPending$Shared is in unnamed module of loader 'a' @H, parent loader 'b' @H)
//   declaring-first=java.lang.LinkageError
//   declaring-first msg=loader constraint violation: loader 'a' @H wants to load class L5W37LoaderConstraintPending$Shared. A different class with the same name was previously loaded by 'b' @H. (L5W37LoaderConstraintPending$Shared is in unnamed module of loader 'b' @H, parent loader 'app')
//   field-first=java.lang.LinkageError
//   field-first msg=loader constraint violation: loader 'b' @H wants to load class L5W37LoaderConstraintPending$Shared. A different class with the same name was previously loaded by 'a' @H. (L5W37LoaderConstraintPending$Shared is in unnamed module of loader 'a' @H, parent loader 'b' @H)
//   initiating=java.lang.LinkageError
//   initiating msg=loader constraint violation: loader 'a' @H wants to load class L5W37LoaderConstraintPending$Shared. A different class with the same name was previously loaded by 'b' @H. (L5W37LoaderConstraintPending$Shared is in unnamed module of loader 'b' @H, parent loader 'app')
//   agree=17
//   both-loaded=java.lang.LinkageError
//   both-loaded msg=loader constraint violation: when resolving method 'int L5W37LoaderConstraintPending$Api.take(L5W37LoaderConstraintPending$Shared)' the class loader 'a' @H of the current class, L5W37LoaderConstraintPending$User, and the class loader 'b' @H for the method's defining class, L5W37LoaderConstraintPending$Api, have different Class objects for the type L5W37LoaderConstraintPending$Shared used in the signature (L5W37LoaderConstraintPending$User is in unnamed module of loader 'a' @H, parent loader 'b' @H; L5W37LoaderConstraintPending$Api is in unnamed module of loader 'b' @H, parent loader 'app')
//   retry-1=java.lang.LinkageError
//   retry-1 msg=loader constraint violation: loader 'b' @H wants to load class L5W37LoaderConstraintPending$Shared. A different class with the same name was previously loaded by 'a' @H. (L5W37LoaderConstraintPending$Shared is in unnamed module of loader 'a' @H, parent loader 'b' @H)
//   retry-2=java.lang.LinkageError
//   retry-2 msg=loader constraint violation: loader 'b' @H wants to load class L5W37LoaderConstraintPending$Shared. A different class with the same name was previously loaded by 'a' @H. (L5W37LoaderConstraintPending$Shared is in unnamed module of loader 'a' @H, parent loader 'b' @H)

import java.io.IOException;
import java.io.InputStream;
import java.util.Set;

public class L5W37LoaderConstraintPending {
    static final String P = "L5W37LoaderConstraintPending$";

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

    static ChildFirst b() {
        return new ChildFirst("b", L5W37LoaderConstraintPending.class.getClassLoader(),
                Set.of(P + "Api", P + "Shared"), null);
    }

    static ChildFirst a(ClassLoader b, boolean ownShared, ClassLoader sharedFrom) {
        return new ChildFirst("a", b,
                ownShared ? Set.of(P + "User", P + "Shared") : Set.of(P + "User"), sharedFrom);
    }

    public static void main(String[] args) throws Exception {
        {
            ChildFirst b = b();
            run("accessor-first", a(b, true, null), "take");
        }
        {
            ChildFirst b = b();
            b.loadClass(P + "Shared");
            run("declaring-first", a(b, true, null), "take2ThenNew");
        }
        {
            ChildFirst b = b();
            run("field-first", a(b, true, null), "field");
        }
        {
            ChildFirst b = b();
            b.loadClass(P + "Shared");
            ChildFirst c = new ChildFirst("c", L5W37LoaderConstraintPending.class.getClassLoader(),
                    Set.of(P + "Shared"), null);
            run("initiating", a(b, false, c), "take2ThenNew");
        }
        {
            ChildFirst b = b();
            run("agree", a(b, false, null), "take2ThenNew");
        }
        {
            ChildFirst b = b();
            ChildFirst a = a(b, true, null);
            b.loadClass(P + "Shared");
            a.loadClass(P + "Shared");
            run("both-loaded", a, "take");
        }
        {
            ChildFirst b = b();
            ChildFirst a = a(b, true, null);
            run("retry-1", a, "take");
            run("retry-2", a, "take");
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

        public static int take2(Shared s) {
            return s == null ? 1 : 2;
        }

        public static int readHeld() {
            return held.v;
        }
    }

    public static class User {
        public static int take() {
            return Api.take(new Shared());
        }

        public static int take2ThenNew() {
            int r = Api.take2(null);
            return r * 10 + new Shared().v;
        }

        public static int field() {
            Api.held = new Shared();
            return Api.readHeld();
        }
    }
}
