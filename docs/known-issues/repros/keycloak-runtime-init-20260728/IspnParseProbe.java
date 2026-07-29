import io.quarkus.bootstrap.runner.RunnerClassLoader;
import io.quarkus.bootstrap.runner.SerializedApplication;
import java.io.InputStream;
import java.lang.reflect.Constructor;
import java.lang.reflect.Method;
import java.net.URL;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;

public class IspnParseProbe {
    public static void main(String[] a) throws Exception {
        Path appRoot = Paths.get("/data/tmp/kc-dist/keycloak-26.6.1/lib");
        RunnerClassLoader cl;
        try (InputStream in = Files.newInputStream(appRoot.resolve("quarkus/quarkus-application.dat"))) {
            cl = SerializedApplication.read(in, appRoot).getRunnerClassLoader();
        }
        Thread.currentThread().setContextClassLoader(cl);
        URL xml = cl.getResource("cache-local.xml");
        System.out.println("cache-local.xml=" + xml);
        Class<?> reg = Class.forName("org.infinispan.configuration.parsing.ParserRegistry", true, cl);
        Constructor<?> ctor = reg.getConstructor(ClassLoader.class);
        Object r = ctor.newInstance(cl);
        try {
            Method m = reg.getMethod("parse", URL.class);
            Object holder = m.invoke(r, xml);
            System.out.println("parse OK -> " + holder.getClass().getName());
        } catch (Throwable t) {
            Throwable c = t; while (c.getCause() != null) c = c.getCause();
            System.out.println("parse FAIL " + c.getClass().getName() + ": " + c.getMessage());
        }
        System.out.println("== DONE OK ==");
    }
}
