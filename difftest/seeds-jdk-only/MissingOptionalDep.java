// difftest: strict
//
// JDK-only boundary vector: the missing optional dependency.
//
// This is the vector `--jdk-only` exists for. The names below are NOT on the
// classpath, so the specification-correct outcome is a `ClassNotFoundException`
// whose message is the requested class name — which is exactly what HotSpot
// does. CratonVM's compatibility mode has historically fabricated a class for
// the enterprise prefixes (`org/jboss`, `io/quarkus`, `io/smallrye`; see
// docs/feature-designs/jdk-only-mode.md §1's terminology table, "enterprise
// fallback stub ... forbidden"), so this seed is expected to diverge in
// `real-compatible-*` and to *agree* in `jdk-only-*` once §5 lands. Both
// outcomes are recorded, on separate ledger rows, because the ledger key is
// (class, jdk_profile).
//
// Every branch prints a stable token, so a fabricated class shows up as a
// `LOADED` line where HotSpot printed `CNFE`, and a wrong error type shows up as
// `OTHER`. Nothing is caught silently.
public class MissingOptionalDep {

    /** Names that must not resolve: an app name plus the enterprise prefixes. */
    private static final String[] ABSENT = {
        "com.example.optional.NotThere",
        "org.jboss.logging.Logger",
        "org.jboss.logmanager.LogContext",
        "io.quarkus.runtime.Application",
        "io.smallrye.config.SmallRyeConfig",
        "org.apache.commons.logging.LogFactory",
    };

    public static void main(String[] args) {
        for (String name : ABSENT) {
            try {
                Class<?> c = Class.forName(name);
                // A fabricated stand-in lands here; HotSpot never does.
                System.out.println("LOADED " + name + " -> " + c.getName());
            } catch (ClassNotFoundException e) {
                System.out.println("CNFE " + name + ": " + e.getMessage());
            } catch (LinkageError e) {
                System.out.println("LINKAGE " + name + ": " + e.getClass().getName());
            } catch (Throwable t) {
                System.out.println("OTHER " + name + ": " + t.getClass().getName());
            }
        }

        ClassLoader app = MissingOptionalDep.class.getClassLoader();
        String probe = "com.example.optional.NotThere";

        // initialize=false must not change the outcome.
        try {
            Class.forName(probe, false, app);
            System.out.println("LOADED-noinit");
        } catch (ClassNotFoundException e) {
            System.out.println("CNFE-noinit: " + e.getMessage());
        } catch (Throwable t) {
            System.out.println("OTHER-noinit: " + t.getClass().getName());
        }

        // The loader's own entry point must agree with Class.forName.
        try {
            app.loadClass(probe);
            System.out.println("LOADED-loadClass");
        } catch (ClassNotFoundException e) {
            System.out.println("CNFE-loadClass: " + e.getMessage());
        } catch (Throwable t) {
            System.out.println("OTHER-loadClass: " + t.getClass().getName());
        }

        // The platform and boot loaders must not conjure it either.
        try {
            Class.forName(probe, false, ClassLoader.getPlatformClassLoader());
            System.out.println("LOADED-platform");
        } catch (ClassNotFoundException e) {
            System.out.println("CNFE-platform: " + e.getMessage());
        } catch (Throwable t) {
            System.out.println("OTHER-platform: " + t.getClass().getName());
        }

        // A resource lookup for the same name must be absent too, or the class
        // was "found" by one half of the loader and not the other.
        System.out.println("resource-absent: "
                + (app.getResource("com/example/optional/NotThere.class") == null));

        // Array-of-missing must fail the same way (arrays are VM-created, but
        // the *component* still has to resolve).
        try {
            Class.forName("[Lcom.example.optional.NotThere;", false, app);
            System.out.println("LOADED-array");
        } catch (ClassNotFoundException e) {
            System.out.println("CNFE-array: true");
        } catch (Throwable t) {
            System.out.println("OTHER-array: " + t.getClass().getName());
        }

        // A real JDK class on the same code path must still resolve, so a
        // regression that breaks loading outright is distinguishable from the
        // strict refusal this seed is about.
        try {
            System.out.println("CONTROL " + Class.forName("java.util.ArrayList").getName());
        } catch (ClassNotFoundException e) {
            System.out.println("CONTROL-FAILED: " + e.getMessage());
        }

        System.out.println("done");
    }
}
