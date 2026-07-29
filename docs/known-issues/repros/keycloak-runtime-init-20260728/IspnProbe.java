import io.quarkus.bootstrap.runner.RunnerClassLoader;
import io.quarkus.bootstrap.runner.SerializedApplication;
import java.io.InputStream;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.ServiceLoader;

public class IspnProbe {
    public static void main(String[] a) throws Exception {
        Path appRoot = Paths.get("/data/tmp/kc-dist/keycloak-26.6.1/lib");
        RunnerClassLoader cl;
        try (InputStream in = Files.newInputStream(appRoot.resolve("quarkus/quarkus-application.dat"))) {
            cl = SerializedApplication.read(in, appRoot).getRunnerClassLoader();
        }
        Thread.currentThread().setContextClassLoader(cl);
        System.out.println("version.properties visible=" + (cl.getResource("META-INF/infinispan-version.properties") != null));
        Class<?> v = Class.forName("org.infinispan.commons.util.Version", true, cl);
        System.out.println("Version.getVersion()=" + v.getMethod("getVersion").invoke(null));
        System.out.println("Version.getMajorMinor()=" + v.getMethod("getMajorMinor").invoke(null));
        Class<?> parser = Class.forName("org.infinispan.configuration.parsing.ConfigurationParser", true, cl);
        int n = 0;
        for (Object o : ServiceLoader.load(parser, cl)) { n++; }
        System.out.println("ConfigurationParser services=" + n);
        java.util.Enumeration<java.net.URL> e = cl.getResources("META-INF/services/org.infinispan.configuration.parsing.ConfigurationParser");
        int u = 0; while (e.hasMoreElements()) { e.nextElement(); u++; }
        System.out.println("service files=" + u);
        System.out.println("== DONE OK ==");
    }
}
