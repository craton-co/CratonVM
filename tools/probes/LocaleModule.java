import java.lang.module.ModuleFinder;
import java.util.Optional;
import java.util.ServiceLoader;
import java.util.Set;
import java.util.TreeSet;

/**
 * Is `jdk.localedata` present at all, and if so is its LocaleDataMetaInfo
 * service provider visible? The CLDR adapter reports 5 supported locales
 * instead of 1063, and those 5 are exactly java.base's own set, so the question
 * is whether the supplementary module is missing from the graph, present but
 * not readable, or present with its service provider not discoverable.
 *
 * Each of those three has a different fix, so the probe separates them instead
 * of just reporting "locale data is broken".
 */
public class LocaleModule {

    public static void main(String[] args) {
        System.out.println("java.version = " + System.getProperty("java.version"));

        // 1. Is it in the BOOT layer (resolved and readable)?
        Optional<Module> boot = ModuleLayer.boot().findModule("jdk.localedata");
        System.out.println("1. boot layer has jdk.localedata = " + boot.isPresent());
        boot.ifPresent(m -> System.out.println("     descriptor = " + m.getDescriptor().name()
                + "  provides=" + m.getDescriptor().provides().size()));

        // 2. Is it OBSERVABLE at all (in the runtime image, even if not resolved)?
        try {
            boolean observable = ModuleFinder.ofSystem().find("jdk.localedata").isPresent();
            System.out.println("2. system ModuleFinder sees jdk.localedata = " + observable);
        } catch (Throwable t) {
            System.out.println("2. system ModuleFinder -> " + t.getClass().getName() + ": " + t.getMessage());
        }

        // 3. How many modules are in the boot layer at all? A tiny number means
        //    the graph itself is the problem, not this one module.
        try {
            Set<String> names = new TreeSet<>();
            for (Module m : ModuleLayer.boot().modules()) {
                names.add(m.getName());
            }
            System.out.println("3. boot layer module count = " + names.size());
            StringBuilder sb = new StringBuilder();
            for (String n : names) {
                if (n.startsWith("jdk.local") || n.equals("java.base") || n.startsWith("jdk.charsets")) {
                    if (sb.length() > 0) sb.append(", ");
                    sb.append(n);
                }
            }
            System.out.println("     of interest: [" + sb + "]");
        } catch (Throwable t) {
            System.out.println("3. boot modules -> " + t.getClass().getName());
        }

        // 4. Is the service provider discoverable? This is what the CLDR adapter
        //    actually consumes.
        try {
            Class<?> spi = Class.forName("sun.util.locale.provider.LocaleDataMetaInfo");
            int n = 0;
            StringBuilder sb = new StringBuilder();
            for (Object o : ServiceLoader.load(spi)) {
                n++;
                if (sb.length() > 0) sb.append(", ");
                sb.append(o.getClass().getName());
            }
            System.out.println("4. ServiceLoader<LocaleDataMetaInfo> count = " + n + "  [" + sb + "]");
        } catch (Throwable t) {
            System.out.println("4. ServiceLoader<LocaleDataMetaInfo> -> "
                    + t.getClass().getName() + ": " + t.getMessage());
        }

        // 5. Can the supplementary metadata class be loaded by name at all?
        for (String cn : new String[] {
                "sun.util.resources.cldr.provider.CLDRLocaleDataMetaInfo",
                "sun.util.cldr.CLDRBaseLocaleDataMetaInfo" }) {
            try {
                Class<?> c = Class.forName(cn);
                System.out.println("5. Class.forName(" + cn + ") = OK  module="
                        + c.getModule().getName());
            } catch (Throwable t) {
                System.out.println("5. Class.forName(" + cn + ") -> " + t.getClass().getName());
            }
        }
    }
}
