import java.io.BufferedReader;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.net.URL;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.Enumeration;
import java.util.Iterator;
import java.util.List;
import java.util.NoSuchElementException;
import java.util.Optional;
import java.util.ServiceConfigurationError;
import java.util.ServiceLoader;
import java.util.stream.Collectors;

/**
 * JDK-only corpus: {@code ServiceLoader} -- CLASS PATH provider discovery.
 * (The module-path half lives in {@link RJdkModule}.)
 *
 * The provider list is a real {@code META-INF/services/...} resource shipped in
 * {@code regression-suite/resources/} and copied into the build directory by
 * {@code run.sh}; discovery therefore goes through the real
 * {@code ClassLoader.getResources} path, not a fabricated shortcut.
 *
 * Determinism: {@code ServiceLoader} iteration order across multiple config
 * files is unspecified, so every emitted list is sorted.
 */
public class RJdkServices {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /** The service type. Providers are listed in META-INF/services/RJdkServices$Greeting. */
    public interface Greeting {
        String greet();
    }

    /** Provider discovered through its public no-arg constructor. */
    public static final class En implements Greeting {
        public En() {
        }

        @Override
        public String greet() {
            return "hello";
        }
    }

    /** Provider discovered through its public no-arg constructor. */
    public static final class Fr implements Greeting {
        public Fr() {
        }

        @Override
        public String greet() {
            return "bonjour";
        }
    }

    /**
     * A third constructor-discovered provider.
     *
     * NB: the static {@code provider()} FACTORY form is only honoured for
     * providers deployed in a NAMED MODULE -- on the class path the provider
     * class must itself be a subtype of the service, or ServiceLoader fails
     * with "not a subtype". The factory form is covered by {@link RJdkModule}.
     */
    public static final class De implements Greeting {
        public De() {
        }

        @Override
        public String greet() {
            return "hallo";
        }
    }

    /** A service whose config file names a class that does not exist. */
    public interface Broken {
        void run();
    }

    static void classPathDiscovery() {
        ClassLoader loader = RJdkServices.class.getClassLoader();
        ServiceLoader<Greeting> sl = ServiceLoader.load(Greeting.class, loader);
        List<String> greets = new ArrayList<>();
        List<String> types = new ArrayList<>();
        for (Greeting g : sl) {
            greets.add(g.greet());
            types.add(g.getClass().getSimpleName());
        }
        Collections.sort(greets);
        check(greets.equals(Arrays.asList("bonjour", "hallo", "hello")),
                "discovered providers: " + greets);
        check(types.size() == 3, "provider count: " + types);

        Collections.sort(types);
        check(types.equals(Arrays.asList("De", "En", "Fr")), "provider classes: " + types);

        // stream() exposes the provider TYPE without instantiating it.
        List<String> lazy = ServiceLoader.load(Greeting.class, loader).stream()
                .map(p -> p.type().getSimpleName())
                .sorted()
                .collect(Collectors.toList());
        check(lazy.equals(Arrays.asList("De", "En", "Fr")), "stream types: " + lazy);

        // Each load() call gets fresh instances; reload() clears the cache.
        Greeting first = ServiceLoader.load(Greeting.class, loader).iterator().next();
        Greeting second = ServiceLoader.load(Greeting.class, loader).iterator().next();
        check(first != second, "each ServiceLoader must create its own instances");
        ServiceLoader<Greeting> reloadable = ServiceLoader.load(Greeting.class, loader);
        Greeting a = reloadable.iterator().next();
        Greeting b = reloadable.iterator().next();
        check(a == b, "a single ServiceLoader caches its instances");
        reloadable.reload();
        check(reloadable.iterator().next() != a, "reload() must discard the cache");

        Optional<Greeting> found = ServiceLoader.load(Greeting.class, loader).findFirst();
        check(found.isPresent(), "findFirst");
        // `.length() > 0` was satisfied by ANY non-empty string, including one
        // from a fabricated stand-in that is not one of the three configured
        // providers at all. The product has to be one of the greetings the
        // discovery above already pinned by exact value.
        check(greets.contains(found.get().greet()),
                "findFirst product is not one of the discovered providers: "
                        + found.get().greet());

        // Exhausted iterator behaviour.
        Iterator<Greeting> it = ServiceLoader.load(Greeting.class, loader).iterator();
        int n = 0;
        while (it.hasNext()) {
            it.next();
            n++;
        }
        check(n == 3, "iterator yielded " + n);
        boolean threw = false;
        try {
            it.next();
        } catch (NoSuchElementException expected) {
            threw = true;
        }
        check(threw, "an exhausted ServiceLoader iterator must throw NoSuchElementException");
        System.out.println("CK RJdkServices greets=" + greets + " types=" + lazy);
    }

    static void configResource() throws Exception {
        ClassLoader loader = RJdkServices.class.getClassLoader();
        String path = "META-INF/services/RJdkServices$Greeting";
        URL url = loader.getResource(path);
        check(url != null, "the service config resource must be visible: " + path);
        Enumeration<URL> all = loader.getResources(path);
        int count = 0;
        while (all.hasMoreElements()) {
            all.nextElement();
            count++;
        }
        check(count == 1, "expected exactly one config file, found " + count);

        List<String> lines = new ArrayList<>();
        try (InputStream in = loader.getResourceAsStream(path);
                BufferedReader r = new BufferedReader(
                        new InputStreamReader(in, StandardCharsets.UTF_8))) {
            String line;
            while ((line = r.readLine()) != null) {
                line = line.trim();
                if (!line.isEmpty() && !line.startsWith("#")) {
                    lines.add(line);
                }
            }
        }
        Collections.sort(lines);
        check(lines.equals(Arrays.asList(
                "RJdkServices$De", "RJdkServices$En", "RJdkServices$Fr")),
                "config entries: " + lines);
        // The URL is host-specific; only its shape is asserted, never printed.
        check(url.toString().endsWith(path), "resource URL shape");
        System.out.println("CK RJdkServices config=" + lines);
    }

    static void badProvider() {
        ClassLoader loader = RJdkServices.class.getClassLoader();
        ServiceLoader<Broken> sl = ServiceLoader.load(Broken.class, loader);
        boolean threw = false;
        String kind = "";
        try {
            for (Broken b : sl) {
                b.run();
            }
        } catch (ServiceConfigurationError e) {
            threw = true;
            // The message embeds the provider class name; the CAUSE type is the
            // portable part, so that is what we assert and print.
            kind = e.getCause() == null ? "none" : e.getCause().getClass().getSimpleName();
        }
        check(threw, "a config file naming a missing class must raise ServiceConfigurationError");
        check(kind.equals("ClassNotFoundException") || kind.equals("none"),
                "unexpected ServiceConfigurationError cause: " + kind);

        // A service type with no providers at all yields an empty, non-null loader.
        ServiceLoader<Runnable> none = ServiceLoader.load(Runnable.class, loader);
        check(!none.iterator().hasNext(), "a service with no providers must iterate empty");
        check(none.findFirst().isEmpty(), "findFirst on an empty service");
        System.out.println("CK RJdkServices badProviderCause=" + kind);
    }

    public static void main(String[] args) throws Exception {
        classPathDiscovery();
        configResource();
        badProvider();
        System.out.println("CK RJdkServices checks=" + checks);
        System.out.println("PASS RJdkServices (" + checks + " checks)");
    }
}
