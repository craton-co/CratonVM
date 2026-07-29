import io.quarkus.bootstrap.runner.RunnerClassLoader;
import io.quarkus.bootstrap.runner.SerializedApplication;
import java.io.InputStream;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;

public class AntlrLoaderProbe {
    public static void main(String[] a) throws Exception {
        Path appRoot = Paths.get("/data/tmp/kc-dist/keycloak-26.6.1/lib");
        RunnerClassLoader cl;
        try (InputStream in = Files.newInputStream(appRoot.resolve("quarkus/quarkus-application.dat"))) {
            cl = SerializedApplication.read(in, appRoot).getRunnerClassLoader();
        }
        String[] names = {
            "org.antlr.v4.runtime.atn.ATNConfig",
            "org.antlr.v4.runtime.atn.ATNConfigSet",
            "org.antlr.v4.runtime.Lexer",
            "org.hibernate.grammars.hql.HqlLexer",
        };
        for (String n : names) {
            try {
                Class<?> c = Class.forName(n, true, cl);
                System.out.println("OK   " + n + " loader=" + c.getClassLoader());
            } catch (Throwable t) {
                Throwable r = t; while (r.getCause() != null) r = r.getCause();
                System.out.println("FAIL " + n + " -> " + r.getClass().getName() + ": " + r.getMessage());
            }
        }
        System.out.println("== DONE OK ==");
    }
}
