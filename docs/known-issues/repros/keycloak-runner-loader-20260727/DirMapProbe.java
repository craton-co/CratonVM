import io.quarkus.bootstrap.runner.RunnerClassLoader;
import io.quarkus.bootstrap.runner.SerializedApplication;

import java.io.InputStream;
import java.lang.reflect.Field;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.Map;

/** Does `RunnerClassLoader.resourceDirectoryMap` answer get() correctly? */
public class DirMapProbe {
    public static void main(String[] args) throws Exception {
        Path appRoot = Paths.get("/data/tmp/kc-dist/keycloak-26.6.1/lib");
        RunnerClassLoader cl;
        try (InputStream in = Files.newInputStream(appRoot.resolve("quarkus/quarkus-application.dat"))) {
            cl = SerializedApplication.read(in, appRoot).getRunnerClassLoader();
        }
        Field f = RunnerClassLoader.class.getDeclaredField("resourceDirectoryMap");
        f.setAccessible(true);
        @SuppressWarnings("unchecked")
        Map<String, Object[]> map = (Map<String, Object[]>) f.get(cl);
        System.out.println("resourceDirectoryMap: class=" + map.getClass().getName() + " size=" + map.size());

        String[] dirs = {
            "io/quarkus/value/registry",
            "io/quarkus/arc/setup",
            "io/quarkus/runner/recorded",
            "io/quarkus/runtime/configuration",
        };
        for (String d : dirs) {
            Object[] v = map.get(d);
            boolean scanFound = false;
            int scanLen = -1;
            for (Map.Entry<String, Object[]> e : map.entrySet()) {
                if (e.getKey().equals(d)) {
                    scanFound = true;
                    scanLen = e.getValue() == null ? -1 : e.getValue().length;
                    break;
                }
            }
            System.out.println("  get=" + (v == null ? "null" : ("len=" + v.length))
                    + " containsKey=" + map.containsKey(d)
                    + " scanFound=" + scanFound + " scanLen=" + scanLen
                    + " hash=" + d.hashCode() + "  " + d);
        }
    }
}
