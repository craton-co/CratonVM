import com.cratonvm.jdkonly.svc.Ctored;
import com.cratonvm.jdkonly.svc.Exported;
import com.cratonvm.jdkonly.svc.Greeter;
import com.cratonvm.jdkonly.svc.Nulled;
import com.cratonvm.jdkonly.svc.Rejected;
import com.cratonvm.jdkonly.svc.Unsub;
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
        check(d.name().equals(MODULE), "descriptor name: " + d.name());
        check(!d.isAutomatic(), "an explicit module must not be automatic");
        check(!d.isOpen(), "the module itself is not an open module");

        // ---------------------------------------------------------------
        // The flag/version accessors, and an HONEST statement of what is and
        // is not being measured here.
        //
        // KNOWN NOT DISCRIMINATING, and recorded rather than dressed up.
        // `isAutomatic()` above is answered by a HARDCODED false in this VM
        // (native-builtins/src/jboss_jdkspecific.rs, build_module_descriptor:
        // `set_field_by_name(desc, "automatic", Value::Int(0))`, with a comment
        // saying exactly this), and false happens to be the RIGHT answer for
        // cratonvm.jdkonly.svc. So that check passes on the fix and on the
        // hardcode alike. It cannot be repaired from inside this file: telling
        // the two apart needs a module that DISAGREES with the constant, i.e.
        // an automatic module -- a plain jar with no module-info on the module
        // path -- which is new harness work. See
        // docs/known-issues/jdk-only/W2-3-module-descriptor-answers-empty-sets.md,
        // whose out-of-file patch is the fix, and which reaches the same
        // conclusion about why no assertion was added there either. The marker
        // is left visible rather than replaced by a check that always passes.
        //
        // What CAN be pinned from here, and is:
        //
        //  1. The two flag accessors and the modifier SET must agree. They are
        //     two views of one fact, and this VM builds them on two separate
        //     code paths (build_module_modifier_set vs the `open`/`automatic`
        //     fields), so they can and do drift independently -- and a fix that
        //     lands on only one of the two paths is the most likely next state
        //     of this exact residual.
        check(d.modifiers().contains(ModuleDescriptor.Modifier.OPEN) == d.isOpen(),
                "isOpen() and modifiers().contains(OPEN) disagree: " + d.modifiers());
        check(d.modifiers().contains(ModuleDescriptor.Modifier.AUTOMATIC) == d.isAutomatic(),
                "isAutomatic() and modifiers().contains(AUTOMATIC) disagree: " + d.modifiers());
        //  2. Nothing may be FABRICATED. This module-info is compiled with no
        //     --module-version and no --main-class, so empty is the truthful
        //     answer for all three; the VM leaves the backing fields null on
        //     purpose and the record says so in as many words. These assertions
        //     therefore do NOT discriminate the missing-accessor defect -- they
        //     discriminate the other direction, an implementation that invents a
        //     value to look complete, which is this campaign's dominant species.
        check(d.version().isEmpty(), "no --module-version was recorded: " + d.version());
        check(d.rawVersion().isEmpty(), "no raw version was recorded: " + d.rawVersion());
        check(d.mainClass().isEmpty(), "no --main-class was recorded: " + d.mainClass());
        //  3. version() is the PARSED form of the raw version string, so it
        //     cannot be present when the raw string is absent. True vacuously
        //     today (both empty); it stops being vacuous the moment either is
        //     sourced, which is the point of writing it now.
        check(d.rawVersion().isPresent() || d.version().isEmpty(),
                "version() present with no rawVersion(): " + d.version());
        //  4. toNameAndVersion() must be derived from the two above rather than
        //     independently synthesised.
        check(d.toNameAndVersion().equals(
                        MODULE + d.rawVersion().map(v -> "@" + v).orElse("")),
                "toNameAndVersion: " + d.toNameAndVersion());

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
        // Five services: Greeter with two legal providers, the two illegal
        // FACTORY shapes -- Rejected (provider() return type is not a subtype)
        // and Nulled (provider() answers null) -- and the two illegal
        // CONSTRUCTOR shapes, Unsub (not a subtype) and Ctored (no public
        // no-arg constructor). See moduleServiceRejects and
        // moduleServiceConstructorRejects. The descriptor records all five
        // clauses either way: the JDK's rules here are enforced at LOAD, not at
        // resolution, which is what makes this list a control on the four
        // negatives rather than a duplicate of them.
        check(provides.equals(Arrays.asList(
                "com.cratonvm.jdkonly.svc.Ctored->1",
                "com.cratonvm.jdkonly.svc.Greeter->2",
                "com.cratonvm.jdkonly.svc.Nulled->1",
                "com.cratonvm.jdkonly.svc.Rejected->1",
                "com.cratonvm.jdkonly.svc.Unsub->1")), "provides: " + provides);

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

        // W6-8's missing POSITIVE witness: the SAME module gate on
        // Method.invoke. `greet()` is public on a public final class, so
        // nothing except the module gate can refuse it -- and the Method's
        // DECLARING class is EnGreeter, which lives in the encapsulated
        // package. (`Greeter.class.getMethod("greet")` is the other half of
        // that distinction and must be allowed: it is declared by an EXPORTED
        // type. Same method name, different answer.)
        //
        // The receiver deliberately does NOT come from
        // `internal.getDeclaredConstructor().newInstance()`. The check three
        // lines up asserts that exact call is REFUSED, so an assertion built on
        // it would catch the CONSTRUCTOR gate's IllegalAccessException and pass
        // whether or not Method.invoke has a module gate at all -- green
        // forever, measuring the check above it twice. ServiceLoader is the one
        // door allowed to produce an EnGreeter, so the instance comes from
        // there.
        Greeter enGreeter = null;
        for (Greeter g : ServiceLoader.load(Greeter.class)) {
            if ("module-hello".equals(g.greet())) {
                enGreeter = g;
            }
        }
        check(enGreeter != null, "ServiceLoader must produce the encapsulated provider -- "
                + "without it the Method.invoke check below has no receiver and would "
                + "NPE rather than measure the gate");
        threw = false;
        try {
            internal.getMethod("greet").invoke(enGreeter);
        } catch (IllegalAccessException expected) {
            threw = true;
        }
        check(threw, "invoking a PUBLIC method of a class in a non-exported package must "
                + "be refused");

        // Negative control for that gate. The identical shape on an EXPORTED
        // package must SUCCEED; without this a gate that refused every
        // reflective invoke would satisfy the check above.
        check("exported-public".equals(
                        Exported.class.getMethod("visible").invoke(new Exported())),
                "invoking a public method of an EXPORTED package must NOT be refused");
        System.out.println("CK RJdkModule openedDeepReflect=ok closedDeepReflect=" + kind
                + " publicInvokeRefused=" + threw);
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

    static final String UNSUB_SERVICE = "com.cratonvm.jdkonly.svc.Unsub";
    static final String NOT_SUB_PROVIDER = "com.cratonvm.jdkonly.svc.internal.NotSubProvider";
    static final String CTORED_SERVICE = "com.cratonvm.jdkonly.svc.Ctored";
    static final String HIDDEN_CTOR = "com.cratonvm.jdkonly.svc.internal.HiddenCtor";

    /**
     * {@code ServiceLoader.loadProvider}'s CONSTRUCTOR-form subtype rule,
     * measured on HotSpot 25.0.3.9:
     *
     * <pre>
     * java.util.ServiceConfigurationError: com.cratonvm.jdkonly.svc.Unsub:
     *   class com.cratonvm.jdkonly.svc.internal.NotSubProvider not a subtype
     *                                                  (getCause() == null)
     * </pre>
     *
     * i.e. the two-argument {@code fail(service, clazz + " not a subtype")}.
     * The message is asserted by opening / provider name / tail rather than
     * verbatim, because the JDK's two provider paths render the class
     * differently for this one rule -- {@code clazz} (so
     * {@code Class.toString()}, with its {@code class }/{@code interface }
     * prefix) on the module path, {@code clazz.getName()} on the classpath --
     * and the shape is what this vector is pinning.
     */
    static void checkNotSubtype(String where, Throwable t, Object handedOut) {
        check(t != null,
                where + " must not hand out a non-subtype provider (got " + handedOut + ")");
        check(t.getClass() == ServiceConfigurationError.class,
                where + " must raise ServiceConfigurationError exactly, not "
                        + t.getClass().getName() + ": " + t);
        String m = t.getMessage();
        check(m != null, where + " ServiceConfigurationError must carry a message");
        check(m.startsWith(UNSUB_SERVICE + ": "),
                where + " message must open with the service name: " + m);
        check(m.contains(NOT_SUB_PROVIDER), where + " message must name the provider: " + m);
        check(m.endsWith("not a subtype"), where + " message tail: " + m);
        check(t.getCause() == null,
                where + " fail(service, msg) leaves no cause, got " + t.getCause());
    }

    /**
     * {@code ServiceLoader.getConstructor}'s failure, measured on HotSpot
     * 25.0.3.9:
     *
     * <pre>
     * java.util.ServiceConfigurationError: com.cratonvm.jdkonly.svc.Ctored:
     *   com.cratonvm.jdkonly.svc.internal.HiddenCtor
     *   Unable to get public no-arg constructor
     *     caused by java.lang.NoSuchMethodException:
     *       com.cratonvm.jdkonly.svc.internal.HiddenCtor.&lt;init&gt;()
     * </pre>
     *
     * This is the THREE-argument {@code fail(service, msg, x)}, so the cause is
     * part of the contract and is asserted: a VM that refuses with a bare
     * message is refusing for a reason it never established, and
     * {@code getConstructor()}'s {@code NoSuchMethodException} IS the reason.
     */
    static void checkNoPublicCtor(String where, Throwable t, Object handedOut) {
        check(t != null, where + " must not hand out a provider with no public no-arg "
                + "constructor (got " + handedOut + ")");
        check(t.getClass() == ServiceConfigurationError.class,
                where + " must raise ServiceConfigurationError exactly, not "
                        + t.getClass().getName() + ": " + t);
        String m = t.getMessage();
        check(m != null, where + " ServiceConfigurationError must carry a message");
        check(m.startsWith(CTORED_SERVICE + ": "),
                where + " message must open with the service name: " + m);
        check(m.contains(HIDDEN_CTOR), where + " message must name the provider: " + m);
        check(m.endsWith("Unable to get public no-arg constructor"),
                where + " message tail: " + m);
        check(t.getCause() instanceof NoSuchMethodException,
                where + " must carry getConstructor()'s NoSuchMethodException, got "
                        + t.getCause());
    }

    /**
     * The CONSTRUCTOR half of {@code ServiceLoader.loadProvider} -- the two
     * rules that apply once {@code findStaticProviderMethod} has answered null:
     *
     * <pre>
     * if (!service.isAssignableFrom(clazz)) fail(service, clazz + " not a subtype");
     * ctor = clazz.getConstructor();   // PUBLIC no-arg, or fail
     * </pre>
     *
     * Both were absent on BOTH provider paths. Neither could be seen by the
     * existing checks: every provider in the {@code Greeter} clause is a legal
     * subtype with a public constructor, so the positive case exercises the
     * code and cannot exercise the refusal -- the same vacuous-green shape the
     * factory half of this file was written for.
     *
     * TIMING, and the same rule as {@code moduleServiceRejects}: HotSpot
     * surfaces both errors during TRAVERSAL, so every block below wraps the
     * whole traversal rather than the call that returns the iterator or the
     * stream. An eager VM and a lazy VM both satisfy that.
     */
    static void moduleServiceConstructorRejects() {
        // --- not a subtype, constructor form ---------------------------------
        List<String> viaIterator = null;
        Throwable iteratorErr = null;
        try {
            List<String> ids = new ArrayList<>();
            for (Unsub u : ServiceLoader.load(Unsub.class)) {
                ids.add(u.id());
            }
            viaIterator = ids;
        } catch (Throwable t) {
            iteratorErr = t;
        }
        checkNotSubtype("iterator()", iteratorErr, viaIterator);

        Optional<Unsub> viaFindFirst = null;
        Throwable findFirstErr = null;
        try {
            viaFindFirst = ServiceLoader.load(Unsub.class).findFirst();
        } catch (Throwable t) {
            findFirstErr = t;
        }
        checkNotSubtype("findFirst()", findFirstErr, viaFindFirst);

        // Provider.type() without ever calling get(): the JDK refuses while
        // BUILDING the wrapper, so a VM that only fails inside get() would
        // still be handing out a Provider the spec says must not exist. This is
        // also the check that distinguishes a real gate from a lucky
        // ClassCastException raised by the consumer's own implicit cast.
        List<String> viaStream = null;
        Throwable streamErr = null;
        try {
            viaStream = ServiceLoader.load(Unsub.class).stream()
                    .map(p -> p.type().getName())
                    .collect(java.util.stream.Collectors.toList());
        } catch (Throwable t) {
            streamErr = t;
        }
        checkNotSubtype("stream()", streamErr, viaStream);

        // --- no PUBLIC no-arg constructor ------------------------------------
        List<String> viaCtorIterator = null;
        Throwable ctorIteratorErr = null;
        try {
            List<String> ids = new ArrayList<>();
            for (Ctored c : ServiceLoader.load(Ctored.class)) {
                ids.add(c.id());
            }
            viaCtorIterator = ids;
        } catch (Throwable t) {
            ctorIteratorErr = t;
        }
        checkNoPublicCtor("iterator()", ctorIteratorErr, viaCtorIterator);

        List<String> viaCtorStream = null;
        Throwable ctorStreamErr = null;
        try {
            viaCtorStream = ServiceLoader.load(Ctored.class).stream()
                    .map(p -> p.type().getName())
                    .collect(java.util.stream.Collectors.toList());
        } catch (Throwable t) {
            ctorStreamErr = t;
        }
        checkNoPublicCtor("stream()", ctorStreamErr, viaCtorStream);

        // The two paths must not merely both throw -- they must report the same
        // configuration error the same way. No hardcoded oracle is needed, and
        // this is exactly the failure mode the whole section exists for: one
        // path validating and the other not.
        check(iteratorErr.getMessage().equals(streamErr.getMessage()),
                "iterator() and stream() must agree on the message:\n  iterator: "
                        + iteratorErr.getMessage() + "\n  stream:   " + streamErr.getMessage());
        check(ctorIteratorErr.getMessage().equals(ctorStreamErr.getMessage()),
                "iterator() and stream() must agree on the message:\n  iterator: "
                        + ctorIteratorErr.getMessage() + "\n  stream:   "
                        + ctorStreamErr.getMessage());

        // A "fix" that refuses everything would pass every check above.
        List<String> stillGood = greetersVia(ServiceLoader.load(Greeter.class));
        check(stillGood.equals(Arrays.asList("module-factory", "module-hello")),
                "a legal service must still load after the constructor-form refusals: "
                        + stillGood);
        System.out.println("CK RJdkModule ctorRejects=" + iteratorErr.getClass().getSimpleName()
                + "/" + ctorIteratorErr.getClass().getSimpleName()
                + " ctorCause=" + ctorIteratorErr.getCause().getClass().getSimpleName()
                + " stillGood=" + stillGood);
    }

    /**
     * An array class's module is its COMPONENT type's module -- never a blanket
     * {@code java.base}. {@code Class.getModule()}'s javadoc says it outright
     * ("If this class represents an array type then this method returns the
     * Module for the element type"), and it was measured on Temurin 25.0.3:
     *
     * <pre>
     * int[].class.getModule()        module java.base
     * String[].class.getModule()     module java.base
     * ArrProbe[].class.getModule()   unnamed module @691a7f8f
     * </pre>
     *
     * CratonVM synthesised every array class with a hardcoded
     * {@code module_name = "java.base"}, so the two java.base rows above were
     * right by accident and every other row was wrong. The positive rows are
     * kept as a control: a "fix" that answers the unnamed module for everything
     * would satisfy the negative rows alone.
     */
    static void arrayModules() {
        Module svc = svc();
        Module base = Object.class.getModule();
        Module unnamed = RJdkModule.class.getModule();

        // Control: a primitive component type, and a java.base reference one.
        // These passed against the hardcode too, and must keep passing.
        check(int[].class.getModule() == base, "int[] belongs to java.base");
        check(long[][].class.getModule() == base, "long[][] belongs to java.base");
        check(String[].class.getModule() == base, "String[] belongs to java.base");

        // The defect: a component type in a NAMED application module.
        check(Exported[].class.getModule() == svc,
                "Exported[] must report its component's module, got "
                        + Exported[].class.getModule());
        check(Greeter[].class.getModule() == svc,
                "Greeter[] must report its component's module, got "
                        + Greeter[].class.getModule());
        // Multi-dimensional: the rule has to recurse through the inner array,
        // which is how the element type's loader already propagates.
        check(Exported[][].class.getModule() == svc,
                "Exported[][] must report its element type's module, got "
                        + Exported[][].class.getModule());

        // ... and a CLASSPATH component type: the UNNAMED module, not java.base.
        check(!RJdkModule[].class.getModule().isNamed(),
                "a class-path component type gives an array in the unnamed module, got "
                        + RJdkModule[].class.getModule());
        check(RJdkModule[].class.getModule() == unnamed,
                "the array's unnamed module must be the component's own: "
                        + RJdkModule[].class.getModule() + " vs " + unnamed);
        check(RJdkModule[][].class.getModule() == unnamed,
                "same for the multi-dimensional case: " + RJdkModule[][].class.getModule());

        System.out.println("CK RJdkModule arrayModule=" + Exported[].class.getModule().getName()
                + " intArray=" + int[].class.getModule().getName()
                + " cpArrayNamed=" + RJdkModule[].class.getModule().isNamed());
    }

    public static void main(String[] args) throws Exception {
        descriptor();
        readabilityAndExports();
        encapsulation();
        resources();
        arrayModules();
        moduleServices();
        moduleServiceRejects();
        moduleServiceConstructorRejects();
        System.out.println("CK RJdkModule checks=" + checks);
        System.out.println("PASS RJdkModule (" + checks + " checks)");
    }
}
