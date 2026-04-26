import java.io.File;
import org.jboss.modules.LocalModuleLoader;
import org.jboss.modules.Module;
import org.jboss.modules.ModuleLoader;
import org.jboss.modules.ModuleLoadException;

public class ModXmlProbe {
    public static void main(String[] args) throws Exception {
        // WildFly-style layered modules: pass the per-layer roots to LocalModuleLoader.
        File baseLayer = new File("C:/craton/keycloak-16.1.1/modules/system/layers/base");
        File kcLayer   = new File("C:/craton/keycloak-16.1.1/modules/system/layers/keycloak");
        ModuleLoader loader = new LocalModuleLoader(new File[]{ baseLayer, kcLayer });
        Module m = loader.loadModule("asm.asm");
        System.out.println("loaded module " + m.getName());
        // try a few harder ones (multi-deps + properties)
        String[] names = new String[]{
            "javax.api",
            "org.jboss.logging",
            "org.wildfly.security.elytron-private",
            "org.jboss.logmanager",
            "io.undertow.core",
            "org.apache.commons.codec"
        };
        for (String name : names) {
            try {
                Module mm = loader.loadModule(name);
                System.out.println("loaded " + name + " ok name=" + mm.getName());
            } catch (ModuleLoadException e) {
                System.out.println("FAIL " + name + " : " + e);
            } catch (Throwable t) {
                System.out.println("FAILX " + name + " : " + t);
            }
        }
        System.out.println("DONE");
    }
}
