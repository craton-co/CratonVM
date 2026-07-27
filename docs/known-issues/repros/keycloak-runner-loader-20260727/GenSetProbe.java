import io.quarkus.bootstrap.runner.RunnerClassLoader;
import io.quarkus.bootstrap.runner.SerializedApplication;

import java.io.InputStream;
import java.lang.reflect.Field;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.Set;

/** Is `RunnerClassLoader.generatedBytecode` (a HashSet<String>) answering contains() correctly? */
public class GenSetProbe {
    public static void main(String[] args) throws Exception {
        Path appRoot = Paths.get("/data/tmp/kc-dist/keycloak-26.6.1/lib");
        RunnerClassLoader cl;
        try (InputStream in = Files.newInputStream(appRoot.resolve("quarkus/quarkus-application.dat"))) {
            cl = SerializedApplication.read(in, appRoot).getRunnerClassLoader();
        }
        Field f = RunnerClassLoader.class.getDeclaredField("generatedBytecode");
        f.setAccessible(true);
        @SuppressWarnings("unchecked")
        Set<String> gen = (Set<String>) f.get(cl);
        System.out.println("generatedBytecode set: class=" + gen.getClass().getName() + " size=" + gen.size());

        String[] names = {
            "io/quarkus/runner/recorded/ArcProcessor$initializeContainer643029769.class",
            "io/quarkus/value/registry/ValueRegistry_Vjm2hphPTUShgv9MdpQ7IxocVcI_Synthetic_Bean.class",
            "io/quarkus/value/registry/ValueRegistry_Observer_Synthetic_i8SkhdhAV_w_PxAJmk32fKwyL0I.class",
            "io/quarkus/arc/setup/Default_ComponentsProvider_addBeans0.class",
        };
        for (String n : names) {
            boolean hashed = gen.contains(n);
            boolean linear = false;
            String hit = null;
            for (String s : gen) {
                if (s.equals(n)) { linear = true; hit = s; break; }
            }
            System.out.println("  contains=" + hashed + " scan=" + linear
                    + " hash=" + n.hashCode() + (hit != null ? " hitHash=" + hit.hashCode() : "")
                    + "  " + n);
        }
    }
}
