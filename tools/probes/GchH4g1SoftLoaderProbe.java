import java.io.IOException;
import java.io.InputStream;
import java.lang.ref.SoftReference;
import java.lang.reflect.Method;

/**
 * gch h4/g1 -- a user class loader held ONLY through a SoftReference that the
 * LRU policy keeps (the reference is read every round) must survive every
 * collection with its class, as on HotSpot.
 *
 * {@code gch-h3os-g1-remark-unloads-classes-before-it-resurrects-soft-retained-referents-20261008}:
 * G1's final remark judged class and loader liveness on the raw mark bitmap,
 * where every soft referent slot is hidden, and resurrected the policy-kept soft
 * referents only afterwards. An object reachable only through a kept soft
 * reference therefore survived while its class (and its defining loader) was
 * unloaded. Here that object is the only instance of a class a child-first
 * loader defined, and the loader is reachable only through that class.
 *
 * stdout is deterministic: one line per round, then DONE. Diagnostics on stderr.
 * Args: [rounds] (default 6).
 */
public class GchH4g1SoftLoaderProbe {
    static final String PAYLOAD = "GchH4g1SoftLoaderProbe$Payload";
    static volatile Object sink;

    /** Loaded twice: by the application loader (here) and by {@link ChildFirst}. */
    public static class Payload {
        private final byte[] body = new byte[1024];

        public int value() {
            return 42 + body.length - 1024;
        }
    }

    /** Defines {@link #PAYLOAD} itself; delegates everything else. */
    static final class ChildFirst extends ClassLoader {
        ChildFirst(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!name.equals(PAYLOAD)) {
                return super.loadClass(name, resolve);
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    byte[] b = read(name);
                    c = defineClass(name, b, 0, b.length);
                }
                if (resolve) {
                    resolveClass(c);
                }
                return c;
            }
        }
    }

    static byte[] read(String name) throws ClassNotFoundException {
        String res = name.replace('.', '/') + ".class";
        try (InputStream in = GchH4g1SoftLoaderProbe.class.getClassLoader().getResourceAsStream(res)) {
            if (in == null) {
                throw new ClassNotFoundException(name);
            }
            return in.readAllBytes();
        } catch (IOException e) {
            throw new ClassNotFoundException(name, e);
        }
    }

    /** The loader, its class and the instance are reachable only through the result. */
    static SoftReference<Object> make() throws Exception {
        ClassLoader loader = new ChildFirst(GchH4g1SoftLoaderProbe.class.getClassLoader());
        Class<?> c = Class.forName(PAYLOAD, true, loader);
        return new SoftReference<>(c.getConstructor().newInstance());
    }

    /** Short-lived garbage, so the payload is promoted out of the young generation. */
    static void churn() {
        for (int i = 0; i < 4096; i++) {
            sink = new byte[1024];
        }
        sink = null;
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 6;
        SoftReference<Object> ref = make();
        for (int r = 1; r <= rounds; r++) {
            churn();
            long t0 = System.nanoTime();
            System.gc();
            System.err.println("[probe] round=" + r + " gc_ms=" + (System.nanoTime() - t0) / 1_000_000);
            Object o = ref.get(); // a read every round: the LRU policy keeps it
            if (o == null) {
                System.out.println("round " + r + " cleared");
                continue;
            }
            Class<?> c = o.getClass();
            boolean childLoaded = c.getClassLoader() instanceof ChildFirst;
            boolean distinct = c != Payload.class;
            Method m = c.getMethod("value");
            int v = (Integer) m.invoke(o);
            boolean same = Class.forName(PAYLOAD, false, c.getClassLoader()) == c;
            System.out.println("round " + r + " kept=true value=" + v + " childLoaded=" + childLoaded
                    + " distinct=" + distinct + " sameClass=" + same);
            o = null;
            c = null;
            m = null;
        }
        System.out.println("DONE");
    }
}
