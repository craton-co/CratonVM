import io.quarkus.bootstrap.runner.RunnerClassLoader;
import io.quarkus.bootstrap.runner.SerializedApplication;

import java.io.InputStream;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.Collection;
import java.util.List;
import java.util.Map;
import java.util.ServiceLoader;

/**
 * Isolated repro for "Arc reports No matching bean found for every type".
 * Builds the real Quarkus RunnerClassLoader, then drives Arc's own bootstrap
 * the way ArcRecorder.initContainer does, and reports how many
 * ComponentsProviders / beans the container ended up with.
 */
public class ArcInitProbe {
    public static void main(String[] args) throws Exception {
        Path appRoot = Paths.get("/data/tmp/kc-dist/keycloak-26.6.1/lib");
        RunnerClassLoader cl;
        try (InputStream in = Files.newInputStream(appRoot.resolve("quarkus/quarkus-application.dat"))) {
            cl = SerializedApplication.read(in, appRoot).getRunnerClassLoader();
        }
        Thread.currentThread().setContextClassLoader(cl);

        // 1) Raw ServiceLoader discovery of ComponentsProvider — what
        //    ArcContainerImpl's constructor does first.
        Class<?> cpClass = cl.loadClass("io.quarkus.arc.ComponentsProvider");
        int viaTccl = 0;
        for (Object o : ServiceLoader.load(cpClass, cl)) {
            viaTccl++;
            System.out.println("  ComponentsProvider: " + o.getClass().getName());
        }
        System.out.println("ServiceLoader(ComponentsProvider, runnerCL) -> " + viaTccl);

        // 2) The resource the ServiceLoader reads, straight from the loader.
        java.util.Enumeration<java.net.URL> urls =
                cl.getResources("META-INF/services/io.quarkus.arc.ComponentsProvider");
        int urlCount = 0;
        while (urls.hasMoreElements()) {
            System.out.println("  services URL: " + urls.nextElement());
            urlCount++;
        }
        System.out.println("getResources(META-INF/services/...ComponentsProvider) -> " + urlCount);

        // 3) Drive Arc itself.
        Class<?> arc = cl.loadClass("io.quarkus.arc.Arc");
        Object container;
        try {
            Method init = arc.getMethod("initialize");
            container = init.invoke(null);
        } catch (Throwable t) {
            Throwable c = t.getCause() != null ? t.getCause() : t;
            System.out.println("Arc.initialize() FAILED: " + c);
            c.printStackTrace(System.out);
            return;
        }
        System.out.println("Arc.initialize() -> " + container);

        Class<?> impl = cl.loadClass("io.quarkus.arc.impl.ArcContainerImpl");
        for (String fname : new String[] {"beans", "interceptors", "observers", "beansById", "beansByName"}) {
            try {
                Field f = impl.getDeclaredField(fname);
                f.setAccessible(true);
                Object v = f.get(container);
                String size = (v instanceof Collection) ? String.valueOf(((Collection<?>) v).size())
                        : (v instanceof Map) ? String.valueOf(((Map<?, ?>) v).size())
                        : String.valueOf(v);
                System.out.println("  ArcContainerImpl." + fname + " size=" + size);
            } catch (NoSuchFieldException e) {
                System.out.println("  ArcContainerImpl." + fname + " <no such field>");
            }
        }

        // 4) Resolve something concrete, the way BeanContainerImpl does.
        Class<?> arcContainer = cl.loadClass("io.quarkus.arc.ArcContainer");
        Method instance = arcContainer.getMethod("instance", Class.class, java.lang.annotation.Annotation[].class);
        for (String type : new String[] {
                "io.quarkus.rest.runtime.__QuarkusInit",
                "org.jboss.resteasy.reactive.server.providers.serialisers.ServerStringMessageBodyHandler",
                "org.keycloak.quarkus.runtime.KeycloakMain",
        }) {
            try {
                Class<?> t = cl.loadClass(type);
                Object handle = instance.invoke(container, t, new java.lang.annotation.Annotation[0]);
                Method avail = handle.getClass().getMethod("isAvailable");
                avail.setAccessible(true);
                System.out.println("  instance(" + t.getSimpleName() + ") available="
                        + avail.invoke(handle) + " loader=" + t.getClassLoader());
            } catch (Throwable t2) {
                System.out.println("  instance(" + type + ") threw " + t2);
            }
        }
    }
}
