import java.util.ResourceBundle;

/** Loaded by the child loader in CallerBundleProbe; never by the app loader. */
public class BundleUser {
    public static String load() {
        ResourceBundle b = ResourceBundle.getBundle("probebundle");
        return "OK k=" + b.getString("k")
                + " bundleLoaderVisible=" + (BundleUser.class.getClassLoader() != null);
    }
}
