// Interpreter round i1, wave 14, lane L2 — a compiled call site whose owner
// the JIT SUBSTITUTED for the constant-pool class (here the declaring class of
// a method of a `final` class) must not ask the CALLER's loader for that
// owner at run time: the caller's bytecode never names it
// (docs/internal/fixed-bugs/interpreter-L2-substituted-owner-dispatch-resolves-the-declaring-class-by-name-FIXED-20260925.md).
//
// `L2SubstitutedOwnerCaller` is defined by a logging, child-first loader and
// loops over `fin.len()`, where `Fin` is `final` and `len()` is declared by its
// superclass `Anc` (a `synchronized` method, so the compiled site is neither
// spliced nor bound directly and reaches the dispatch helper). HotSpot resolves
// `Fin` through the caller's loader and finds `Anc.len` in `Fin`'s own
// hierarchy; the caller's loader is never asked for `Anc`.
//
// Compare stdout against HotSpot 25 (`java L2SubstitutedOwnerLoader`). Run
// CratonVM with `--compatible`, with and without `--nojit`. Expected output,
// all modes:
//   total=3000000
//   caller loader asked for the ancestor: false
//
// Before the fix, CratonVM with the JIT on could print `true` once the loop was
// compiled: the dispatch helper resolved the substituted name
// `L2SubstitutedOwnerLoader$Anc` through the caller's loader
// (`resolve_class_loader_aware`), which drives a user-defined loader's
// `loadClass` for a name it has not seen.
import java.io.IOException;
import java.io.InputStream;
import java.lang.reflect.Method;
import java.util.Collections;
import java.util.HashSet;
import java.util.Set;

public class L2SubstitutedOwnerLoader {
    static class Anc {
        public synchronized int len() {
            return 3;
        }
    }

    public static final class Fin extends Anc {}

    static final class LoggingLoader extends ClassLoader {
        final Set<String> asked = Collections.synchronizedSet(new HashSet<>());

        LoggingLoader(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            asked.add(name);
            if (!name.equals("L2SubstitutedOwnerCaller")) {
                return super.loadClass(name, resolve);
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    try (InputStream in = getParent().getResourceAsStream(name + ".class")) {
                        if (in == null) {
                            throw new ClassNotFoundException(name);
                        }
                        byte[] b = in.readAllBytes();
                        c = defineClass(name, b, 0, b.length);
                    } catch (IOException e) {
                        throw new ClassNotFoundException(name, e);
                    }
                }
                return c;
            }
        }
    }

    public static void main(String[] args) throws Exception {
        LoggingLoader loader = new LoggingLoader(L2SubstitutedOwnerLoader.class.getClassLoader());
        Class<?> caller = loader.loadClass("L2SubstitutedOwnerCaller");
        Method run = caller.getMethod("run", int.class);
        run.setAccessible(true);
        long total = 0;
        for (int round = 0; round < 20; round++) {
            total += (Long) run.invoke(null, 50_000);
        }
        System.out.println("total=" + total);
        System.out.println("caller loader asked for the ancestor: "
                + loader.asked.contains("L2SubstitutedOwnerLoader$Anc"));
    }
}

class L2SubstitutedOwnerCaller {
    public static long run(int n) {
        L2SubstitutedOwnerLoader.Fin fin = new L2SubstitutedOwnerLoader.Fin();
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += fin.len();
        }
        return s;
    }
}
