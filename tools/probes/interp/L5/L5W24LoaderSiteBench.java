// Interpreter round i1, wave 24, lane L5 — Bench: `new` / `checkcast` /
// `instanceof` / `anewarray` sites in code defined by a USER class loader.
//
// Before wave 24 the interpreter's per-thread site tables refused every site
// of a referencing class with a loader namespace
// (`opcodes::referencing_class_has_loader_namespace`), so each execution of
// such a site re-ran `resolve_class_loader_aware` (two to four
// `class_manager` reads and the initiating-memo probe) plus the §5.4.4 access
// check. Wave 24 admits a namespaced site when its answer is a JDK-global
// name or the one the loader itself recorded (`opcodes::site_fill_admitted`;
// stage 1 of `i1-L2-proposal-per-class-resolved-constant-pool`).
//
// The same `Work` class runs twice: as defined by the application loader
// (row `app`, always cached) and as defined by a fresh URLClassLoader whose
// parent is the platform loader (row `loader`, a loader namespace). Rows are
// interleaved and repeated; each prints the median ns per loop iteration.
//
// Run (no setup):
//   javac -d out L5W24LoaderSiteBench.java
//   cratonvm --java-home <jdk25> --nojit -cp out L5W24LoaderSiteBench
//   cratonvm --java-home <jdk25> --nojit --compatible -cp out L5W24LoaderSiteBench
//   (optionally without --nojit; the compiled tier resolves these sites
//   through its own helpers, so the interpreter rows are the ones this is for)
//
// Compare: the `loader/app` ratio. Expected direction: before wave 24
// `loader` is several times `app` under --nojit; after, close to 1. A/B the
// fix itself on one binary with CRATONVM_JIT_NO_CAST_SITE_CACHE=1
// CRATONVM_JIT_NO_NEW_SITE_CACHE=1, which turns both tables off for both rows
// (the `loader` row then pays what it paid before wave 24 for every site).
//
// Correctness lines (compare verbatim; HotSpot 25):
//   checksum app: 5000150000
//   checksum loader: 5000150000
//   loader class distinct: true
//   loader defines Item: true
// followed by three `timing` lines (HotSpot -Xint on the lane's laptop:
// app 1511, loader 1349 ns/iter, ratio 0.89 — HotSpot's resolved constant
// pool does not care which loader defined the class).
public class L5W24LoaderSiteBench {
    public static final class Item {
        public final int v;

        public Item(int v) {
            this.v = v;
        }
    }

    public static final class Work implements java.util.function.IntToLongFunction {
        public Work() {
        }

        @Override
        public long applyAsLong(int n) {
            long sum = 0;
            for (int i = 0; i < n; i++) {
                Object o = new Item(i);
                if (o instanceof Item) {
                    sum += ((Item) o).v;
                }
                Item[] a = new Item[1];
                a[0] = (Item) o;
                sum += a.length;
                Object s = new StringBuilder(0);
                if (s instanceof CharSequence) {
                    sum += 1;
                }
            }
            return sum;
        }
    }

    static final int N = 20000;
    static final int REPS = 15;

    static java.util.function.IntToLongFunction make(ClassLoader loader) throws Exception {
        Class<?> c = Class.forName("L5W24LoaderSiteBench$Work", true, loader);
        return (java.util.function.IntToLongFunction) c.getConstructor().newInstance();
    }

    static long median(long[] xs) {
        long[] s = xs.clone();
        java.util.Arrays.sort(s);
        return s[s.length / 2];
    }

    public static void main(String[] args) throws Exception {
        String[] parts = System.getProperty("java.class.path").split(java.io.File.pathSeparator);
        java.net.URL[] urls = new java.net.URL[parts.length];
        for (int i = 0; i < parts.length; i++) {
            urls[i] = new java.io.File(parts[i]).toURI().toURL();
        }
        java.net.URLClassLoader child =
                new java.net.URLClassLoader(urls, ClassLoader.getPlatformClassLoader());

        java.util.function.IntToLongFunction app = make(L5W24LoaderSiteBench.class.getClassLoader());
        java.util.function.IntToLongFunction loaded = make(child);

        System.out.println("checksum app: " + app.applyAsLong(N * 5));
        System.out.println("checksum loader: " + loaded.applyAsLong(N * 5));
        System.out.println("loader class distinct: " + (loaded.getClass() != app.getClass()));
        Class<?> itemInChild = Class.forName("L5W24LoaderSiteBench$Item", false, child);
        System.out.println("loader defines Item: " + (itemInChild.getClassLoader() == child));

        long[] appNs = new long[REPS];
        long[] loaderNs = new long[REPS];
        long sink = 0;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            sink += app.applyAsLong(N);
            long t1 = System.nanoTime();
            sink += loaded.applyAsLong(N);
            long t2 = System.nanoTime();
            appNs[r] = t1 - t0;
            loaderNs[r] = t2 - t1;
        }
        long a = median(appNs);
        long l = median(loaderNs);
        java.util.Locale root = java.util.Locale.ROOT;
        System.out.println("timing app: " + String.format(root, "%.1f", (double) a / N) + " ns/iter");
        System.out.println("timing loader: " + String.format(root, "%.1f", (double) l / N) + " ns/iter");
        System.out.println("timing loader/app: " + String.format(root, "%.2f", (double) l / Math.max(1, a)));
        if (sink == 42) {
            System.out.println("unreachable");
        }
    }
}
