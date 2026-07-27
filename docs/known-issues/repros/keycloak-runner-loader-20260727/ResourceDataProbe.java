import io.quarkus.bootstrap.runner.ClassLoadingResource;
import io.quarkus.bootstrap.runner.RunnerClassLoader;
import io.quarkus.bootstrap.runner.SerializedApplication;

import java.io.InputStream;
import java.lang.reflect.Field;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.Map;

/** Which ClassLoadingResource in the directory returns the class bytes? */
public class ResourceDataProbe {
    public static void main(String[] args) throws Exception {
        Path appRoot = Paths.get("/data/tmp/kc-dist/keycloak-26.6.1/lib");
        RunnerClassLoader cl;
        try (InputStream in = Files.newInputStream(appRoot.resolve("quarkus/quarkus-application.dat"))) {
            cl = SerializedApplication.read(in, appRoot).getRunnerClassLoader();
        }
        Field f = RunnerClassLoader.class.getDeclaredField("resourceDirectoryMap");
        f.setAccessible(true);
        @SuppressWarnings("unchecked")
        Map<String, ClassLoadingResource[]> map = (Map<String, ClassLoadingResource[]>) f.get(cl);
        Field gf = RunnerClassLoader.class.getDeclaredField("generatedBytecodeClassLoadingResource");
        gf.setAccessible(true);
        Object genRes = gf.get(cl);

        String[][] cases = {
            {"io/quarkus/value/registry",
             "io/quarkus/value/registry/ValueRegistry_Vjm2hphPTUShgv9MdpQ7IxocVcI_Synthetic_Bean.class"},
            {"io/quarkus/arc/setup",
             "io/quarkus/arc/setup/Default_ComponentsProvider_addBeans0.class"},
        };
        for (String[] c : cases) {
            ClassLoadingResource[] rs = map.get(c[0]);
            System.out.println(c[0] + " -> " + (rs == null ? "null" : rs.length + " resources"));
            if (rs == null) continue;
            for (int i = 0; i < rs.length; i++) {
                ClassLoadingResource r = rs[i];
                byte[] data;
                String err = "";
                try {
                    data = r.getResourceData(c[1]);
                } catch (Throwable t) {
                    data = null;
                    err = " EX:" + t;
                }
                System.out.println("   [" + i + "] " + r + " isGenerated=" + (r == genRes)
                        + " data=" + (data == null ? "null" : String.valueOf(data.length)) + err);
            }
        }
    }
}
