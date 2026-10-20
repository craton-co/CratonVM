import java.util.ServiceLoader;

/**
 * ServiceLoader.load uses the thread context (application) loader;
 * ServiceLoader.loadInstalled uses the PLATFORM loader and is what
 * CLDRLocaleProviderAdapter uses in its static initialiser to find the
 * supplementary LocaleDataMetaInfo. A probe that calls plain load() therefore
 * cannot see the failure the adapter hits -- which is why the module graph and
 * the provider data both looked perfect while the adapter still reported 5
 * locales.
 *
 * java.sql.Driver is a second, unrelated service as a cross-check: if
 * loadInstalled is empty for EVERY service and not just this one, the defect is
 * in loadInstalled itself rather than in anything locale-specific.
 */
public class LoadInstalled {

    static void probe(String svc) {
        Class<?> spi;
        try {
            spi = Class.forName(svc);
        } catch (Throwable t) {
            System.out.println(svc + " -> not loadable: " + t.getClass().getName());
            return;
        }
        int a = 0;
        StringBuilder sa = new StringBuilder();
        try {
            for (Object o : ServiceLoader.load(spi)) {
                a++;
                if (sa.length() > 0) sa.append(", ");
                sa.append(o.getClass().getSimpleName());
            }
        } catch (Throwable t) {
            sa.append("<").append(t.getClass().getSimpleName()).append(">");
        }
        int b = 0;
        StringBuilder sb = new StringBuilder();
        try {
            for (Object o : ServiceLoader.loadInstalled(spi)) {
                b++;
                if (sb.length() > 0) sb.append(", ");
                sb.append(o.getClass().getSimpleName());
            }
        } catch (Throwable t) {
            sb.append("<").append(t.getClass().getSimpleName()).append(">");
        }
        System.out.println(svc);
        System.out.println("    load()          = " + a + "  [" + sa + "]");
        System.out.println("    loadInstalled() = " + b + "  [" + sb + "]");
    }

    public static void main(String[] args) {
        System.out.println("platform loader = " + ClassLoader.getPlatformClassLoader());
        System.out.println("system   loader = " + ClassLoader.getSystemClassLoader());
        probe("sun.util.locale.provider.LocaleDataMetaInfo");
        probe("java.nio.file.spi.FileSystemProvider");
        probe("java.time.chrono.Chronology");
    }
}
