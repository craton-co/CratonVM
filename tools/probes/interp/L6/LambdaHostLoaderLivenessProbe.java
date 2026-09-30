/*
 * Interpreter round i1 wave 8, lane L6: a live lambda instance keeps its host
 * class's loader alive.
 *
 * HotSpot defines a lambda's spun class in its host's loader, so a reachable
 * lambda keeps that loader -- and the host class and its implementation
 * method -- alive (JLS 12.7). Before wave 8 a CratonVM lambda proxy had no
 * instance->loader edge, so a lambda that captures nothing of its loader
 * (`() -> ...`, or one capturing only a String), stored where only JDK code
 * reaches it, outlived its loader: the host could unload and the next call
 * failed to resolve its implementation method
 * (docs/internal/fixed-bugs/interpreter-L6-lambda-proxy-instance-does-not-keep-its-host-loader-alive-FIXED-20260924.md).
 *
 * Self-contained: the nested class `Host` is re-defined from its own class
 * file bytes in a fresh child-first loader, its lambdas are stored in a
 * static list, every other reference to the loader is dropped, and the
 * collector runs a few times. CratonVM needs `CRATONVM_LOADER_UNLOAD` on (its
 * default).
 *
 * Stdout is deterministic. HotSpot 25 prints exactly:
 *
 *   hostLoaderIsChild=true
 *   loaderAlive=true
 *   runnable=host
 *   supplier=host:x
 *   hostAlive=true
 *
 * Informational only, on stderr: whether a loader whose lambdas were all
 * dropped was collected (HotSpot normally says `true`; it depends on the
 * collector, so it is not part of the compared output).
 */
import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;
import java.util.function.Supplier;

public class LambdaHostLoaderLivenessProbe {
    static final String HOST = "LambdaHostLoaderLivenessProbe$Host";

    /** Only reachable from this list once `stash` returns. */
    static final List<Object> KEEP = new ArrayList<>();

    public static class Host {
        public static Runnable r() {
            return () -> System.out.println("runnable=host");
        }

        public static Supplier<String> s(String x) {
            return () -> "host:" + x;
        }
    }

    static final class ChildFirst extends ClassLoader {
        private final byte[] hostBytes;

        ChildFirst(byte[] hostBytes) {
            super(LambdaHostLoaderLivenessProbe.class.getClassLoader());
            this.hostBytes = hostBytes;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                if (name.equals(HOST)) {
                    Class<?> c = findLoadedClass(name);
                    if (c == null) {
                        c = defineClass(name, hostBytes, 0, hostBytes.length);
                    }
                    return c;
                }
                return super.loadClass(name, resolve);
            }
        }
    }

    static byte[] hostBytes() throws Exception {
        try (InputStream in =
                LambdaHostLoaderLivenessProbe.class.getResourceAsStream(HOST + ".class")) {
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            byte[] buf = new byte[4096];
            int n;
            while ((n = in.read(buf)) > 0) {
                out.write(buf, 0, n);
            }
            return out.toByteArray();
        }
    }

    /** Defines a Host copy, stores its lambdas when `keep`, and returns only weak refs. */
    static WeakReference<?>[] stash(byte[] bytes, boolean keep) throws Exception {
        ClassLoader loader = new ChildFirst(bytes);
        Class<?> host = Class.forName(HOST, true, loader);
        if (keep) {
            System.out.println("hostLoaderIsChild=" + (host.getClassLoader() == loader));
            KEEP.add(host.getMethod("r").invoke(null));
            KEEP.add(host.getMethod("s", String.class).invoke(null, "x"));
        } else {
            ((Runnable) host.getMethod("r").invoke(null)).getClass();
        }
        return new WeakReference<?>[] {new WeakReference<>(loader), new WeakReference<>(host)};
    }

    public static void main(String[] args) throws Exception {
        byte[] bytes = hostBytes();
        WeakReference<?>[] kept = stash(bytes, true);
        WeakReference<?>[] dropped = stash(bytes, false);
        for (int i = 0; i < 5; i++) {
            System.gc();
            Thread.sleep(20);
        }
        System.out.println("loaderAlive=" + (kept[0].get() != null));
        ((Runnable) KEEP.get(0)).run();
        @SuppressWarnings("unchecked")
        Supplier<String> s = (Supplier<String>) KEEP.get(1);
        System.out.println("supplier=" + s.get());
        System.out.println("hostAlive=" + (kept[1].get() != null));
        System.err.println("droppedLoaderCollected=" + (dropped[0].get() == null));
    }
}
