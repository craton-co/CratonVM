/**
 * ServiceLoader matches a module-provided service by the MODULE`s class loader.
 * loadInstalled asks for the PLATFORM loader; load (TCCL) asks for the
 * application loader. If CratonVM assigns platform modules to the wrong loader,
 * load would find the providers and loadInstalled would not -- exactly the
 * observed split. This prints the loader each module and provider class is
 * assigned to.
 */
public class ModuleLoaders {

    static String ln(ClassLoader cl) {
        if (cl == null) return "null (boot)";
        String n = cl.getName();
        return (n == null ? cl.getClass().getName() : n) + "  [" + cl.getClass().getSimpleName() + "]";
    }

    static void cls(String cn) {
        try {
            Class<?> c = Class.forName(cn);
            System.out.println("  " + cn);
            System.out.println("      module = " + c.getModule().getName());
            System.out.println("      loader = " + ln(c.getClassLoader()));
            System.out.println("      module.getClassLoader = " + ln(c.getModule().getClassLoader()));
        } catch (Throwable t) {
            System.out.println("  " + cn + " -> " + t.getClass().getName());
        }
    }

    public static void main(String[] args) {
        System.out.println("platform = " + ln(ClassLoader.getPlatformClassLoader()));
        System.out.println("system   = " + ln(ClassLoader.getSystemClassLoader()));
        System.out.println("providers:");
        cls("sun.util.resources.cldr.provider.CLDRLocaleDataMetaInfo");
        cls("sun.util.resources.provider.NonBaseLocaleDataMetaInfo");
        cls("jdk.nio.zipfs.ZipFileSystemProvider");
        System.out.println("controls (must be java.base / boot on a healthy VM):");
        cls("java.lang.String");
        System.out.println("modules and their loaders:");
        for (Module m : ModuleLayer.boot().modules()) {
            String n = m.getName();
            if (n.equals("jdk.localedata") || n.equals("jdk.zipfs")
                    || n.equals("java.base") || n.equals("jdk.charsets")) {
                System.out.println("  " + n + "  loader = " + ln(m.getClassLoader()));
            }
        }
    }
}
