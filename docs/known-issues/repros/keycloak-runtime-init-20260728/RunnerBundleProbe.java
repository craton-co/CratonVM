import io.quarkus.bootstrap.runner.RunnerClassLoader;
import io.quarkus.bootstrap.runner.SerializedApplication;

import java.io.InputStream;
import java.net.URL;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.Locale;
import java.util.ResourceBundle;

/** Isolated repro: ResourceBundle lookup through Quarkus's RunnerClassLoader. */
public class RunnerBundleProbe {
    public static void main(String[] args) throws Exception {
        System.out.println("Locale.getDefault()=" + Locale.getDefault());
        Path appRoot = Paths.get("/data/tmp/kc-dist/keycloak-26.6.1/lib");
        RunnerClassLoader cl;
        try (InputStream in = Files.newInputStream(appRoot.resolve("quarkus/quarkus-application.dat"))) {
            SerializedApplication app = SerializedApplication.read(in, appRoot);
            cl = app.getRunnerClassLoader();
        }
        String res = "liquibase/i18n/liquibase-core.properties";
        URL u = cl.getResource(res);
        System.out.println("cl.getResource(" + res + ")=" + u);
        InputStream is = cl.getResourceAsStream(res);
        System.out.println("cl.getResourceAsStream != null ? " + (is != null));
        if (is != null) is.close();

        try {
            ResourceBundle b = ResourceBundle.getBundle("liquibase/i18n/liquibase-core", Locale.getDefault(), cl);
            System.out.println("getBundle(name,locale,cl) OK keys=" + b.keySet().size());
        } catch (Throwable t) {
            System.out.println("getBundle(name,locale,cl) FAIL " + t.getClass().getName() + ": " + t.getMessage());
        }

        // The real Liquibase idiom: a class DEFINED BY the runner loader calls
        // ResourceBundle.getBundle(String), which resolves against the caller's
        // own ClassLoader via Reflection.getCallerClass().
        try {
            Class<?> c = cl.loadClass("liquibase.util.StringUtil");
            System.out.println("liquibase.util.StringUtil loader=" + c.getClassLoader());
        } catch (Throwable t) {
            System.out.println("loadClass(liquibase.util.StringUtil) FAIL " + t);
        }
        try {
            Class<?> c = cl.loadClass("liquibase.Scope");
            System.out.println("liquibase.Scope loader=" + c.getClassLoader());
        } catch (Throwable t) {
            System.out.println("loadClass(liquibase.Scope) FAIL " + t);
        }
        // CoreBundle-style static init through the runner loader.
        try {
            Class<?> c = cl.loadClass("liquibase.util.StringUtil");
            Class<?> probe = cl.loadClass("liquibase.exception.CommandLineParsingException");
            System.out.println("loaded " + probe.getName());
        } catch (Throwable t) {
            System.out.println("liquibase probe FAIL " + t);
        }
        // The real Liquibase idiom: a class DEFINED BY the runner loader runs
        // ResourceBundle.getBundle(String) in its <clinit>, which resolves the
        // bundle against the CALLER class's own ClassLoader.
        for (String n : new String[] { "liquibase.lockservice.StandardLockService", "liquibase.Liquibase" }) {
            try {
                Class<?> c = Class.forName(n, true, cl);
                System.out.println("clinit OK " + c.getName());
            } catch (Throwable t) {
                Throwable r = t; while (r.getCause() != null) r = r.getCause();
                System.out.println("clinit FAIL " + n + " -> " + r.getClass().getName() + ": " + r.getMessage());
            }
        }
        System.out.println("== DONE OK ==");
    }
}
