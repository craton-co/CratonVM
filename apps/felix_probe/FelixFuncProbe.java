import org.osgi.framework.Constants;
import org.osgi.framework.launch.FrameworkFactory;
import org.osgi.framework.launch.Framework;
import org.osgi.framework.Bundle;
import org.osgi.framework.BundleContext;
import java.util.HashMap;
import java.util.Map;

public class FelixFuncProbe {
    public static void main(String[] args) throws Exception {
        FrameworkFactory factory = new org.apache.felix.framework.FrameworkFactory();
        Map<String, String> config = new HashMap<>();
        config.put(Constants.FRAMEWORK_STORAGE_CLEAN, Constants.FRAMEWORK_STORAGE_CLEAN_ONFIRSTINIT);
        Framework framework = factory.newFramework(config);
        framework.init();
        System.out.println("After init: state=" + framework.getState());
        System.out.flush();
        framework.start();
        System.out.println("After start: state=" + framework.getState());
        System.out.flush();
        BundleContext ctx = framework.getBundleContext();
        Bundle systemBundle = ctx.getBundle(0);
        System.out.println("System bundle: " + systemBundle.getSymbolicName());
        System.out.flush();
        Bundle[] bundles = ctx.getBundles();
        System.out.println("Installed bundle count: " + bundles.length);
        System.out.flush();
        if (framework.getState() != Bundle.ACTIVE) {
            System.out.println("FAIL: framework state " + framework.getState() + " != ACTIVE");
            System.exit(1);
        }
        framework.stop();
        framework.waitForStop(5000);
        System.out.println("Framework stopped, final state=" + framework.getState());
        System.out.println("OK");
        System.out.flush();
        System.exit(0);
    }
}
