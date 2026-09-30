// Interpreter round i1, wave 40, lane L5 -- what a VM-initiated resolution
// does with an exception thrown by the `findClass` of a loader that does NOT
// override `loadClass` (so its `loadClass` is the JDK's, which CratonVM runs
// as its base-delegation native `cl_real_load_class_base`). JVMS §5.3: a
// `ClassNotFoundException` becomes `NoClassDefFoundError` (cause: the CNFE);
// any other throwable propagates as it is. The loader's parent is null (or
// the platform loader), so on HotSpot nothing but its own `findClass` can
// answer `$Target`, which IS on the application class path: a VM that
// swallows the throw and asks the flat store finds a class.
//
// Rows: `ise`, `error`, `linkage`, `cnfe` (null parent), `platform-ise`
// (platform parent), and `define-only`: a null-parent loader that overrides
// nothing and defines `$Ref` itself (the JDK `findClass` throws the CNFE).
//
// Evidence and fix for
// `docs/internal/fixed-bugs/interpreter-L5-a-vm-initiated-loadclass-exception-is-swallowed-into-a-global-fallback-FIXED-20261005.md`
// (the base-delegation remainder). Before wave 40, and under `--compatible` by
// design, every row printed `ok` (the drive's global fallback defined
// `$Target` in the application loader). Wave 40 (`--jdk-only`,
// `constants.rs` `drive_defining_loader_load_named`'s `base_throw`,
// `classloader_real::base_load_class_miss_is_the_loaders`): HotSpot's rows.
//
// Positive control: `CRATONVM_DBG=access` prints six
// `[ACCESS-DBG] LOADER-THROW PROPAGATE #n ... from L5W40BaseLoaderThrow$FindOnly.loadClass("L5W40BaseLoaderThrow$Target") ...`
// lines (the last from `$DefineOnly`), and the exit census counts
// `loader-throws-propagated=6`.
//
// Run (no setup):
//   javac -d out L5W40BaseLoaderThrow.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W40BaseLoaderThrow
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   ise=java.lang.IllegalStateException cause=null
//   error=java.lang.AssertionError cause=null
//   linkage=java.lang.LinkageError cause=null
//   cnfe=java.lang.NoClassDefFoundError cause=java.lang.ClassNotFoundException
//   platform-ise=java.lang.IllegalStateException cause=null
//   define-only=java.lang.NoClassDefFoundError cause=java.lang.ClassNotFoundException

import java.io.IOException;
import java.io.InputStream;

public class L5W40BaseLoaderThrow {
    static final String P = "L5W40BaseLoaderThrow$";

    /** Overrides `findClass` only: its `loadClass` is the JDK's. */
    static final class FindOnly extends ClassLoader {
        final String kind;

        FindOnly(String kind, ClassLoader parent) {
            super(parent);
            this.kind = kind;
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            if (name.equals(P + "Target")) {
                switch (kind) {
                    case "ise":
                    case "platform-ise":
                        throw new IllegalStateException("refused " + name);
                    case "error":
                        throw new AssertionError("refused " + name);
                    case "linkage":
                        throw new LinkageError("refused " + name);
                    default:
                        throw new ClassNotFoundException(name);
                }
            }
            if (!name.equals(P + "Ref")) {
                throw new ClassNotFoundException(name);
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

    /** Overrides nothing: defines `$Ref` through `define`, null parent. */
    static final class DefineOnly extends ClassLoader {
        DefineOnly() {
            super(null);
        }

        Class<?> define(String name) throws IOException {
            try (InputStream in =
                    ClassLoader.getSystemResourceAsStream(name.replace('.', '/') + ".class")) {
                byte[] b = in.readAllBytes();
                return defineClass(name, b, 0, b.length);
            }
        }
    }

    static void run(String kind, Class<?> ref) throws Exception {
        try {
            ref.getMethod("make").invoke(null);
            System.out.println(kind + "=ok");
        } catch (java.lang.reflect.InvocationTargetException e) {
            Throwable t = e.getCause();
            System.out.println(kind + "=" + t.getClass().getName() + " cause="
                    + (t.getCause() == null ? "null" : t.getCause().getClass().getName()));
        }
    }

    public static void main(String[] args) throws Exception {
        for (String kind : new String[] {"ise", "error", "linkage", "cnfe", "platform-ise"}) {
            ClassLoader parent = kind.startsWith("platform") ? ClassLoader.getPlatformClassLoader() : null;
            run(kind, new FindOnly(kind, parent).loadClass(P + "Ref"));
        }
        run("define-only", new DefineOnly().define(P + "Ref"));
    }

    public static class Target {
    }

    public static class Ref {
        public static Object make() {
            return new Target();
        }
    }
}
