import org.osgi.framework.Constants;
import org.osgi.framework.launch.FrameworkFactory;
import org.osgi.framework.launch.Framework;
import java.util.HashMap;
import java.util.Map;
public class FelixProbe {
    public static void main(String[] args) throws Exception {
        FrameworkFactory factory = new org.apache.felix.framework.FrameworkFactory();
        Map<String, String> config = new HashMap<>();
        config.put(Constants.FRAMEWORK_STORAGE_CLEAN, Constants.FRAMEWORK_STORAGE_CLEAN_ONFIRSTINIT);
        Framework framework = factory.newFramework(config);
        framework.init();
        System.out.println("Felix framework state: " + framework.getState());
        framework.stop();
        framework.waitForStop(5000);
        System.out.println("Felix stopped: " + framework.getState());
        System.out.println("OK");
    }
}
