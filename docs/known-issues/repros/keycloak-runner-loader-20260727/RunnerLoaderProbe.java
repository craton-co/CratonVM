import io.quarkus.bootstrap.runner.RunnerClassLoader;
import io.quarkus.bootstrap.runner.SerializedApplication;

import java.io.InputStream;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;

/** Isolated repro for "Quarkus RunnerClassLoader cannot find a generated-bytecode class". */
public class RunnerLoaderProbe {
    public static void main(String[] args) throws Exception {
        Path appRoot = Paths.get("/data/tmp/kc-dist/keycloak-26.6.1/lib");
        RunnerClassLoader cl;
        try (InputStream in = Files.newInputStream(appRoot.resolve("quarkus/quarkus-application.dat"))) {
            SerializedApplication app = SerializedApplication.read(in, appRoot);
            cl = app.getRunnerClassLoader();
        }
        String[] names = {
            "io.quarkus.runner.recorded.ArcProcessor$initializeContainer643029769",
            "io.quarkus.value.registry.ValueRegistry_Vjm2hphPTUShgv9MdpQ7IxocVcI_Synthetic_Bean",
            "io.quarkus.value.registry.ValueRegistry_Observer_Synthetic_i8SkhdhAV_w_PxAJmk32fKwyL0I",
            "io.quarkus.arc.setup.Default_ComponentsProvider_addBeans0",
            "io.quarkus.runtime.configuration.MemorySize",
        };
        for (String n : names) {
            String result;
            try {
                Class<?> c = cl.loadClass(n);
                result = "OK loader=" + c.getClassLoader();
            } catch (Throwable t) {
                result = "FAIL " + t.getClass().getName() + ": " + t.getMessage();
            }
            System.out.println(result + "   <- " + n);
        }
    }
}
