import io.quarkus.bootstrap.runner.RunnerClassLoader;
import io.quarkus.bootstrap.runner.SerializedApplication;
import io.quarkus.commons.classloading.ClassLoaderHelper;

import java.io.InputStream;
import java.lang.reflect.Field;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.Set;

public class JdkPkgProbe {
    public static void main(String[] args) throws Exception {
        String[] names = {
            "io.quarkus.runner.recorded.ArcProcessor$initializeContainer643029769",
            "io.quarkus.value.registry.ValueRegistry_Vjm2hphPTUShgv9MdpQ7IxocVcI_Synthetic_Bean",
            "io.quarkus.arc.setup.Default_ComponentsProvider_addBeans0",
        };
        for (String n : names) {
            System.out.println("isInJdkPackage=" + ClassLoaderHelper.isInJdkPackage(n)
                    + " resourceName=" + ClassLoaderHelper.fromClassNameToResourceName(n));
        }
        Path appRoot = Paths.get("/data/tmp/kc-dist/keycloak-26.6.1/lib");
        RunnerClassLoader cl;
        try (InputStream in = Files.newInputStream(appRoot.resolve("quarkus/quarkus-application.dat"))) {
            cl = SerializedApplication.read(in, appRoot).getRunnerClassLoader();
        }
        Field pf = RunnerClassLoader.class.getDeclaredField("parentFirstPackages");
        pf.setAccessible(true);
        @SuppressWarnings("unchecked")
        Set<String> parentFirst = (Set<String>) pf.get(cl);
        System.out.println("parentFirstPackages size=" + parentFirst.size());
        for (String p : new String[] {"io.quarkus.value.registry", "io.quarkus.arc.setup",
                                      "io.quarkus.runner.recorded"}) {
            System.out.println("  parentFirst.contains(" + p + ")=" + parentFirst.contains(p));
        }
    }
}
