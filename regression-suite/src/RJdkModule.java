import com.cratonvm.jdkonly.svc.Exported;
import com.cratonvm.jdkonly.svc.Greeter;
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
        check(provides.equals(Collections.singletonList(
                "com.cratonvm.jdkonly.svc.Greeter->2")), "provides: " + provides);

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

    public static void main(String[] args) throws Exception {
        descriptor();
        readabilityAndExports();
        encapsulation();
        resources();
        moduleServices();
        System.out.println("CK RJdkModule checks=" + checks);
        System.out.println("PASS RJdkModule (" + checks + " checks)");
    }
}
