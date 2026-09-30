// Interpreter round i1, wave 41, lane L5 -- an ARRAY class reference
// (`ldc X[].class`, `checkcast X[]`) whose component the initiating loader
// refuses. JVMS §5.3.3: the component is resolved by the same loader, so the
// loader is asked for `X` ONCE per resolution, and its throwable is the
// resolution's: an `IllegalStateException` propagates as it is (not recorded:
// the next execution asks again), a `ClassNotFoundException` becomes
// `NoClassDefFoundError: <internal array-element name>` caused by it, and is
// recorded (the next execution does not ask).
//
// `$Ref` is defined by the loader (which overrides `loadClass`, parent: the
// application loader); `$Target` IS on the application class path, so a VM
// that drops the loader's throw and asks the flat store finds a class.
//
// Rows (one fresh loader each): `<row> first=`, `<row> second=` (the same
// instruction executed again), `<row> asked=N` (how often the loader saw
// `$Target`).
//
// Fix for the array-component asks of
// `docs/internal/fixed-bugs/interpreter-L5-a-vm-initiated-loadclass-exception-is-swallowed-into-a-global-fallback-FIXED-20261005.md`
// (the wave-38 "array-component pre-pass" remainder). Before wave 41 CratonVM
// (`--jdk-only`, from the code) asked the loader through the unchecked drive
// in `resolve_array_class_loader_aware`, dropped its throw, and let
// `ClassManager::synthesize_array_class_for_loader` load the APPLICATION's
// `$Target` from the flat store: the `ldc` rows printed `ok class`, the
// `checkcast` rows a `java.lang.ClassCastException` (an `Object[]` is not the
// application's `Target[]`), and every row `asked=1`. The last row, a
// child-first loader that DEFINES its own `$Target`, asked after the
// application loader has loaded its `$Target`: `Target[]` was an array of the
// application's `$Target` (`ClassManager::find_class_by_name_for_loader` falls
// back to the built-in chain before the loader is asked): `child-ldc=ok app`,
// `child-ldc asked=0`. `--compatible` keeps all of that by design.
//
// Positive control: `CRATONVM_DBG=access` prints
// `[ACCESS-DBG] LOADER-THROW PROPAGATE #n ... from L5W41ArrayComponentLoaderThrow$L.loadClass("L5W41ArrayComponentLoaderThrow$Target") ...`
// six times (two per `-ise` row, one per `-cnfe` row) and the exit census
// counts `loader-throws-propagated=6`.
//
// Run (no setup):
//   javac -d out L5W41ArrayComponentLoaderThrow.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W41ArrayComponentLoaderThrow
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   ldc-ise first=java.lang.IllegalStateException: refused L5W41ArrayComponentLoaderThrow$Target cause=null
//   ldc-ise second=java.lang.IllegalStateException: refused L5W41ArrayComponentLoaderThrow$Target cause=null
//   ldc-ise asked=2
//   ldc-cnfe first=java.lang.NoClassDefFoundError: [LL5W41ArrayComponentLoaderThrow$Target; cause=java.lang.ClassNotFoundException: refused L5W41ArrayComponentLoaderThrow$Target
//   ldc-cnfe second=java.lang.NoClassDefFoundError: [LL5W41ArrayComponentLoaderThrow$Target; cause=java.lang.ClassNotFoundException: refused L5W41ArrayComponentLoaderThrow$Target
//   ldc-cnfe asked=1
//   checkcast-ise first=java.lang.IllegalStateException: refused L5W41ArrayComponentLoaderThrow$Target cause=null
//   checkcast-ise second=java.lang.IllegalStateException: refused L5W41ArrayComponentLoaderThrow$Target cause=null
//   checkcast-ise asked=2
//   checkcast-cnfe first=java.lang.NoClassDefFoundError: [LL5W41ArrayComponentLoaderThrow$Target; cause=java.lang.ClassNotFoundException: refused L5W41ArrayComponentLoaderThrow$Target
//   checkcast-cnfe second=java.lang.NoClassDefFoundError: [LL5W41ArrayComponentLoaderThrow$Target; cause=java.lang.ClassNotFoundException: refused L5W41ArrayComponentLoaderThrow$Target
//   checkcast-cnfe asked=1
//   app target=true
//   child-ldc=ok own
//   child-ldc asked=1

import java.io.IOException;
import java.io.InputStream;
import java.lang.reflect.InvocationTargetException;

public class L5W41ArrayComponentLoaderThrow {
    static final String P = "L5W41ArrayComponentLoaderThrow$";

    static final class L extends ClassLoader {
        final String mode;
        int asked;

        L(String mode) {
            super(L5W41ArrayComponentLoaderThrow.class.getClassLoader());
            this.mode = mode;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (name.equals(P + "Target")) {
                asked++;
                if (mode.equals("cnfe")) {
                    throw new ClassNotFoundException("refused " + name);
                }
                if (mode.equals("ise")) {
                    throw new IllegalStateException("refused " + name);
                }
            } else if (!name.equals(P + "Ref")) {
                return super.loadClass(name, resolve);
            }
            // `$Ref`, and `$Target` in the `child` mode: defined here.
            Class<?> c = findLoadedClass(name);
            if (c != null) {
                return c;
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

    static String describe(Throwable t) {
        Throwable c = t.getCause();
        return t.getClass().getName() + ": " + t.getMessage() + " cause="
                + (c == null ? "null" : c.getClass().getName() + ": " + c.getMessage());
    }

    static void call(String row, Class<?> ref, String method) throws Exception {
        try {
            // A non-null operand: `checkcast` of null resolves nothing.
            Object r = ref.getMethod(method, Object.class).invoke(null, new Object[] {new Object[0]});
            System.out.println(row + "=ok " + r);
        } catch (InvocationTargetException e) {
            System.out.println(row + "=" + describe(e.getCause()));
        }
    }

    public static void main(String[] args) throws Exception {
        String[][] rows = {
            {"ldc-ise", "literal", "ise"},
            {"ldc-cnfe", "literal", "cnfe"},
            {"checkcast-ise", "cast", "ise"},
            {"checkcast-cnfe", "cast", "cnfe"},
        };
        for (String[] row : rows) {
            L l = new L(row[2]);
            Class<?> ref = l.loadClass(P + "Ref");
            call(row[0] + " first", ref, row[1]);
            call(row[0] + " second", ref, row[1]);
            System.out.println(row[0] + " asked=" + l.asked);
        }
        // The application loader has loaded its `$Target` by now (below); a
        // child-first loader's `Target[]` is still an array of ITS `$Target`.
        System.out.println("app target=" + (new Target().getClass().getClassLoader()
                == L5W41ArrayComponentLoaderThrow.class.getClassLoader()));
        L child = new L("child");
        call("child-ldc", child.loadClass(P + "Ref"), "literalLoader");
        System.out.println("child-ldc asked=" + child.asked);
    }

    public static class Target {
    }

    public static class Ref {
        public static Object literal(Object ignored) {
            return Target[].class == null ? "null" : "class";
        }

        public static Object cast(Object o) {
            Target[] t = (Target[]) o;
            return t == null ? "null" : "non-null";
        }

        public static Object literalLoader(Object ignored) {
            ClassLoader l = Target[].class.getComponentType().getClassLoader();
            return l == Ref.class.getClassLoader() ? "own"
                    : l == ClassLoader.getSystemClassLoader() ? "app" : String.valueOf(l);
        }
    }
}
