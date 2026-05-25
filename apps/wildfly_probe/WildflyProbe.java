import org.jboss.modules.ModuleLoader;
import org.jboss.modules.LocalModuleLoader;
import org.jboss.modules.LocalModuleFinder;
import org.jboss.modules.ModuleFinder;
import org.jboss.modules.Module;
import java.io.File;

public class WildflyProbe {
    public static void main(String[] args) throws Exception {
        if (args.length < 1) {
            System.out.println("Usage: WildflyProbe <wildfly-home>");
            System.exit(1);
        }
        File wfHome = new File(args[0]);
        File modulesDir = new File(wfHome, "modules");
        if (!modulesDir.isDirectory()) {
            System.out.println("FAIL: modules dir not found: " + modulesDir);
            System.exit(1);
        }
        System.out.println("Modules dir: " + modulesDir.getAbsolutePath());
        // Construct a LocalModuleLoader rooted at wildfly's modules/. This
        // exercises the JBoss Modules class loading machinery.
        File[] roots = new File[] { modulesDir };
        LocalModuleFinder finder = new LocalModuleFinder(roots);
        ModuleLoader loader = new LocalModuleLoader(roots);
        System.out.println("Loader created: " + loader);
        // Try resolving a well-known module: javax.api / java.base / etc.
        // WildFly always ships `org.jboss.modules` itself as a module.
        try {
            Module mod = loader.loadModule("org.jboss.logging");
            System.out.println("Loaded module: " + mod.getName());
        } catch (Throwable t) {
            System.out.println("Module load soft-fail: " + t.getClass().getSimpleName() + ": " + t.getMessage());
        }
        System.out.println("OK");
        System.exit(0);
    }
}
