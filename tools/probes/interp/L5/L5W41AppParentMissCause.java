// Interpreter round i1, wave 41, lane L5 -- a VM-initiated resolution through
// an APPLICATION-parented loader that does not override `loadClass` (its
// `loadClass` is the JDK's, which CratonVM runs as its base-delegation native
// `classloader_real::cl_real_load_class_base`), for a name nobody has: the
// parent misses, the loader's `findClass` throws a `ClassNotFoundException`.
// JVMS §5.3 / HotSpot: the resolution fails with `NoClassDefFoundError:
// <internal name>` whose cause is THAT exception (`resolve_or_fail` wraps the
// pending CNFE), and the error is recorded against the entry (JVMS §5.4.3: the
// second execution fails without asking the loader again).
//
// `$Ref` is defined by the loader from its own class file with `$Gone`
// renamed to `$Nope` (same length), so it references a class that exists
// nowhere; the loader's parent is the application loader.
//
// Rows (a fresh loader for each of `find-new`, `find-static`, `base-new`,
// `global-new`, `global-static-first`):
//   find-new / find-new-again  `new $Nope`, findClass throws CNFE("custom ...")
//   find-static                `invokestatic $Nope.m()` (the owner's door)
//   base-new                   the loader overrides nothing: the JDK
//                              `findClass` throws CNFE(<binary name>)
//   global-new / global-static the same for a JDK-global name the JDK lacks,
//                              `javax/l5w41/MissingJavaxName1` (`$GRef`):
//                              `new`, then `invokestatic` of the same
//                              class entry (rethrows the record)
//   global-static-first        `invokestatic` of that name first: the owner's
//                              ask after its flat route missed
//   ... asked=N                how often findClass saw the missing name
//
// Fix for `docs/internal/fixed-bugs/interpreter-L5-a-vm-initiated-loadclass-exception-is-swallowed-into-a-global-fallback-FIXED-20261005.md`
// (the "base-delegation native's CNFE for an app-parented chain" and the
// `invokestatic` owner's after-miss remainders). Before wave 41 CratonVM
// (`--jdk-only`) dropped the loader's CNFE and failed the resolution with the
// global route's own `NoClassDefFoundError`, with no cause (`cause=null` on
// the failure rows, `asked` unchanged; from the code, not run).
// `--compatible` keeps the old shape by design (every failure row
// `cause=null`).
//
// Positive control: `CRATONVM_DBG=access` prints
// `[ACCESS-DBG] LOADER-THROW PROPAGATE #n java.lang.ClassNotFoundException from L5W41AppParentMissCause$Find.loadClass("...") ...`
// five times (`find-new`, `find-static`, `global-new`,
// `global-static-first` from `$Find`; `base-new` from `$Plain`); the two
// `-again` / `global-static` rows rethrow the record without asking. The
// exit line `[ACCESS-DBG] LOADER census: ...` counts
// `loader-throws-propagated=5`.
//
// Run (no setup):
//   javac -d out L5W41AppParentMissCause.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W41AppParentMissCause
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   find-new=java.lang.NoClassDefFoundError: L5W41AppParentMissCause$Nope cause=java.lang.ClassNotFoundException: custom L5W41AppParentMissCause$Nope
//   find-new-again=java.lang.NoClassDefFoundError: L5W41AppParentMissCause$Nope cause=java.lang.ClassNotFoundException: custom L5W41AppParentMissCause$Nope
//   find-new asked=1
//   find-static=java.lang.NoClassDefFoundError: L5W41AppParentMissCause$Nope cause=java.lang.ClassNotFoundException: custom L5W41AppParentMissCause$Nope
//   find-static asked=1
//   base-new=java.lang.NoClassDefFoundError: L5W41AppParentMissCause$Nope cause=java.lang.ClassNotFoundException: L5W41AppParentMissCause$Nope
//   global-new=java.lang.NoClassDefFoundError: javax/l5w41/MissingJavaxName1 cause=java.lang.ClassNotFoundException: custom javax.l5w41.MissingJavaxName1
//   global-static=java.lang.NoClassDefFoundError: javax/l5w41/MissingJavaxName1 cause=java.lang.ClassNotFoundException: custom javax.l5w41.MissingJavaxName1
//   global asked=1
//   global-static-first=java.lang.NoClassDefFoundError: javax/l5w41/MissingJavaxName1 cause=java.lang.ClassNotFoundException: custom javax.l5w41.MissingJavaxName1
//   global-static-first asked=1

import java.io.IOException;
import java.io.InputStream;
import java.lang.reflect.InvocationTargetException;

public class L5W41AppParentMissCause {
    static final String P = "L5W41AppParentMissCause$";

    /** The JDK-global name `$GRef` references instead of `$GoneJ`. */
    static final String JAVAX = "javax/l5w41/MissingJavaxName1";

    /** `$Ref`'s class file with every `$Gone` renamed `$Nope`. */
    static byte[] refBytes() throws IOException {
        return patched("Ref", "$Gone", "$Nope");
    }

    /** `$GRef`'s class file with every `L5W41AppParentMissCause$GoneJ` renamed {@link #JAVAX}. */
    static byte[] globalRefBytes() throws IOException {
        return patched("GRef", P + "GoneJ", JAVAX);
    }

    static byte[] patched(String nested, String fromText, String toText) throws IOException {
        byte[] b;
        try (InputStream in = ClassLoader.getSystemResourceAsStream(P + nested + ".class")) {
            b = in.readAllBytes();
        }
        byte[] from = fromText.getBytes();
        byte[] to = toText.getBytes();
        if (from.length != to.length) {
            throw new AssertionError(from.length + " != " + to.length);
        }
        for (int i = 0; i + from.length <= b.length; i++) {
            boolean hit = true;
            for (int j = 0; j < from.length && hit; j++) {
                hit = b[i + j] == from[j];
            }
            if (hit) {
                System.arraycopy(to, 0, b, i, to.length);
            }
        }
        return b;
    }

    /** Overrides `findClass` only; the parent is the application loader. */
    static final class Find extends ClassLoader {
        int asked;

        Find() {
            super(L5W41AppParentMissCause.class.getClassLoader());
        }

        Class<?> defineRef() throws IOException {
            byte[] b = refBytes();
            return defineClass(P + "Ref", b, 0, b.length);
        }

        Class<?> defineGlobalRef() throws IOException {
            byte[] b = globalRefBytes();
            return defineClass(P + "GRef", b, 0, b.length);
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            if (name.equals(P + "Nope") || name.equals(JAVAX.replace('/', '.'))) {
                asked++;
            }
            throw new ClassNotFoundException("custom " + name);
        }
    }

    /** Overrides nothing; the parent is the application loader. */
    static final class Plain extends ClassLoader {
        Plain() {
            super(L5W41AppParentMissCause.class.getClassLoader());
        }

        Class<?> defineRef() throws IOException {
            byte[] b = refBytes();
            return defineClass(P + "Ref", b, 0, b.length);
        }
    }

    static String describe(Throwable t) {
        Throwable c = t.getCause();
        return t.getClass().getName() + ": " + t.getMessage() + " cause="
                + (c == null ? "null" : c.getClass().getName() + ": " + c.getMessage());
    }

    static void run(String row, Class<?> ref, String method) throws Exception {
        try {
            ref.getMethod(method).invoke(null);
            System.out.println(row + "=ok");
        } catch (InvocationTargetException e) {
            System.out.println(row + "=" + describe(e.getCause()));
        }
    }

    public static void main(String[] args) throws Exception {
        Find f = new Find();
        Class<?> ref = f.defineRef();
        run("find-new", ref, "make");
        run("find-new-again", ref, "make");
        System.out.println("find-new asked=" + f.asked);

        Find g = new Find();
        run("find-static", g.defineRef(), "call");
        System.out.println("find-static asked=" + g.asked);

        run("base-new", new Plain().defineRef(), "make");

        Find h = new Find();
        Class<?> gref = h.defineGlobalRef();
        run("global-new", gref, "make");
        run("global-static", gref, "call");
        System.out.println("global asked=" + h.asked);

        Find k = new Find();
        run("global-static-first", k.defineGlobalRef(), "call");
        System.out.println("global-static-first asked=" + k.asked);
    }

    public static class GoneJ {
        public static int m() {
            return 2;
        }
    }

    public static class GRef {
        public static Object make() {
            return new GoneJ();
        }

        public static int call() {
            return GoneJ.m();
        }
    }

    public static class Gone {
        public static int m() {
            return 1;
        }
    }

    public static class Ref {
        public static Object make() {
            return new Gone();
        }

        public static int call() {
            return Gone.m();
        }
    }
}
