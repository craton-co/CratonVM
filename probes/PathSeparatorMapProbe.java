// Regression probe for the WebFlux/WebMvc `DefaultPathContainer$DefaultSeparator`
// ClassCastException family.
//
// Spring's `DefaultPathContainer` resolves a path separator like this:
//
//   private static final Map<Character, DefaultSeparator> SEPARATORS =
//       Map.of('/', new DefaultSeparator('/', "%2F"),
//              '.', new DefaultSeparator('.', "%2E"));
//   ...
//   DefaultSeparator sep = SEPARATORS.get(options.separator());   // implicit checkcast
//
// On CratonVM that map is NOT ordinary Java state. `Map.of` allocates a native
// backing `HashMap`, and a `HashMap` whose keys unbox to `Value::Int` — which
// `Character` does — is served by the `hm_int_fast` **collection overlay**: a
// process-global Rust side table keyed by the map's identity hash. The two
// `DefaultSeparator` values are reachable ONLY from that side table; no Java
// field and no card describes the edge, so the collector has to root them
// through `external_roots`.
//
// When that rooting is missed, `get` hands back a dangling `ObjectRef`. A
// reclaimed object's header is zeroed, `ClassId(0)` renders as
// `java/lang/Object`, and the implicit checkcast on the generic `get` reports
//
//   ClassCastException: java.lang.Object cannot be cast to
//                       org.springframework.http.server.DefaultPathContainer$DefaultSeparator
//
// which is exactly the reported signature, thrown from
// `DefaultPathContainer.createFromUrlPath` via `RequestPath.parse` on both the
// WebFlux and the servlet request paths.
//
// The shape that expresses the bug (same reasoning as
// `regression-suite/src/ROverlaySystemGcStress.java`) is an overlay-backed map
// that survives long enough to be PROMOTED, whose promoting cycle is also a
// `System.gc()` — `System.gc()` sets `major_gc_requested()`, which makes
// `native_roots::scan_collection_overlays` skip the unconditional overlay root
// scan and rely on the marker's per-owner walk instead. So the loop below keeps
// every map alive across many explicit collections and re-verifies the OLD ones
// each round; a fresh map that was never promoted cannot express it.
//
// PIN `--Xmx`: the default heap derives from system RAM and decides how many
// collections run at all, so an unpinned run is not a controlled experiment.
//
//   cratonvm --java-home <jdk> --Xmx 256m -cp <out> PathSeparatorMapProbe
//
// Usage: PathSeparatorMapProbe [rounds] [bundles] [threads]
public class PathSeparatorMapProbe {

    static final class Sep {
        final char value;
        final String encoded;

        Sep(char value, String encoded) {
            this.value = value;
            this.encoded = encoded;
        }
    }

    /** One `DefaultPathContainer`-shaped separator table. */
    static final class Bundle {
        final int id;
        final java.util.Map<Character, Sep> separators;

        Bundle(int id) {
            this.id = id;
            this.separators = java.util.Map.of(
                    '/', new Sep('/', "%2F" + id),
                    '.', new Sep('.', "%2E" + id));
        }

        void verify() {
            // The implicit checkcast on the generic `get` is the crash site.
            Sep slash = separators.get('/');
            Sep dot = separators.get('.');
            if (slash == null || dot == null) {
                throw new AssertionError("null separator (bundle " + id + "): slash="
                        + slash + " dot=" + dot);
            }
            if (slash.value != '/' || !("%2F" + id).equals(slash.encoded)) {
                throw new AssertionError("corrupt '/' (bundle " + id + "): value="
                        + slash.value + " encoded=" + slash.encoded);
            }
            if (dot.value != '.' || !("%2E" + id).equals(dot.encoded)) {
                throw new AssertionError("corrupt '.' (bundle " + id + "): value="
                        + dot.value + " encoded=" + dot.encoded);
            }
            if (separators.size() != 2) {
                throw new AssertionError("size " + separators.size() + " != 2 (bundle "
                        + id + ")");
            }
        }
    }

    static volatile Throwable failure;
    static long checks;

    /** Short-lived garbage, so each round actually has something to collect. */
    static Object churn(int round) {
        java.util.List<Object> junk = new java.util.ArrayList<>();
        for (int i = 0; i < 512; i++) {
            junk.add(new byte[128 + (i & 127)]);
            junk.add("junk" + round + "." + i);
        }
        return junk.get(junk.size() - 1);
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 40;
        int bundleCount = args.length > 1 ? Integer.parseInt(args[1]) : 40;
        int threads = args.length > 2 ? Integer.parseInt(args[2]) : 2;

        java.util.List<Bundle> live = new java.util.ArrayList<>();
        Object sink = null;

        for (int round = 0; round < rounds; round++) {
            live.add(new Bundle(round));
            if (live.size() > bundleCount) {
                live.remove(0);
            }
            sink = churn(round);
            System.gc();
            // Re-verify every OLD bundle, not just the fresh one: only a map
            // that was already promoted can express the defect.
            for (Bundle b : live) {
                b.verify();
                checks += 4;
            }
        }

        // Same table, read concurrently while more collections run — the
        // Tomcat/Netty worker-thread shape the field reports came from.
        java.util.List<Bundle> shared = new java.util.ArrayList<>(live);
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            ts[t] = new Thread(() -> {
                try {
                    for (int i = 0; i < 200 && failure == null; i++) {
                        for (Bundle b : shared) {
                            b.verify();
                        }
                        churn(i);
                        if ((i & 15) == 0) {
                            System.gc();
                        }
                    }
                } catch (Throwable e) {
                    failure = e;
                }
            }, "sep-" + t);
            ts[t].start();
        }
        for (Thread t : ts) {
            t.join();
        }

        if (failure != null) {
            System.out.println("FAIL PathSeparatorMapProbe (" + checks + " checks)");
            failure.printStackTrace(System.out);
            System.exit(1);
        }
        System.out.println("PASS PathSeparatorMapProbe (" + checks
                + " checks, " + live.size() + " live bundles, sink=" + (sink != null) + ")");
    }
}
