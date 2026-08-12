import com.cratonvm.jdkonly.svc.Exported;
import com.cratonvm.jdkonly.svc.Greeter;
import com.cratonvm.jdkonly.svc.Nulled;
import com.cratonvm.jdkonly.svc.Rejected;
import com.cratonvm.jdkonly.svc.open.Opened;
import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.lang.module.ModuleDescriptor;
import java.lang.reflect.Field;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.Optional;
import java.util.ServiceConfigurationError;
import java.util.ServiceLoader;

/**
 * JDK-only corpus: named-module application -- module path, reads, exports,
 * opens, resources, and module-path {@code ServiceLoader} providers.
 *
 * The module {@code cratonvm.jdkonly.svc} is compiled from
 * {@code regression-suite/modules/} into {@code regression-suite/build-modules}
 * and resolved with {@code --module-path <dir> --add-modules
 * cratonvm.jdkonly.svc}; this vector itself runs from the CLASS PATH, i.e. from
 * the unnamed module, which is the ordinary "application uses a modular
 * dependency" shape.
 *
 * NB: CratonVM's launcher accepts {@code --module-path} / {@code --add-modules}
 * but has no {@code -m <module>/<class>} main-module form, so a fully modular
 * launch is out of scope for this vector -- see
 * regression-suite/jdk-only-coverage.txt.
 *
 * Determinism: descriptor collections are unordered sets, so every emitted list
 * is sorted; no paths or URLs are printed.
 */
public class RJdkModule {
    static final String MODULE = "cratonvm.jdkonly.svc";
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    static Module svc() {
        Optional<Module> m = ModuleLayer.boot().findModule(MODULE);
        check(m.isPresent(), MODULE + " is not in the boot layer -- was --module-path/"
                + "--add-modules passed?");
        return m.get();
    }

    static void descriptor() {
        Module m = svc();
        check(m.isNamed(), "the module must be named");
        check(m.getName().equals(MODULE), "module name: " + m.getName());
        check(m.getLayer() == ModuleLayer.boot(), "module must be in the boot layer");

        ModuleDescriptor d = m.getDescriptor();
        check(d != null, "module descriptor");
        check(!d.isAutomatic(), "an explicit module must not be automatic");
        check(!d.isOpen(), "the module itself is not an open module");

        List<String> exports = new ArrayList<>();
        for (ModuleDescriptor.Exports e : d.exports()) {
            exports.add(e.source() + (e.isQualified() ? "@qualified" : ""));
        }
        Collections.sort(exports);
        check(exports.equals(Arrays.asList(
                "com.cratonvm.jdkonly.svc", "com.cratonvm.jdkonly.svc.open")),
                "exports: " + exports);

        List<String> opens = new ArrayList<>();
        for (ModuleDescriptor.Opens o : d.opens()) {
            opens.add(o.source());
        }
        Collections.sort(opens);
        check(opens.equals(Collections.singletonList("com.cratonvm.jdkonly.svc.open")),
                "opens: " + opens);

        List<String> packages = new ArrayList<>(d.packages());
        Collections.sort(packages);
        check(packages.equals(Arrays.asList(
                "com.cratonvm.jdkonly.svc",
                "com.cratonvm.jdkonly.svc.internal",
                "com.cratonvm.jdkonly.svc.open")),
                "packages: " + packages);

        List<String> provides = new ArrayList<>();
        for (ModuleDescriptor.Provides p : d.provides()) {
            List<String> impls = new ArrayList<>(p.providers());
            Collections.sort(impls);
            provides.add(p.service() + "->" + impls.size());
        }
        Collections.sort(provides);
        // Three services: Greeter with two legal providers, and the two illegal
        // factory shapes -- Rejected (provider() return type is not a subtype)
        // and Nulled (provider() answers null). See moduleServiceRejects. The
        // descriptor records all three clauses either way: the JDK's rules here
        // are enforced at LOAD, not at resolution.
        check(provides.equals(Arrays.asList(
                "com.cratonvm.jdkonly.svc.Greeter->2",
                "com.cratonvm.jdkonly.svc.Nulled->1",
                "com.cratonvm.jdkonly.svc.Rejected->1")), "provides: " + provides);

        check(d.requires().stream().anyMatch(r -> r.name().equals("java.base")),
                "every module implicitly requires java.base");
        System.out.println("CK RJdkModule exports=" + exports + " opens=" + opens
                + " packages=" + packages + " provides=" + provides);
    }

    static void readabilityAndExports() {
        Module svc = svc();
        Module unnamed = RJdkModule.class.getModule();
        check(!unnamed.isNamed(), "the class-path consumer must be in the unnamed module");
        check(unnamed.getName() == null, "an unnamed module has no name");

        // The unnamed module reads every module; a named module does NOT read
        // the unnamed module unless it says so.
        check(unnamed.canRead(svc), "the unnamed module must read every resolved module");
        check(!svc.canRead(unnamed),
                "a named module must NOT implicitly read the unnamed module");
        check(svc.canRead(Object.class.getModule()), "svc reads java.base");
        check(Object.class.getModule().getName().equals("java.base"), "java.base identity");

        check(svc.isExported("com.cratonvm.jdkonly.svc"), "exported package");
        check(svc.isExported("com.cratonvm.jdkonly.svc.open"), "second exported package");
        check(!svc.isExported("com.cratonvm.jdkonly.svc.internal"),
                "the internal package must NOT be exported");
        check(svc.isOpen("com.cratonvm.jdkonly.svc.open"), "opened package");
        check(!svc.isOpen("com.cratonvm.jdkonly.svc"),
                "an exported package is not automatically opened");
        check(!svc.isOpen("com.cratonvm.jdkonly.svc.internal"), "internal is not open");

        // Class identity: the exported types belong to the module.
        check(Greeter.class.getModule() == svc, "Greeter belongs to the module");
        check(Exported.class.getModule() == svc, "Exported belongs to the module");
        check(Greeter.class.getClassLoader() == RJdkModule.class.getClassLoader(),
                "module-path classes are defined to the application loader");

        // Ordinary (non-reflective) use of an exported type works.
        check(new Exported().visible().equals("exported-public"), "exported public API");
        check(new Opened().visible().equals("opened-public"), "opened package public API");
        System.out.println("CK RJdkModule reads=" + unnamed.canRead(svc)
                + " svcReadsUnnamed=" + svc.canRead(unnamed)
                + " internalExported=" + svc.isExported("com.cratonvm.jdkonly.svc.internal"));
    }

    static void encapsulation() throws Exception {
        // OPENED package: deep reflection succeeds.
        Field open = Opened.class.getDeclaredField("secret");
        open.setAccessible(true);
        check("opened-private".equals(open.get(new Opened())),
                "deep reflection into an OPENED package must succeed");

        // EXPORTED-but-not-opened package: deep reflection is refused.
        Field closed = Exported.class.getDeclaredField("hidden");
        boolean threw = false;
        String kind = "";
        try {
            closed.setAccessible(true);
        } catch (RuntimeException e) {
            kind = e.getClass().getSimpleName();
            threw = kind.equals("InaccessibleObjectException");
        }
        check(threw, "setAccessible into an exported-but-not-opened package must be refused, got "
                + kind);

        // ENCAPSULATED package: the class is loadable by name (module-path
        // classes are defined to the app loader) but not instantiable.
        Class<?> internal = Class.forName("com.cratonvm.jdkonly.svc.internal.EnGreeter");
        check(internal.getModule() == svc(), "internal class belongs to the module");
        threw = false;
        try {
            internal.getDeclaredConstructor().newInstance();
        } catch (IllegalAccessException expected) {
            threw = true;
        }
        check(threw, "instantiating a class in a non-exported package must be refused");
        System.out.println("CK RJdkModule openedDeepReflect=ok closedDeepReflect=" + kind);
    }

    static String slurp(InputStream in) throws Exception {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        byte[] buf = new byte[1024];
        int n;
        while ((n = in.read(buf)) > 0) {
            out.write(buf, 0, n);
        }
        in.close();
        return out.toString(StandardCharsets.UTF_8.name()).trim();
    }

    static void resources() throws Exception {
        Module svc = svc();
        // A resource in an OPENED package is readable from another module.
        InputStream open = svc.getResourceAsStream(
                "com/cratonvm/jdkonly/svc/open/greeting.txt");
        check(open != null, "a resource in an opened package must be readable");
        String body = slurp(open);
        check(body.equals("module-greeting-resource"), "resource body: " + body);

        // A resource in a non-open package is encapsulated.
        InputStream secret = svc.getResourceAsStream("com/cratonvm/jdkonly/svc/secret.txt");
        check(secret == null,
                "a resource in a non-open package must NOT be readable from another module");

        // .class files are never encapsulated.
        InputStream cls = svc.getResourceAsStream(
                "com/cratonvm/jdkonly/svc/internal/EnGreeter.class");
        check(cls != null, ".class resources are always readable");
        cls.close();

        // A missing resource is null, not a fabricated empty stream.
        check(svc.getResourceAsStream("com/cratonvm/jdkonly/svc/open/absent.txt") == null,
                "a missing module resource must be null");
        System.out.println("CK RJdkModule resource=" + body + " encapsulated=" + (secret == null));
    }

    static void moduleServices() {
        // Module-path providers are discovered through the module descriptor,
        // NOT through META-INF/services -- including the static provider()
        // factory form, which is module-path only.
        ServiceLoader<Greeter> sl = ServiceLoader.load(Greeter.class);
        List<String> greets = new ArrayList<>();
        for (Greeter g : sl) {
            greets.add(g.greet());
        }
        Collections.sort(greets);
        check(greets.equals(Arrays.asList("module-factory", "module-hello")),
                "module service providers: " + greets);

        List<String> types = ServiceLoader.load(Greeter.class).stream()
                .map(p -> p.type().getSimpleName())
                .sorted()
                .collect(java.util.stream.Collectors.toList());
        // For a constructor provider, Provider.type() is the provider class; for
        // a provider() FACTORY it is the factory method's RETURN type (the JDK
        // builds ProviderImpl with factoryMethod.getReturnType()), hence
        // "Greeter" rather than "FactoryGreeter".
        check(types.equals(Arrays.asList("EnGreeter", "Greeter")), "provider types: " + types);

        // The layer-scoped overload must agree.
        List<String> viaLayer = new ArrayList<>();
        for (Greeter g : ServiceLoader.load(ModuleLayer.boot(), Greeter.class)) {
            viaLayer.add(g.greet());
        }
        Collections.sort(viaLayer);
        check(viaLayer.equals(greets), "layer-scoped ServiceLoader must agree: " + viaLayer);
        System.out.println("CK RJdkModule services=" + greets + " types=" + types);
    }

    /** The greeters a legal ServiceLoader must still produce, sorted. */
    static List<String> greetersVia(ServiceLoader<Greeter> sl) {
        List<String> out = new ArrayList<>();
        for (Greeter g : sl) {
            out.add(g.greet());
        }
        Collections.sort(out);
        return out;
    }

    /**
     * The shape of the error, measured on HotSpot 25.0.3.9 rather than assumed:
     *
     * <pre>
     * java.util.ServiceConfigurationError:
     *   com.cratonvm.jdkonly.svc.Rejected: public static java.lang.Object
     *   com.cratonvm.jdkonly.svc.internal.WrongFactory.provider()
     *   return type not a subtype        (getCause() == null)
     * </pre>
     *
     * i.e. {@code ServiceLoader.fail(service, factoryMethod + " return type not
     * a subtype")}, which is {@code service.getName() + ": " + msg} with the
     * one-argument {@code ServiceConfigurationError} constructor, so no cause.
     * Asserting only "something was thrown" would pass against a
     * {@code ClassCastException} raised somewhere else entirely, which is the
     * failure this whole section exists to distinguish.
     */
    static void checkRejection(String where, Throwable t, Object handedOut) {
        check(t != null, where + " must not hand out the illegal provider (got " + handedOut + ")");
        check(t instanceof ServiceConfigurationError,
                where + " must raise ServiceConfigurationError, not "
                        + t.getClass().getName() + ": " + t);
        check(t.getClass() == ServiceConfigurationError.class,
                where + " must raise ServiceConfigurationError exactly, not a subclass: "
                        + t.getClass().getName());
        String m = t.getMessage();
        check(m != null, where + " ServiceConfigurationError must carry a message");
        check(m.startsWith(REJECTED_SERVICE + ": "),
                where + " message must open with the service name: " + m);
        check(m.contains(WRONG_FACTORY), where + " message must name the provider: " + m);
        check(m.endsWith("return type not a subtype"), where + " message tail: " + m);
        check(t.getCause() == null,
                where + " ServiceLoader.fail(service, msg) leaves no cause, got " + t.getCause());
    }

    static final String REJECTED_SERVICE = "com.cratonvm.jdkonly.svc.Rejected";
    static final String WRONG_FACTORY = "com.cratonvm.jdkonly.svc.internal.WrongFactory";
    static final String NULLED_SERVICE = "com.cratonvm.jdkonly.svc.Nulled";
    static final String NULL_PROVIDER = "com.cratonvm.jdkonly.svc.internal.NullProvider";

    /**
     * {@code ProviderImpl.invokeFactoryMethod}'s null guard. Measured on
     * HotSpot 25.0.3.9:
     *
     * <pre>
     * java.util.ServiceConfigurationError: com.cratonvm.jdkonly.svc.Nulled:
     *   public static com.cratonvm.jdkonly.svc.Nulled
     *   com.cratonvm.jdkonly.svc.internal.NullProvider.provider() returned null
     * </pre>
     */
    static void checkNullRejection(String where, Throwable t, Object handedOut) {
        check(t != null, where + " must not hand out a null provider (got " + handedOut + ")");
        check(t.getClass() == ServiceConfigurationError.class,
                where + " must raise ServiceConfigurationError exactly, not "
                        + t.getClass().getName() + ": " + t);
        String m = t.getMessage();
        check(m != null, where + " ServiceConfigurationError must carry a message");
        check(m.startsWith(NULLED_SERVICE + ": "),
                where + " message must open with the service name: " + m);
        check(m.contains(NULL_PROVIDER), where + " message must name the provider: " + m);
        check(m.endsWith("returned null"), where + " message tail: " + m);
        check(t.getCause() == null,
                where + " ServiceLoader.fail(service, msg) leaves no cause, got " + t.getCause());
    }

    /**
     * The NEGATIVE half of the module-path {@code provider()} factory form:
     * a module-declared provider whose factory returns a type that is NOT a
     * subtype of the service must fail the load, on EVERY traversal.
     *
     * {@code ServiceLoader.loadProvider} raises there
     * ({@code !service.isAssignableFrom(returnType)}), and every route into it
     * -- {@code iterator()}, {@code forEach}, {@code findFirst()},
     * {@code stream()} -- goes through it. The two families are asserted
     * separately on purpose: a guard installed on only one of them reads green
     * forever against a fixture whose provider is legal, which is exactly how
     * this gap survived the other 44 checks in this file.
     *
     * TIMING. On HotSpot the error surfaces during TRAVERSAL, not at the call
     * that returns the iterator or the stream: {@code
     * ModuleServicesLookupIterator.hasNext()} catches the error and stashes it
     * in {@code nextError}, and {@code next()} rethrows it; {@code stream()} is
     * lazy and only pulls through {@code ProviderSpliterator.tryAdvance}.
     * Measured: {@code load()}, {@code iterator()}, {@code hasNext()} and
     * {@code stream()} alone all return normally on HotSpot 25. Each block
     * below therefore wraps the WHOLE traversal, so a VM that materialises
     * eagerly and a VM that materialises lazily both satisfy it.
     */
    static void moduleServiceRejects() {
        // Discovery itself is not the failure: the descriptor names the clause,
        // and it is `load` that must refuse it.
        check(ServiceLoader.load(Rejected.class) != null, "load(Rejected) must return a loader");

        // --- the iterator family -------------------------------------------
        List<String> viaIterator = null;
        Throwable iteratorErr = null;
        try {
            List<String> ids = new ArrayList<>();
            for (Rejected r : ServiceLoader.load(Rejected.class)) {
                ids.add(r.id());
            }
            viaIterator = ids;
        } catch (Throwable t) {
            iteratorErr = t;
        }
        checkRejection("iterator()", iteratorErr, viaIterator);

        Optional<Rejected> viaFindFirst = null;
        Throwable findFirstErr = null;
        try {
            viaFindFirst = ServiceLoader.load(Rejected.class).findFirst();
        } catch (Throwable t) {
            findFirstErr = t;
        }
        checkRejection("findFirst()", findFirstErr, viaFindFirst);

        List<String> viaForEach = null;
        Throwable forEachErr = null;
        try {
            List<String> ids = new ArrayList<>();
            ServiceLoader.load(Rejected.class).forEach(r -> ids.add(r.id()));
            viaForEach = ids;
        } catch (Throwable t) {
            forEachErr = t;
        }
        checkRejection("forEach()", forEachErr, viaForEach);

        // --- the stream family ----------------------------------------------
        // Provider.type() is read WITHOUT calling Provider.get(): the JDK
        // refuses the provider while building the wrapper, so a VM that only
        // fails inside get() would still be handing out a Provider the spec
        // says must not exist.
        List<String> viaStream = null;
        Throwable streamErr = null;
        try {
            viaStream = ServiceLoader.load(Rejected.class).stream()
                    .map(p -> p.type().getName())
                    .collect(java.util.stream.Collectors.toList());
        } catch (Throwable t) {
            streamErr = t;
        }
        checkRejection("stream()", streamErr, viaStream);

        // A short-circuiting terminal must not slip past the check either.
        Object viaStreamFirst = null;
        Throwable streamFirstErr = null;
        try {
            viaStreamFirst = ServiceLoader.load(Rejected.class).stream().findFirst();
        } catch (Throwable t) {
            streamFirstErr = t;
        }
        checkRejection("stream().findFirst()", streamFirstErr, viaStreamFirst);

        // --- the OTHER illegal factory shape: provider() answers null --------
        // Different rule, different moment. `ProviderImpl.invokeFactoryMethod`
        // raises only when the factory has actually been called, so
        // `Provider.type()` must still answer normally and only `get()` throws.
        // Asserting the type() call is the non-vacuous half: a VM that refuses
        // a null-returning factory while building the wrapper would satisfy
        // "get() throws" and still be wrong.
        List<String> nulledTypes = ServiceLoader.load(Nulled.class).stream()
                .map(p -> p.type().getName())
                .collect(java.util.stream.Collectors.toList());
        check(nulledTypes.equals(Collections.singletonList(NULLED_SERVICE)),
                "Provider.type() must answer before get() is ever called: " + nulledTypes);

        List<String> viaNullIterator = null;
        Throwable nullIteratorErr = null;
        try {
            List<String> ids = new ArrayList<>();
            for (Nulled n : ServiceLoader.load(Nulled.class)) {
                ids.add(String.valueOf(n));
            }
            viaNullIterator = ids;
        } catch (Throwable t) {
            nullIteratorErr = t;
        }
        checkNullRejection("iterator()", nullIteratorErr, viaNullIterator);

        List<String> viaNullStream = null;
        Throwable nullStreamErr = null;
        try {
            viaNullStream = ServiceLoader.load(Nulled.class).stream()
                    .map(p -> String.valueOf(p.get()))
                    .collect(java.util.stream.Collectors.toList());
        } catch (Throwable t) {
            nullStreamErr = t;
        }
        checkNullRejection("stream().get()", nullStreamErr, viaNullStream);

        // The two paths must not merely both throw -- they must report the SAME
        // provider failure the same way. Comparing them needs no hardcoded
        // oracle string and catches precisely the class of bug this section
        // exists for: one path validating, the other not, or the two drifting
        // into different wordings for one configuration error.
        check(nullIteratorErr.getMessage().equals(nullStreamErr.getMessage()),
                "iterator() and stream() must agree on the message:\n  iterator: "
                        + nullIteratorErr.getMessage() + "\n  stream:   "
                        + nullStreamErr.getMessage());
        check(iteratorErr.getMessage().equals(streamErr.getMessage()),
                "iterator() and stream() must agree on the message:\n  iterator: "
                        + iteratorErr.getMessage() + "\n  stream:   "
                        + streamErr.getMessage());

        // The legal service is untouched by the illegal one declared beside it,
        // and is still legal AFTER the refusals -- a "fix" that refuses
        // everything would otherwise pass every check above.
        List<String> stillGood = greetersVia(ServiceLoader.load(Greeter.class));
        check(stillGood.equals(Arrays.asList("module-factory", "module-hello")),
                "a legal service must still load after a rejected one: " + stillGood);
        List<String> stillGoodTypes = ServiceLoader.load(Greeter.class).stream()
                .map(p -> p.type().getSimpleName())
                .sorted()
                .collect(java.util.stream.Collectors.toList());
        check(stillGoodTypes.equals(Arrays.asList("EnGreeter", "Greeter")),
                "a legal service must still stream after a rejected one: " + stillGoodTypes);

        System.out.println("CK RJdkModule rejected=" + REJECTED_SERVICE.substring(
                REJECTED_SERVICE.lastIndexOf('.') + 1)
                + " iterator=" + iteratorErr.getClass().getSimpleName()
                + " stream=" + streamErr.getClass().getSimpleName()
                + " cause=" + (streamErr.getCause() == null ? "none" : "some")
                + " stillGood=" + stillGood);
    }

    public static void main(String[] args) throws Exception {
        descriptor();
        readabilityAndExports();
        encapsulation();
        resources();
        moduleServices();
        moduleServiceRejects();
        System.out.println("CK RJdkModule checks=" + checks);
        System.out.println("PASS RJdkModule (" + checks + " checks)");
    }
}
