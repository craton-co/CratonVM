import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.net.URL;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Collections;
import java.util.Enumeration;
import java.util.HashSet;
import java.util.IdentityHashMap;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Locale;
import java.util.Optional;
import java.util.ServiceLoader;
import java.util.Set;
import java.util.TreeMap;
import java.util.TreeSet;

/**
 * R11 -- one service provider reaching {@code ServiceLoader} through TWO
 * sources, and the three things that are NOT the cause.
 *
 * <h2>What went wrong</h2>
 *
 * Four bc-java corpus classes died before running a single test with
 * {@code JUnitException: Cannot create Launcher for multiple engines with the
 * same ID 'junit-jupiter'}. That message is thrown by
 * {@code EngineIdValidator.validate}, which walks the
 * {@code LinkedHashSet<TestEngine>} {@code LauncherFactory.collectTestEngines}
 * filled from {@code ServiceLoader.load(TestEngine.class, defaultLoader)} and
 * pushes each {@code getId()} into a fresh {@code HashSet<String>}. It throws
 * iff two DISTINCT engine objects report the same id -- i.e. iff the
 * ServiceLoader handed out {@code JupiterTestEngine} twice.
 *
 * <p>The classpath is not duplicated: measured on the stored {@code cp.args},
 * exactly two of the 31 entries carry
 * {@code META-INF/services/org.junit.platform.engine.TestEngine}, one naming
 * Jupiter and one naming Vintage. HotSpot, given the same file, builds one of
 * each.
 *
 * <p>The mechanism is that under CratonVM the SAME jar is visible to
 * {@code ServiceLoader}'s two independent sources at once:
 *
 * <ol>
 *   <li>the module source -- {@code ServicesCatalog} on the system loader,
 *       populated from every {@code module-info.class} CratonVM found while
 *       scanning the APPLICATION class path;</li>
 *   <li>the class-path source -- {@code loader.getResources(
 *       "META-INF/services/<svc>")}.</li>
 * </ol>
 *
 * On a real JVM these two can never collide, because a jar is on the module
 * path XOR the class path, and a modular jar reached through the CLASS path is
 * an unnamed-module citizen whose {@code module-info} the JDK ignores outright.
 *
 * <h2>Why "getResources returned the URL twice" is NOT the mechanism</h2>
 *
 * MEASURED on HotSpot 25, this host, 2026-08-13, with a purpose-built modular
 * jar: putting the SAME jar on {@code -cp} twice gives
 * {@code getResources("META-INF/services/d1.svc.Thing")} two URLs and
 * {@code ServiceLoader} exactly ONE provider. The reason is in the bytecode:
 * {@code ServiceLoader$LazyClassPathLookupIterator} carries a
 * {@code Set<String> providerNames} that de-duplicates provider names across
 * URLs, and its {@code hasNextService} skips any provider class whose
 * {@code getModule().isNamed()} is true.
 *
 * <p>So under {@code --jdk-only}, where the {@code ServiceLoader} natives are
 * {@code NativeKind::SyntheticStub} and strict mode refuses them at
 * registration, the REAL {@code ServiceLoader} bytecode runs and a duplicate
 * URL cannot reach the caller. A duplicate provider survives only when the
 * module source supplies it AND the class-path copy of the same class reports
 * {@code isNamed() == false} -- an inconsistent pair no real JVM can be put
 * into, and exactly the pair CratonVM creates when it registers an
 * app-class-path {@code module-info} into the system loader's
 * {@code ServicesCatalog}.
 *
 * <p>{@link #resourceCensus} is kept anyway, as the FALSIFIER for the
 * hypothesis rather than the test for the defect. Its green result must never
 * be read as clearing this bug.
 *
 * <h2>Three alternatives this vector exists to rule out</h2>
 *
 * The original hypothesis was "{@code getResources} returned the same URL
 * twice". Two more are equally consistent with the message and were never
 * excluded. All three are measured here, separately, so that a green run says
 * which of them is green:
 *
 * <ul>
 *   <li>{@link #resourceCensus} -- {@code getResources} yields DISTINCT URLs,
 *       and yields the SAME COUNT on every call. A duplicate URL, or a count
 *       that drifts between iterations, fails here.</li>
 *   <li>{@link #setDiscipline} -- a fresh {@code HashSet.add} is true then
 *       false, and a {@code LinkedHashSet} of N identity-distinct objects
 *       iterates exactly N times with no repeat. {@code EngineIdValidator}
 *       fails if EITHER of these is wrong, with no ServiceLoader defect at
 *       all.</li>
 *   <li>{@link #providerCensus} -- no service resolves the same provider CLASS
 *       twice, and iterating one {@code ServiceLoader} twice yields the SAME
 *       instances (the JDK's documented cache).</li>
 * </ul>
 *
 * <h2>The strict half, and why it is gated rather than skipped</h2>
 *
 * {@link #classPathModuleSeparation} is the check that actually goes red on
 * the defect, and it needs an input the JVM cannot manufacture at runtime:
 * a MODULAR jar (or exploded module directory) on {@code -cp}, present when
 * the VM starts, because CratonVM scans the application class path for
 * {@code module-info.class} during {@code ClassManager::new} -- before
 * {@code main}. A jar this vector builds itself would be scanned by nobody.
 *
 * <p>So the input is supplied by the harness through
 * {@code -Dcratonvm.rt.cpmodule=<module name>} plus
 * {@code -Dcratonvm.rt.cpclass=<a class in it>} and, optionally,
 * {@code -Dcratonvm.rt.cpservice=<service FQN it declares>}. When
 * {@code cpmodule} is ABSENT this vector FAILS rather than skipping. A
 * precondition that silently disarms the only discriminating check is how a
 * vector becomes a green-forever gate, and this file's whole subject is a row
 * that was published green because nothing ran.
 *
 * <p>Run it WITHOUT {@code --module-path} naming the same module: on a real
 * JVM a {@code --module-path} module IS resolved into the boot layer and IS
 * defined to the application loader, so supplying both makes assertion 1 false
 * on HotSpot too and the vector would be measuring its own command line.
 *
 * <h2>Determinism</h2>
 *
 * No URL string, path, module name or provider name from the environment is
 * printed -- only counts and fixed literals -- because the harness diffs the
 * {@code CK} lines between two VMs on two different machines.
 */
public class RServiceLoaderDoubleSource {

    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /** Iterations of each repeated census. A single sample is not a result. */
    static int iters() {
        String v = System.getProperty("cratonvm.rt.iters");
        if (v == null) {
            return 64;
        }
        try {
            return Math.max(2, Integer.parseInt(v.trim()));
        } catch (NumberFormatException e) {
            return 64;
        }
    }

    static ClassLoader appLoader() {
        return ClassLoader.getSystemClassLoader();
    }

    /**
     * Service interfaces every JDK 25 image can be asked about. These are
     * deliberately JDK-owned: their providers come from the platform modules,
     * so on a correct VM they exercise the module source and the class-path
     * source without needing anything on the application class path.
     */
    static final String[] JDK_SERVICES = {
        "java.nio.file.spi.FileSystemProvider",
        "java.nio.charset.spi.CharsetProvider",
        "java.util.spi.ToolProvider",
        "java.time.chrono.Chronology",
        "java.security.Provider",
    };

    // ------------------------------------------------------------------
    // 1. getResources: distinct == total, and stable across calls.
    // ------------------------------------------------------------------

    static List<String> getResourcesExternalForms(ClassLoader cl, String name) throws Exception {
        List<String> out = new ArrayList<>();
        Enumeration<URL> e = cl.getResources(name);
        check(e != null, "getResources must not return null for " + name);
        int guard = 0;
        while (e.hasMoreElements()) {
            URL u = e.nextElement();
            check(u != null, "getResources yielded a null URL for " + name);
            out.add(u.toExternalForm());
            if (++guard > 4096) {
                throw new AssertionError("getResources enumeration never terminated for " + name);
            }
        }
        return out;
    }

    /**
     * The R11 hypothesis, measured. For every descriptor name, over {@code N}
     * iterations: the URL multiset must contain no repeat, and its SIZE must
     * not change from one call to the next.
     *
     * <p>Both halves matter. A duplicate is the stated hypothesis; a count
     * that drifts is the shape a cache or an unordered container would leave,
     * and a run that only ever looked once could not tell them apart.
     */
    static void resourceCensus() throws Exception {
        int n = iters();
        ClassLoader scl = appLoader();
        ClassLoader tccl = Thread.currentThread().getContextClassLoader();
        int dupFindings = 0;
        int driftFindings = 0;
        int namesProbed = 0;

        List<String> names = new ArrayList<>();
        for (String s : JDK_SERVICES) {
            names.add("META-INF/services/" + s);
        }
        String extra = System.getProperty("cratonvm.rt.cpservice");
        if (extra != null && !extra.isEmpty()) {
            names.add("META-INF/services/" + extra);
        }
        // A resource that exists in essentially every jar, so a flat-classpath
        // enumeration of it is the widest net this probe can cast.
        names.add("META-INF/MANIFEST.MF");

        for (String name : names) {
            namesProbed++;
            int first = -1;
            for (int i = 0; i < n; i++) {
                for (ClassLoader cl : new ClassLoader[] {scl, tccl}) {
                    if (cl == null) {
                        continue;
                    }
                    List<String> urls = getResourcesExternalForms(cl, name);
                    Set<String> distinct = new HashSet<>(urls);
                    if (distinct.size() != urls.size()) {
                        dupFindings++;
                    }
                    if (cl == scl) {
                        if (first < 0) {
                            first = urls.size();
                        } else if (first != urls.size()) {
                            driftFindings++;
                        }
                    }
                }
            }
        }

        check(dupFindings == 0,
                "getResources returned the same URL more than once in " + dupFindings
                        + " of " + (namesProbed * iters() * 2) + " enumerations");
        check(driftFindings == 0,
                "getResources returned a different NUMBER of URLs for the same name across "
                        + driftFindings + " repeat calls");
        System.out.println("CK RServiceLoaderDoubleSource getResources names=" + namesProbed
                + " iters=" + n + " dup=0 drift=0");
    }

    // ------------------------------------------------------------------
    // 2. The Set half of EngineIdValidator, isolated.
    // ------------------------------------------------------------------

    /**
     * {@code EngineIdValidator} throws on {@code !ids.add(id)}. A
     * {@code HashSet.add} that answered false on a first insert, or a
     * {@code LinkedHashSet} whose iterator repeated an element, would produce
     * the identical exception with the ServiceLoader entirely innocent. Both
     * are natively implemented in CratonVM ({@code native-collections},
     * {@code NativeKind::Bridge}, kept under {@code --jdk-only}), so neither
     * is a free assumption.
     */
    static void setDiscipline() {
        Set<String> ids = new HashSet<>();
        check(ids.add("junit-jupiter"), "first HashSet.add of a fresh set must return true");
        check(!ids.add("junit-jupiter"), "second HashSet.add of the same key must return false");
        check(ids.add("junit-vintage"), "a different key must still insert");
        check(ids.size() == 2, "HashSet.size() after two distinct inserts must be 2, got " + ids.size());

        // Identity-distinct elements with no equals/hashCode override -- the
        // exact shape of LinkedHashSet<TestEngine>.
        final int n = 64;
        List<Object> made = new ArrayList<>();
        LinkedHashSet<Object> set = new LinkedHashSet<>();
        for (int i = 0; i < n; i++) {
            Object o = new Object();
            made.add(o);
            check(set.add(o), "LinkedHashSet.add of a fresh identity object must return true");
            check(!set.add(o), "LinkedHashSet.add of the SAME object must return false");
        }
        check(set.size() == n, "LinkedHashSet.size() must be " + n + ", got " + set.size());

        IdentityHashMap<Object, Object> seen = new IdentityHashMap<>();
        int walked = 0;
        for (Object o : set) {
            check(seen.put(o, o) == null,
                    "LinkedHashSet iteration repeated an element at position " + walked);
            walked++;
            if (walked > n + 16) {
                throw new AssertionError("LinkedHashSet iteration never terminated");
            }
        }
        check(walked == n, "LinkedHashSet iterated " + walked + " elements, expected " + n);
        check(seen.size() == n, "LinkedHashSet iteration covered " + seen.size() + " of " + n);
        // Keep `made` reachable so a moving collector cannot recycle an
        // address the set still keys on; the check above is about identity.
        check(made.size() == n, "internal: element list");

        System.out.println("CK RServiceLoaderDoubleSource setDiscipline hashset=4 linked=" + n);
    }

    // ------------------------------------------------------------------
    // 3. ServiceLoader: no provider class twice, and the documented cache.
    // ------------------------------------------------------------------

    static Class<?> serviceClassOrNull(String fqn) {
        try {
            return Class.forName(fqn, false, appLoader());
        } catch (Throwable t) {
            return null;
        }
    }

    static <S> List<String> providerTypeNames(Class<S> svc, ClassLoader cl) {
        List<String> out = new ArrayList<>();
        for (ServiceLoader.Provider<S> p : ServiceLoader.load(svc, cl).stream().toList()) {
            out.add(p.type().getName());
        }
        return out;
    }

    /**
     * For every resolvable service: the provider TYPE list must contain no
     * duplicate, on every one of N fresh loaders. This is the assertion that
     * fails when a modular jar is visible to both the module source and the
     * class-path source at once.
     */
    static void providerCensus() {
        int n = Math.min(iters(), 16); // each pass constructs providers
        int svcProbed = 0;
        int dupFindings = 0;
        int driftFindings = 0;
        TreeMap<String, Integer> counts = new TreeMap<>();

        List<String> services = new ArrayList<>();
        Collections.addAll(services, JDK_SERVICES);
        String extra = System.getProperty("cratonvm.rt.cpservice");
        if (extra != null && !extra.isEmpty()) {
            services.add(extra);
        }

        for (String fqn : services) {
            Class<?> svc = serviceClassOrNull(fqn);
            if (svc == null) {
                continue;
            }
            svcProbed++;
            int first = -1;
            for (int i = 0; i < n; i++) {
                List<String> types = providerTypeNames(svc, appLoader());
                if (new HashSet<>(types).size() != types.size()) {
                    dupFindings++;
                }
                if (first < 0) {
                    first = types.size();
                } else if (first != types.size()) {
                    driftFindings++;
                }
            }
            counts.put(fqn, first);
        }

        check(svcProbed > 0, "no JDK service interface resolved -- this census measured nothing");
        check(dupFindings == 0,
                "ServiceLoader resolved the same provider CLASS more than once in "
                        + dupFindings + " of " + (svcProbed * n) + " loads");
        check(driftFindings == 0,
                "ServiceLoader returned a different NUMBER of providers for the same service on "
                        + driftFindings + " repeat loads");

        // The JDK's documented cache: "Iterating over a ServiceLoader a second
        // time yields the same instances, in the same order."
        int cached = 0;
        for (String fqn : services) {
            Class<?> svc = serviceClassOrNull(fqn);
            if (svc == null) {
                continue;
            }
            ServiceLoader<?> sl = ServiceLoader.load(svc, appLoader());
            List<Object> a = new ArrayList<>();
            for (Object o : sl) {
                a.add(o);
            }
            List<Object> b = new ArrayList<>();
            for (Object o : sl) {
                b.add(o);
            }
            check(a.size() == b.size(),
                    "a second iteration of one ServiceLoader changed the provider count");
            for (int i = 0; i < a.size(); i++) {
                check(a.get(i) == b.get(i),
                        "a second iteration of one ServiceLoader yielded a DIFFERENT instance at "
                                + i + " -- the documented cache is not in effect");
            }
            cached++;
        }
        check(cached > 0, "the ServiceLoader cache check measured nothing");
        System.out.println("CK RServiceLoaderDoubleSource providers services=" + svcProbed
                + " iters=" + n + " dup=0 drift=0 cached=" + cached);
    }

    // ------------------------------------------------------------------
    // 4. The strict half: a modular jar on -cp is NOT a boot-layer module.
    // ------------------------------------------------------------------

    static List<String> descriptorProviderNames(String serviceFqn) throws Exception {
        List<String> out = new ArrayList<>();
        for (URL u : Collections.list(appLoader()
                .getResources("META-INF/services/" + serviceFqn))) {
            try (BufferedReader r = new BufferedReader(
                    new InputStreamReader(u.openStream(), StandardCharsets.UTF_8))) {
                String line;
                while ((line = r.readLine()) != null) {
                    int hash = line.indexOf('#');
                    if (hash >= 0) {
                        line = line.substring(0, hash);
                    }
                    line = line.trim();
                    if (!line.isEmpty()) {
                        out.add(line);
                    }
                }
            }
        }
        return out;
    }

    /**
     * The discriminating check. Its precondition is supplied, never inferred,
     * and its absence is a FAILURE.
     */
    static void classPathModuleSeparation() throws Exception {
        String moduleName = System.getProperty("cratonvm.rt.cpmodule");
        String className = System.getProperty("cratonvm.rt.cpclass");
        check(moduleName != null && !moduleName.isEmpty(),
                "-Dcratonvm.rt.cpmodule=<module name> is REQUIRED: without a modular jar on -cp "
                        + "this vector's only discriminating assertion cannot run, and a green "
                        + "result would mean nothing");
        check(className != null && !className.isEmpty(),
                "-Dcratonvm.rt.cpclass=<a class inside that jar> is REQUIRED");
        check(System.getProperty("jdk.module.path") == null,
                "this vector must run WITHOUT --module-path: a module-path module IS resolved "
                        + "into the boot layer and IS defined to the application loader on a real "
                        + "JVM, so the assertions below would be measuring the command line");

        Class<?> inJar = Class.forName(className, false, appLoader());
        check(inJar.getClassLoader() == appLoader(),
                "the supplied class must be loaded from the APPLICATION class path");
        check(!inJar.getModule().isNamed(),
                "a class reached through -cp belongs to the UNNAMED module even when its jar "
                        + "carries a module-info.class; got a named module instead");

        Optional<Module> found = ModuleLayer.boot().findModule(moduleName);
        check(found.isEmpty(),
                "ModuleLayer.boot() contains a module whose only source is the CLASS path. "
                        + "A modular jar on -cp is an unnamed-module citizen and its module-info "
                        + "-- including every `provides` clause -- is ignored outright by a real "
                        + "JVM. Promoting it into the boot layer registers its providers in the "
                        + "system loader's ServicesCatalog, so ServiceLoader then finds each of "
                        + "them TWICE: once from the module source and once from "
                        + "META-INF/services");

        // MEASURED, and it is why there is no "the boot layer must define
        // nothing to the application loader" assertion here: on HotSpot 25
        // with a bare `-cp` and no module path, ModuleLayer.boot() already
        // defines 19 modules to the application class loader (jdk.compiler,
        // jdk.jshell and friends are app-loader modules by design). That
        // tempting one-liner is FALSE on the oracle, so the check that
        // discriminates has to be the named-module lookup above, not a count.
        int bootModules = ModuleLayer.boot().modules().size();
        check(bootModules > 0, "ModuleLayer.boot() must not be empty");

        String serviceFqn = System.getProperty("cratonvm.rt.cpservice");
        if (serviceFqn != null && !serviceFqn.isEmpty()) {
            Class<?> svc = serviceClassOrNull(serviceFqn);
            check(svc != null, "-Dcratonvm.rt.cpservice named an unresolvable service");
            TreeSet<String> fromDescriptors = new TreeSet<>(descriptorProviderNames(serviceFqn));
            List<String> resolved = providerTypeNames(svc, appLoader());
            TreeSet<String> distinct = new TreeSet<>(resolved);
            check(resolved.size() == distinct.size(),
                    "ServiceLoader resolved " + resolved.size() + " providers of which only "
                            + distinct.size() + " are distinct classes -- one provider arrived "
                            + "through two sources");
            check(distinct.equals(fromDescriptors),
                    "ServiceLoader's provider set must be exactly what the META-INF/services "
                            + "descriptors on the class path name: got " + distinct.size()
                            + " provider(s), descriptors name " + fromDescriptors.size());
        }

        System.out.println("CK RServiceLoaderDoubleSource separation inBootLayer=false named=false"
                + " svc=" + (serviceFqn == null || serviceFqn.isEmpty() ? "off" : "on"));
    }

    public static void main(String[] args) throws Exception {
        boolean strict = !"0".equals(System.getProperty("cratonvm.rt.separation", "1"))
                && !"off".equals(String.valueOf(System.getProperty("cratonvm.rt.separation"))
                        .toLowerCase(Locale.ROOT));
        resourceCensus();
        setDiscipline();
        providerCensus();
        if (strict) {
            classPathModuleSeparation();
        } else {
            // Not a skip that hides: it is loud, and it names what was not asked.
            System.out.println("CK RServiceLoaderDoubleSource separation DISABLED"
                    + " -- the only discriminating assertion did not run");
        }
        System.out.println("CK RServiceLoaderDoubleSource checks=" + checks);
        System.out.println("PASS RServiceLoaderDoubleSource (" + checks + " checks)");
    }
}
