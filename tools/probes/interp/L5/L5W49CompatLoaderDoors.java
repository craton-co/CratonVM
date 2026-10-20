// Interpreter round i1, wave 49, lane L5 -- the three `--compatible` loader
// behaviours of `docs/internal/fixed-bugs/interpreter-L5-compatible-resolution-door-gates-that-still-differ-from-hotspot-FIXED-20261015.md`
// that wave 49 builds behind switches (items 1, 3 and 6). `--jdk-only` has
// had all three since waves 28-38; the rows must print HotSpot's lines in
// BOTH modes. Each fixed case runs first (a cold VM: empty memo, no record)
// and again later with fresh loaders.
//
//  throw         (item 1, `CRATONVM_COMPAT_LOADER_THROWS`) a user loader whose
//                `loadClass` throws IllegalStateException for `$Target`, a
//                name on the application class path; `$Ref.make()` does
//                `new Target()`. HotSpot: the loader's throwable is the
//                resolution's, and (not a LinkageError) the loader is asked
//                again on the second call. `--compatible` before wave 49: the
//                throw was dropped and the global store answered (`ok`).
//  global-name   (item 3, `CRATONVM_COMPAT_GLOBAL_NAME_ASK`) a loader that
//                answers `javax.security.auth.x500.X500Principal` with its own
//                copy (static `v()` = 6), asked lazily; `l5w49.GRef.run()`
//                does `invokestatic X500Principal.v()`. HotSpot asks the
//                loader: 6. `--compatible` before: java.base's class was
//                bound, `NoSuchMethodError`.
//  constraint    (item 6, `CRATONVM_COMPAT_LOADER_CONSTRAINTS`) JVMS §5.3.4, as
//                `L5W37LoaderConstraintPending`'s `accessor-first` (the
//                constraint is recorded, the later define fails) and
//                `both-loaded` (the resolution fails). `--compatible` before:
//                nothing recorded, a violation only counted: `7` twice.
//
// Run (no setup):
//   javac -d out L5W49CompatLoaderDoors.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W49CompatLoaderDoors
//
// Positive control (`--compatible`, the three switches on): with
// `CRATONVM_DBG=access`, stderr carries `[ACCESS-DBG] LOADER-THROW
// PROPAGATE` (throw rows), `[ACCESS-DBG] LOADER-CONSTRAINT RECORD` and
// `LOADER-CONSTRAINT DENY` (constraint rows); the exit line `[ACCESS-DBG]
// LOADER census: mode=compatible loader-throws-propagated=4 ...`. With the
// switch `=0` the rows print the "before" values above.
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   throw-1=java.lang.IllegalStateException / java.lang.IllegalStateException asks=2
//   global-name-1=6
//   constraint accessor-first=java.lang.LinkageError
//   constraint accessor-first msg=loader constraint violation: loader 'b' @H wants to load class L5W49CompatLoaderDoors$Shared. A different class with the same name was previously loaded by 'a' @H. (L5W49CompatLoaderDoors$Shared is in unnamed module of loader 'a' @H, parent loader 'b' @H)
//   constraint both-loaded=java.lang.LinkageError
//   constraint both-loaded msg=loader constraint violation: when resolving method 'int L5W49CompatLoaderDoors$Api.take(L5W49CompatLoaderDoors$Shared)' the class loader 'a' @H of the current class, L5W49CompatLoaderDoors$User, and the class loader 'b' @H for the method's defining class, L5W49CompatLoaderDoors$Api, have different Class objects for the type L5W49CompatLoaderDoors$Shared used in the signature (L5W49CompatLoaderDoors$User is in unnamed module of loader 'a' @H, parent loader 'b' @H; L5W49CompatLoaderDoors$Api is in unnamed module of loader 'b' @H, parent loader 'app')
//   throw-2=java.lang.IllegalStateException / java.lang.IllegalStateException asks=2
//   global-name-2=6

import java.io.IOException;
import java.io.InputStream;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.util.Set;

public class L5W49CompatLoaderDoors {
    static final String P = "L5W49CompatLoaderDoors$";
    static final String PRINCIPAL = "javax.security.auth.x500.X500Principal";
    static final String GREF = "l5w49.GRef";

    static byte[] appBytes(String name) throws ClassNotFoundException {
        try (InputStream in =
                ClassLoader.getSystemResourceAsStream(name.replace('.', '/') + ".class")) {
            return in.readAllBytes();
        } catch (IOException e) {
            throw new ClassNotFoundException(name, e);
        }
    }

    // --- throw ------------------------------------------------------------

    static final class Throwing extends ClassLoader {
        int asks;

        Throwing() {
            super(L5W49CompatLoaderDoors.class.getClassLoader());
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                if (name.equals(P + "Target")) {
                    asks++;
                    throw new IllegalStateException("refused " + name);
                }
                if (!name.equals(P + "Ref")) {
                    return super.loadClass(name, resolve);
                }
                byte[] b = appBytes(name);
                return defineClass(name, b, 0, b.length);
            }
        }
    }

    static String call(Class<?> k, String method) {
        try {
            return String.valueOf(k.getMethod(method).invoke(null));
        } catch (java.lang.reflect.InvocationTargetException e) {
            return e.getCause().getClass().getName();
        } catch (Throwable t) {
            return "probe " + t;
        }
    }

    static void throwRow(String label) throws Exception {
        Throwing loader = new Throwing();
        Class<?> ref = loader.loadClass(P + "Ref");
        String first = call(ref, "make");
        String second = call(ref, "make");
        System.out.println(label + "=" + first + " / " + second + " asks=" + loader.asks);
    }

    // --- global-name ------------------------------------------------------

    static byte[] ownPrincipal() {
        return ClassFile.of().build(ClassDesc.of(PRINCIPAL), cb -> cb
                .withFlags(ClassFile.ACC_PUBLIC)
                .withSuperclass(ConstantDescs.CD_Object)
                .withMethodBody("v", MethodTypeDesc.of(ConstantDescs.CD_int),
                        ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                        code -> code.bipush(6).ireturn()));
    }

    static byte[] gref() {
        return ClassFile.of().build(ClassDesc.of(GREF), cb -> cb
                .withFlags(ClassFile.ACC_PUBLIC)
                .withSuperclass(ConstantDescs.CD_Object)
                .withMethodBody("run", MethodTypeDesc.of(ConstantDescs.CD_int),
                        ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                        code -> code
                                .invokestatic(ClassDesc.of(PRINCIPAL), "v",
                                        MethodTypeDesc.of(ConstantDescs.CD_int))
                                .ireturn()));
    }

    static final class GlobalNameLoader extends ClassLoader {
        GlobalNameLoader() {
            super(L5W49CompatLoaderDoors.class.getClassLoader());
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                byte[] b;
                if (name.equals(PRINCIPAL)) {
                    b = ownPrincipal();
                } else if (name.equals(GREF)) {
                    b = gref();
                } else {
                    return super.loadClass(name, resolve);
                }
                return defineClass(name, b, 0, b.length);
            }
        }
    }

    static void globalNameRow(String label) throws Exception {
        Class<?> ref = new GlobalNameLoader().loadClass(GREF);
        System.out.println(label + "=" + call(ref, "run"));
    }

    // --- constraint -------------------------------------------------------

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
                byte[] b = appBytes(name);
                return defineClass(name, b, 0, b.length);
            }
        }
    }

    static String norm(String s) {
        return s == null ? "null" : s.replaceAll("@[0-9a-f]+", "@H");
    }

    static void constraintRow(String label, boolean bothLoaded) throws Exception {
        ChildFirst b = new ChildFirst("b", L5W49CompatLoaderDoors.class.getClassLoader(),
                Set.of(P + "Api", P + "Shared"));
        ChildFirst a = new ChildFirst("a", b, Set.of(P + "User", P + "Shared"));
        if (bothLoaded) {
            b.loadClass(P + "Shared");
            a.loadClass(P + "Shared");
        }
        Class<?> user = a.loadClass(P + "User");
        try {
            Object r = user.getMethod("take").invoke(null);
            System.out.println(label + "=" + r);
        } catch (java.lang.reflect.InvocationTargetException e) {
            Throwable t = e.getCause();
            System.out.println(label + "=" + t.getClass().getName());
            System.out.println(label + " msg=" + norm(t.getMessage()));
        }
    }

    public static void main(String[] args) throws Exception {
        throwRow("throw-1");
        globalNameRow("global-name-1");
        constraintRow("constraint accessor-first", false);
        constraintRow("constraint both-loaded", true);
        throwRow("throw-2");
        globalNameRow("global-name-2");
    }

    public static class Target {
    }

    public static class Ref {
        public static Object make() {
            return new Target();
        }
    }

    public static class Shared {
        public int v = 7;
    }

    public static class Api {
        public static int take(Shared s) {
            return s.v;
        }
    }

    public static class User {
        public static int take() {
            return Api.take(new Shared());
        }
    }
}
