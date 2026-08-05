import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.net.URL;
import java.net.URLClassLoader;

/**
 * L1 — the four VM-internal loader fields moved out of the loader object.
 *
 * CratonVM used to write `loader_type`, `classes_loaded`, `parallel_capable`
 * and `loader_id` into slots 0/3/4/6 of the ClassLoader object. On a real JDK
 * image those slots are `parent` / `nameAndId` / `parallelLockMap` / `classes`
 * — references, all four — so every write destroyed a JDK field, and every
 * read got a coerced null back. This probe is the behavioural half of that
 * fix: it exercises the natives that wrote and read those four values and
 * prints what they answer.
 *
 * It is a MATRIX probe, meant to be diffed line-for-line against the host
 * JDK. Run it on HotSpot FIRST: HotSpot's output is the contract, not
 * CratonVM's previous output. Every line prints a VALUE, never "ok" — a probe
 * that prints "ok" cannot show which of two runs is wrong.
 *
 * Loader identity is the load-bearing part and is easy to break silently:
 * `get_or_create_app_loader` carries a comment about the outage caused when
 * `X.class.getClassLoader() == ClassLoader.getSystemClassLoader()` stopped
 * holding (Gradle's ClassLoaderVisitor depends on it).
 */
public class L1LoaderIdentityProbe {

    static int lines = 0, failed = 0;

    static void kv(String key, Object value) {
        System.out.println("L1 " + key + "=" + value);
        lines++;
    }

    static void section(String name, Runnable body) {
        try {
            body.run();
        } catch (Throwable t) {
            failed++;
            System.out.println("L1-SECTION-FAILED " + name + ": " + t);
        }
    }

    public static void main(String[] args) {
        section("identity", L1LoaderIdentityProbe::identity);
        section("builtin-chain", L1LoaderIdentityProbe::builtinChain);
        section("ctors", L1LoaderIdentityProbe::ctors);
        section("parallel-capable", L1LoaderIdentityProbe::parallelCapable);
        section("define", L1LoaderIdentityProbe::define);
        section("isolation", L1LoaderIdentityProbe::isolation);
        section("urlclassloader", L1LoaderIdentityProbe::urlClassLoader);
        section("errors", L1LoaderIdentityProbe::errors);
        System.out.println("L1PROBE lines=" + lines + " failedSections=" + failed);
    }

    // ---------------------------------------------------------------- identity

    /** The invariant L1's "done when" names explicitly. */
    static void identity() {
        ClassLoader sys = ClassLoader.getSystemClassLoader();
        kv("sys-null", sys == null);
        kv("probe-loader-is-sys", L1LoaderIdentityProbe.class.getClassLoader() == sys);
        kv("tccl-is-sys", Thread.currentThread().getContextClassLoader() == sys);
        kv("sys-is-stable", ClassLoader.getSystemClassLoader() == sys);
        // A JDK class is bootstrap-defined; its loader is null on HotSpot.
        kv("string-loader-null", String.class.getClassLoader() == null);
        kv("sys-class", sys == null ? "-" : sys.getClass().getName());
    }

    /**
     * `getName()` and the `getParent()` walk. This is the pair that slot 0's
     * corruption reached: `CL_LOADER_TYPE` (our slot 0) is the real `parent`.
     * Tomcat's WebappClassLoaderBase.&lt;init&gt; runs exactly this walk.
     */
    static void builtinChain() {
        ClassLoader sys = ClassLoader.getSystemClassLoader();
        ClassLoader platform = ClassLoader.getPlatformClassLoader();
        kv("sys-name", sys.getName());
        kv("platform-name", platform.getName());
        kv("sys-parent-is-platform", sys.getParent() == platform);
        kv("platform-parent-null", platform.getParent() == null);

        // Bounded on purpose: a corrupted `parent` can produce a cycle, and an
        // unbounded walk then hangs the run — which reads exactly like a clean
        // short one.
        int depth = 0;
        ClassLoader j = sys;
        while (j.getParent() != null && depth < 32) {
            j = j.getParent();
            depth++;
        }
        kv("walk-depth", depth);
        kv("walk-top-is-platform", j == platform);
    }

    // ------------------------------------------------------------------- ctors

    /**
     * The three `ClassLoader` constructors are the four writers L1 gated.
     * `getName()`/`getParent()` are the only user-visible read-back of what
     * they store.
     */
    static void ctors() {
        ClassLoader sys = ClassLoader.getSystemClassLoader();

        ClassLoader named = new ClassLoader("l1-named", sys) { };
        kv("named-getName", named.getName());
        kv("named-parent-is-sys", named.getParent() == sys);

        ClassLoader parented = new ClassLoader(sys) { };
        kv("parented-getName", parented.getName());
        kv("parented-parent-is-sys", parented.getParent() == sys);

        ClassLoader defaulted = new ClassLoader() { };
        kv("default-getName", defaulted.getName());
        kv("default-parent-is-sys", defaulted.getParent() == sys);

        ClassLoader nullParented = new ClassLoader(null) { };
        kv("nullparent-getName", nullParented.getName());
        kv("nullparent-parent-null", nullParented.getParent() == null);

        // Distinct instances stay distinct — the side table is keyed by
        // object, and two loaders must never share an entry.
        ClassLoader a = new ClassLoader("l1-same", sys) { };
        ClassLoader b = new ClassLoader("l1-same", sys) { };
        kv("two-same-named-are-distinct", a != b);
        kv("two-same-named-both-report-name", a.getName() + "/" + b.getName());
    }

    // -------------------------------------------------------- parallel-capable

    /** `isRegisteredAsParallelCapable` is package-private; `registerAsParallelCapable`
     *  is the observable half, and the two must not contradict each other. */
    static void parallelCapable() {
        kv("registered-parallel-capable", ParallelLoader.REGISTERED);
        ClassLoader p = new ParallelLoader();
        kv("parallel-loader-loads-string", loadName(p, "java.lang.String"));
    }

    static class ParallelLoader extends ClassLoader {
        static final boolean REGISTERED;
        static {
            REGISTERED = ClassLoader.registerAsParallelCapable();
        }
        ParallelLoader() {
            super("l1-parallel", ClassLoader.getSystemClassLoader());
        }
    }

    // ------------------------------------------------------------------ define

    /** `defineClass` is where `classes_loaded` was incremented and the
     *  namespace id was read. `getClassLoader()` on the result is the
     *  observable. */
    static void define() {
        byte[] bytes = payloadBytes();
        kv("payload-bytes", bytes.length);

        DefiningLoader dl = new DefiningLoader("l1-definer");
        Class<?> c = dl.defineIt(bytes);
        kv("defined-name", c.getName());
        kv("defined-loader-is-definer", c.getClassLoader() == dl);
        kv("defined-differs-from-classpath-copy", c != L1Payload.class);
        kv("classpath-copy-loader-is-sys",
                L1Payload.class.getClassLoader() == ClassLoader.getSystemClassLoader());

        // Second define of the SAME name through the same loader is a
        // LinkageError on HotSpot — an error shape, not a value.
        String second;
        try {
            dl.defineIt(bytes);
            second = "no-exception";
        } catch (Throwable t) {
            second = t.getClass().getName();
        }
        kv("redefine-same-loader", second);
    }

    /** Two unrelated loaders define the same name: distinct classes, and
     *  neither leaks to the other. This is what a shared namespace id would
     *  break. */
    static void isolation() {
        byte[] bytes = payloadBytes();
        DefiningLoader one = new DefiningLoader("l1-iso-1");
        DefiningLoader two = new DefiningLoader("l1-iso-2");
        Class<?> c1 = one.defineIt(bytes);
        Class<?> c2 = two.defineIt(bytes);
        kv("iso-same-name", c1.getName().equals(c2.getName()));
        kv("iso-distinct-classes", c1 != c2);
        kv("iso-loader-1", c1.getClassLoader() == one);
        kv("iso-loader-2", c2.getClassLoader() == two);
        kv("iso-not-assignable", c1.isAssignableFrom(c2));
    }

    static class DefiningLoader extends ClassLoader {
        DefiningLoader(String name) {
            super(name, ClassLoader.getSystemClassLoader());
        }
        Class<?> defineIt(byte[] b) {
            return defineClass("L1Payload", b, 0, b.length);
        }
    }

    static byte[] payloadBytes() {
        try (InputStream in =
                     L1LoaderIdentityProbe.class.getResourceAsStream("/L1Payload.class")) {
            if (in == null) {
                throw new IllegalStateException("L1Payload.class not on the classpath");
            }
            ByteArrayOutputStream bo = new ByteArrayOutputStream();
            byte[] buf = new byte[4096];
            int n;
            while ((n = in.read(buf)) > 0) {
                bo.write(buf, 0, n);
            }
            return bo.toByteArray();
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
    }

    // --------------------------------------------------------- urlclassloader

    static void urlClassLoader() {
        URL[] empty = new URL[0];
        ClassLoader sys = ClassLoader.getSystemClassLoader();
        try (URLClassLoader u = new URLClassLoader(empty, sys)) {
            kv("ucl-parent-is-sys", u.getParent() == sys);
            kv("ucl-urls", u.getURLs().length);
            kv("ucl-loads-through-parent", loadName(u, "L1Payload"));
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
        try (URLClassLoader named =
                     new URLClassLoader("l1-ucl", empty, sys)) {
            kv("ucl-named-getName", named.getName());
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
    }

    // ------------------------------------------------------------------ errors

    /** The JDK's ERROR behaviour is part of the contract, not an afterthought. */
    static void errors() {
        ClassLoader sys = ClassLoader.getSystemClassLoader();
        kv("missing-through-sys", loadName(sys, "no.such.L1Class"));
        ClassLoader custom = new ClassLoader("l1-err", sys) { };
        kv("missing-through-custom", loadName(custom, "no.such.L1Class"));
        kv("found-through-custom", loadName(custom, "L1Payload"));
        kv("sys-resource-missing", sys.getResource("no/such/l1.txt") == null);
    }

    /** `loaded` or the exception's class name — never a boolean. */
    static String loadName(ClassLoader cl, String name) {
        try {
            Class<?> c = cl.loadClass(name);
            return "loaded:" + (c.getClassLoader() == null ? "bootstrap" : c.getClassLoader().getName());
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }
}
