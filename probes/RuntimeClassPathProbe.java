import java.lang.management.ManagementFactory;
import java.net.URLClassLoader;

/**
 * What Spring Boot's ModifiedClassPathClassLoader.doExtractUrls actually reads
 * when the process is launched through a manifest-only "pathing" JAR.
 */
public class RuntimeClassPathProbe {

    public static void main(String[] args) {
        ClassLoader own = RuntimeClassPathProbe.class.getClassLoader();
        System.out.println("own loader        = " + (own == null ? "null" : own.getClass().getName()));
        System.out.println("isURLClassLoader  = " + (own instanceof URLClassLoader));
        System.out.println("java.class.path   = " + System.getProperty("java.class.path"));
        System.out.println("RuntimeMXBean.getClassPath():");
        for (String e : ManagementFactory.getRuntimeMXBean().getClassPath().split(java.io.File.pathSeparator)) {
            System.out.println("    " + e);
        }
    }
}
