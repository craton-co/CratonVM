import java.io.File;
import java.lang.reflect.Method;
import java.net.URL;
import java.net.URLClassLoader;

/**
 * Residual probe for the CGLIB loader-id family
 * (docs/known-issues/springboot/configproxy-cglib-loaderid-fixed-20260727.md).
 *
 * Spring generates a subclass for a bean class that declares `@Lookup`
 * methods, and a wrapper subclass for a concrete `FactoryBean`. CratonVM's
 * native reimplementations used to define both into the APPLICATION loader
 * unconditionally. That is only correct while the bean class itself is
 * app-loaded; for a fork-loaded one the generated subclass lands in a
 * different runtime package than its own superclass, so any package-private
 * override it declares stops being an override (`same_runtime_package`,
 * JVMS 5.4.4) -- exactly the defect the `@Configuration` enhancer hit.
 *
 * Usage: LookupForkDiag app            (probe classes on -cp)
 *        LookupForkDiag fork <dir>     (probe classes ONLY under <dir>)
 */
public class LookupForkDiag {

    static int pass = 0;
    static int fail = 0;

    static void check(String name, boolean ok, Object detail) {
        if (ok) {
            pass++;
            System.out.println("[T] " + name + ": PASS");
        } else {
            fail++;
            System.out.println("[T] " + name + ": FAIL " + detail);
        }
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "app";
        ClassLoader loader;
        if ("fork".equals(mode)) {
            File forkDir = new File(args[1]);
            loader = new URLClassLoader(new URL[] { forkDir.toURI().toURL() },
                    LookupForkDiag.class.getClassLoader());
        } else {
            loader = LookupForkDiag.class.getClassLoader();
        }
        runScenario(mode, loader);
        System.out.println("SUITE LookupForkDiag-" + mode + " passed=" + pass
                + " failed=" + fail + " total=" + (pass + fail));
        System.exit(0);
    }

    static void runScenario(String tag, ClassLoader loader) {
        try {
            Class<?> driver = Class.forName("probe.Driver", true, loader);
            check(tag + ".driverLoader", driver.getClassLoader() == loader,
                    "expected " + loader + " got " + driver.getClassLoader());
            Method run = driver.getDeclaredMethod("run");
            run.setAccessible(true);
            Object result = run.invoke(null);
            for (String p : String.valueOf(result).split(";")) {
                int eq = p.indexOf('=');
                if (eq < 0) {
                    continue;
                }
                check(tag + "." + p.substring(0, eq), "true".equals(p.substring(eq + 1)), p);
            }
        } catch (Throwable t) {
            Throwable c = t;
            while (c.getCause() != null) {
                c = c.getCause();
            }
            check(tag + ".scenario", false, "exception=" + c);
        }
    }
}
