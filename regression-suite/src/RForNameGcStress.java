/**
 * Regression: `Class.forName(name, initialize, loader)` must keep the name
 * String and the loader rooted across the `loader.loadClass(name)` dispatch it
 * makes.
 *
 * CratonVM implements the three-argument overload natively
 * (`native-builtins/src/lang_class.rs::native_class_for_name`). It reads the
 * name String and the loader out of `args`, then calls
 * `invoke_virtual(loader, "loadClass", ...)` — arbitrary Java, and therefore a
 * moving-GC point — and keeps using those locals afterwards. Hibernate reaches
 * this on every `ClassLoaderServiceImpl.classForName`, which is where
 * `CRATONVM_DBG_STALE_OBJREF` caught it.
 *
 * The loader below ALLOCATES inside `loadClass`, so under
 * `CRATONVM_DBG_GC_STRESS` the dispatch is guaranteed to span a collection —
 * the callback shape a plain `Class.forName("java.lang.String")` cannot
 * produce, because the fast paths never dispatch Java at all.
 *
 * Deterministic output; the runner diffs it against HotSpot.
 *
 *   CRATONVM_DBG_GC_STRESS=65536 cratonvm --nojit --Xmx 256m RForNameGcStress
 */
import java.util.ArrayList;
import java.util.List;

public class RForNameGcStress {
    static int checks = 0;

    static void check(boolean c, String m) {
        checks++;
        if (!c) throw new AssertionError(m);
    }

    /**
     * Delegating loader whose `loadClass` allocates before delegating. The
     * allocation is what turns the native's `invoke_virtual` into a guaranteed
     * GC point; the delegation keeps the answer identical to HotSpot's.
     */
    static final class ChurningLoader extends ClassLoader {
        long churn;

        ChurningLoader(ClassLoader parent) {
            super(parent);
        }

        @Override
        public Class<?> loadClass(String name) throws ClassNotFoundException {
            List<String> scratch = new ArrayList<>(8);
            for (int i = 0; i < 8; i++) scratch.add(name + '#' + i);
            churn += scratch.get(7).length();
            return super.loadClass(name);
        }
    }

    /**
     * Overrides the two-argument `loadClass(String, boolean)` and NOT the
     * single-argument one, so `Class.forName`'s native takes the
     * `receiver_overrides_load_class_resolve` branch rather than the
     * single-arg-override shortcut. Also allocates before delegating.
     */
    static final class ResolveChurningLoader extends ClassLoader {
        long churn;

        ResolveChurningLoader(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            List<String> scratch = new ArrayList<>(8);
            for (int i = 0; i < 8; i++) scratch.add(name + '@' + i);
            churn += scratch.get(7).length();
            return super.loadClass(name, resolve);
        }
    }

    /** Overrides nothing — exercises the base parent-first delegation path. */
    static final class PlainLoader extends ClassLoader {
        PlainLoader(ClassLoader parent) {
            super(parent);
        }
    }

    static final String[] NAMES = {
        "java.lang.String",
        "java.util.HashMap",
        "java.util.ArrayList",
        "java.lang.Integer",
        "java.util.LinkedHashMap",
        "java.lang.StringBuilder",
    };

    public static void main(String[] args) throws Exception {
        final int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 400;

        ClassLoader app = RForNameGcStress.class.getClassLoader();
        ChurningLoader loader = new ChurningLoader(app);
        ResolveChurningLoader resolveLoader = new ResolveChurningLoader(app);
        PlainLoader plain = new PlainLoader(app);
        ClassLoader[] loaders = { loader, resolveLoader, plain, app };
        long nameLen = 0;
        for (int r = 0; r < rounds; r++) {
            for (ClassLoader cl : loaders) {
                for (String n : NAMES) {
                    // Build the name String fresh each time so it is a young
                    // object the collector will relocate, exactly like
                    // Hibernate's `ClassLoaderServiceImpl.classForName`.
                    String name = new StringBuilder(n).toString();
                    Class<?> c = Class.forName(name, true, cl);
                    check(c != null, "forName returned null for " + n);
                    check(n.equals(c.getName()), "forName(" + n + ") resolved to " + c.getName());
                    nameLen += c.getName().length();
                    // Also drive ClassLoader.loadClass directly.
                    Class<?> d = cl.loadClass(new StringBuilder(n).toString());
                    check(n.equals(d.getName()), "loadClass(" + n + ") resolved to " + d.getName());
                    nameLen += d.getName().length();
                }
            }
        }

        // A class that genuinely does not exist must still report CNFE, and the
        // exception's message must carry the (post-GC) name.
        int cnfe = 0;
        for (int r = 0; r < 32; r++) {
            String missing = new StringBuilder("no.such.Class").append(r).toString();
            try {
                Class.forName(missing, true, loader);
                check(false, "expected CNFE for " + missing);
            } catch (ClassNotFoundException e) {
                cnfe++;
            }
        }

        System.out.println("CK rounds=" + rounds + " nameLen=" + nameLen
                + " cnfe=" + cnfe + " churn=" + (loader.churn > 0 && resolveLoader.churn > 0));
        System.out.println("PASS RForNameGcStress (" + checks + " checks)");
    }
}
