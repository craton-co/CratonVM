// Interpreter round i1, wave 25, lane L5 — Bench: `getfield` / `putfield` /
// `getstatic` / `putstatic` sites in code defined by a USER class loader.
//
// Until wave 25 the per-thread field-site table admitted a LOADER-SENSITIVE
// site (a referencing class defined by a user loader) only under the opt-in
// `CRATONVM_JIT=field-site-cache-loader`, so each execution of such a site
// re-ran `resolve_field_ref_loader_aware` (two `class_manager` reads, a
// `resolution_cache` read, the loader-local lookup), and `field_fast`'s
// quickened sites, which admit only what `field_sites` holds, never served
// one. Wave 25 made that arm default-on (its correctness argument is in
// `interpreter-L4-proposal-static-field-quickening-FIXED-20260930.md`, wave 16); it is wave
// 24's `site_fill_admitted` rule for `new` / cast sites applied to fields.
//
// The same `Work` class runs twice: as defined by the application loader
// (row `app`) and as defined by a fresh URLClassLoader whose parent is the
// platform loader (row `loader`, a loader namespace). Rows are interleaved and
// repeated; each prints the median ns per loop iteration.
//
// Run (no setup):
//   javac -d out L5W25LoaderFieldSiteBench.java
//   cratonvm --java-home <jdk25> --nojit -cp out L5W25LoaderFieldSiteBench
//   cratonvm --java-home <jdk25> --nojit --compatible -cp out L5W25LoaderFieldSiteBench
//
// Compare: the `loader/app` ratio. Expected direction: with the arm off
// (`CRATONVM_JIT_FIELD_SITE_CACHE_LOADER=0`, the pre-wave-25 default) `loader`
// is several times `app` under --nojit (the wave-16 per-opcode table: about
// 550-720 ns vs 250 ns for a getfield); with it on (unset, the wave-25
// default) close to 1. A/B on one binary by toggling that variable only;
// `CRATONVM_DBG=field-site`'s `field: ... reject_loader=` falls to about 0
// with the arm on, and `fast-field: get hit=` rises.
//
// Correctness lines (compare verbatim; HotSpot 25):
//   checksum app: 45001200000
//   checksum loader: 45001200000
//   loader class distinct: true
//   loader defines Holder: true
// followed by three `timing` lines (HotSpot -Xint on the lane's laptop: app
// 44.7, loader 44.6 ns/iter, ratio 1.00).
public class L5W25LoaderFieldSiteBench {
    public static final class Holder {
        public static long total;
        public static int step = 3;
    }

    public static final class Work implements java.util.function.IntToLongFunction {
        public long acc;
        public int k = 1;

        public Work() {
        }

        @Override
        public long applyAsLong(int n) {
            acc = 0;
            Holder.total = 0;
            for (int i = 0; i < n; i++) {
                acc += i + k;
                Holder.total += Holder.step;
                k = (k & 1) + 1;
            }
            return acc + Holder.total;
        }
    }

    static final int N = 20000;
    static final int REPS = 15;

    static java.util.function.IntToLongFunction make(ClassLoader loader) throws Exception {
        Class<?> c = Class.forName("L5W25LoaderFieldSiteBench$Work", true, loader);
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

        java.util.function.IntToLongFunction app =
                make(L5W25LoaderFieldSiteBench.class.getClassLoader());
        java.util.function.IntToLongFunction loaded = make(child);

        System.out.println("checksum app: " + app.applyAsLong(N * 15));
        System.out.println("checksum loader: " + loaded.applyAsLong(N * 15));
        System.out.println("loader class distinct: " + (loaded.getClass() != app.getClass()));
        Class<?> holderInChild = Class.forName("L5W25LoaderFieldSiteBench$Holder", false, child);
        System.out.println("loader defines Holder: " + (holderInChild.getClassLoader() == child));

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
