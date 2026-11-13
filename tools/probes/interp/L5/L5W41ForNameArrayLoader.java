// Interpreter round i1, wave 41, lane L5 -- `Class.forName("[Lp.X;", init,
// loader)` with a user-defined loader: JVMS §5.3.3 / HotSpot
// (`SystemDictionary::resolve_or_null` on an array name resolves the ELEMENT
// through `loader`, then makes the array of that element's defining loader).
// So a child-first loader's `X[]` is an array of ITS `X`, and the loader's
// refusal is `forName`'s outcome.
//
// Rows (a fresh loader each; the application loader has loaded its own
// `$Target` before the first row):
//   child / child-2d  a loader that defines its own `$Target` when asked:
//                     whose `$Target` the array's element is
//   parent            a loader that delegates `$Target` to its parent
//   ise / cnfe / null the loader throws `IllegalStateException`, throws
//                     `ClassNotFoundException("refused ...")`, returns null
//   ... asked=N       how often the loader saw `$Target`
//
// Before wave 41 CratonVM (from the code; `lang_class.rs`
// `native_class_for_name`'s array arm) answered every array name from the
// global store, ignoring the loader: every row printed `app asked=0`.
// Under `--compatible` that is unchanged by design.
//
// Run (no setup):
//   javac -d out L5W41ForNameArrayLoader.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W41ForNameArrayLoader
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   app target=true
//   child=own asked=1
//   child-2d=own asked=1
//   parent=app asked=1
//   ise=java.lang.IllegalStateException: refused L5W41ForNameArrayLoader$Target asked=1
//   cnfe=java.lang.ClassNotFoundException: refused L5W41ForNameArrayLoader$Target asked=1
//   null=java.lang.ClassNotFoundException: [LL5W41ForNameArrayLoader$Target; asked=1
//
// Positive control: the `asked=1` rows themselves (the loader is only asked
// through the new arm, `lang_class.rs` `for_name_array_through_loader`).

import java.io.IOException;
import java.io.InputStream;

public class L5W41ForNameArrayLoader {
    static final String P = "L5W41ForNameArrayLoader$";

    static final class L extends ClassLoader {
        final String mode;
        int asked;

        L(String mode) {
            super(L5W41ForNameArrayLoader.class.getClassLoader());
            this.mode = mode;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!name.equals(P + "Target")) {
                return super.loadClass(name, resolve);
            }
            asked++;
            switch (mode) {
                case "ise":
                    throw new IllegalStateException("refused " + name);
                case "cnfe":
                    throw new ClassNotFoundException("refused " + name);
                case "null":
                    return null;
                case "parent":
                    return super.loadClass(name, resolve);
                default:
                    break;
            }
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

    static String whose(Class<?> array, ClassLoader l) {
        Class<?> e = array;
        while (e.isArray()) {
            e = e.getComponentType();
        }
        ClassLoader el = e.getClassLoader();
        return el == l ? "own" : el == ClassLoader.getSystemClassLoader() ? "app" : String.valueOf(el);
    }

    static void row(String row, String mode, String name) {
        L l = new L(mode);
        String out;
        try {
            out = whose(Class.forName(name, false, l), l);
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage()
                    + (t.getCause() == null ? "" : " cause=" + t.getCause().getClass().getName());
        }
        System.out.println(row + "=" + out + " asked=" + l.asked);
    }

    public static void main(String[] args) {
        System.out.println("app target=" + (new Target().getClass().getClassLoader()
                == ClassLoader.getSystemClassLoader()));
        String one = "[L" + P + "Target;";
        row("child", "child", one);
        row("child-2d", "child", "[" + one);
        row("parent", "parent", one);
        row("ise", "ise", one);
        row("cnfe", "cnfe", one);
        row("null", "null", one);
    }

    public static class Target {
    }
}
