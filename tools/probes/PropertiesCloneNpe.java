import java.util.Properties;

/**
 * `properties-clone-npe-breaks-testcontainers-docker-client-init-20260829.md`:
 * `((Properties) System.getProperties().clone())` threw NPE inside
 * `Properties.clone` → `Hashtable.clone`, taking out 138 of 191
 * hibernate-reactive classes because Testcontainers' Docker-client strategy
 * clones the system properties in its constructor.
 *
 * <p>The page asked for "a minimal standalone repro … to isolate whether this
 * needs a populated/large properties table, specific key/value types, or
 * reproduces on an empty one". That is what the arms below are, and the answer
 * matters: this VM keeps `Properties` entries in a side table and leaves the
 * JDK's private `map` field NULL BY DESIGN, so an un-overridden JDK body NPEs.
 * A `new Properties()` that was never written through can therefore behave
 * differently from `System.getProperties()`, and only one of them is the
 * reported failure.
 *
 * <p>Every row prints a value, not a boolean, so this diffs against real
 * HotSpot rather than asserting a property this VM might define differently.
 */
public class PropertiesCloneNpe {

    static void row(String name, Runnable body) {
        try {
            body.run();
            System.out.println("CK " + pad(name) + " ok");
        } catch (Throwable t) {
            System.out.println("CK " + pad(name) + " THREW " + t.getClass().getName());
        }
    }

    static String pad(String s) {
        StringBuilder b = new StringBuilder(s);
        while (b.length() < 40) {
            b.append(' ');
        }
        return b.toString();
    }

    public static void main(String[] args) {
        // 1. THE REPORTED SHAPE: Testcontainers' DefaultDockerClientConfig
        //    .createDefaultConfigBuilder does exactly this.
        row("System.getProperties().clone()", () -> {
            Properties p = (Properties) System.getProperties().clone();
            if (p.isEmpty()) {
                throw new IllegalStateException("clone lost every entry");
            }
        });

        // 2. the clone must carry the entries, not just survive
        Properties sys = System.getProperties();
        Properties cl = (Properties) sys.clone();
        System.out.println("CK " + pad("sys.size vs clone.size")
                + sys.size() + " vs " + cl.size()
                + " equal=" + (sys.size() == cl.size()));
        System.out.println("CK " + pad("clone has java.version")
                + (cl.getProperty("java.version") != null));

        // 3. a clone must not alias its source
        cl.setProperty("craton.probe.only.in.clone", "1");
        System.out.println("CK " + pad("write to clone leaks to source")
                + (sys.getProperty("craton.probe.only.in.clone") != null));

        // 4. THE OBVIOUS REPRO, which is NOT the reported one: a Properties
        //    that was never written through. If this passes while (1) fails,
        //    the difference is the receiver's backing, not `clone` itself.
        row("new Properties().clone()  [never written]", () -> {
            Properties p = new Properties();
            Object c = p.clone();
            if (c == null) {
                throw new IllegalStateException("null clone");
            }
        });

        row("new Properties()+put, then clone()", () -> {
            Properties p = new Properties();
            p.setProperty("a", "1");
            Properties c = (Properties) p.clone();
            if (!"1".equals(c.getProperty("a"))) {
                throw new IllegalStateException("clone lost the entry");
            }
        });

        // 5. the sibling the same design note covers: `replaceAll` was missing
        //    from the same "EVERY method must be overridden" set.
        row("System.getProperties() copy .replaceAll", () -> {
            Properties p = (Properties) System.getProperties().clone();
            p.replaceAll((k, v) -> v);
        });

        row("new Properties()+put .replaceAll", () -> {
            Properties p = new Properties();
            p.setProperty("a", "1");
            p.replaceAll((k, v) -> v);
            if (!"1".equals(p.getProperty("a"))) {
                throw new IllegalStateException("replaceAll lost the entry");
            }
        });

        // 6. enumeration order: source and clone must agree (the sibling page
        //    `system-properties-clone-enumerates-in-a-different-order...`).
        Properties a = System.getProperties();
        Properties b = (Properties) a.clone();
        System.out.println("CK " + pad("clone order equals source order")
                + a.stringPropertyNames().toString().equals(b.stringPropertyNames().toString()));
    }
}
