import io.quarkus.bootstrap.runner.RunnerClassLoader;
import io.quarkus.bootstrap.runner.SerializedApplication;

import java.io.InputStream;
import java.lang.reflect.Method;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.Arrays;

/** Is Quarkus's generated ApplicationImpl fully parsed (does doStart exist)? */
public class AppImplProbe {
    public static void main(String[] args) throws Exception {
        Path appRoot = Paths.get("/data/tmp/kc-dist/keycloak-26.6.1/lib");
        RunnerClassLoader cl;
        try (InputStream in = Files.newInputStream(appRoot.resolve("quarkus/quarkus-application.dat"))) {
            cl = SerializedApplication.read(in, appRoot).getRunnerClassLoader();
        }
        for (String name : new String[] {
                "io.quarkus.runner.ApplicationImpl",
                "io.quarkus.runtime.Application",
        }) {
            Class<?> c = Class.forName(name, false, cl);
            Method[] ms = c.getDeclaredMethods();
            System.out.println(name + ": loader=" + c.getClassLoader()
                    + " declaredMethods=" + ms.length
                    + " superclass=" + (c.getSuperclass() == null ? "null" : c.getSuperclass().getName()));
            String[] names = Arrays.stream(ms).map(Method::getName).distinct().sorted().toArray(String[]::new);
            System.out.println("   names=" + Arrays.toString(names));
            for (String m : new String[] {"doStart", "doStop", "start", "stop"}) {
                try {
                    Method found = c.getDeclaredMethod(m, String[].class);
                    System.out.println("   " + m + "(String[]) -> " + found);
                } catch (NoSuchMethodException e) {
                    try {
                        Method found = c.getDeclaredMethod(m);
                        System.out.println("   " + m + "() -> " + found);
                    } catch (NoSuchMethodException e2) {
                        System.out.println("   " + m + " -> ABSENT");
                    }
                }
            }
        }
    }
}
