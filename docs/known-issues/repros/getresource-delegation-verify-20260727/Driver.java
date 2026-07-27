import java.net.URL;
import java.net.URLClassLoader;

public class Driver {
    public static void main(String[] args) throws Exception {
        URL jarUrl = new java.io.File(args[0]).toURI().toURL();
        URLClassLoader child = new URLClassLoader(new URL[] { jarUrl }, null);
        Class<?> holder = Class.forName("probe.lib.ResourceHolder", true, child);

        String content = (String) holder.getMethod("readOwnResource").invoke(null);
        String url = (String) holder.getMethod("resourceUrl").invoke(null);

        System.out.println("content=" + content);
        System.out.println("url=" + url);

        boolean ok = "CRATONVM_GETRESOURCE_DELEGATION_PROBE_OK".equals(content)
                && url != null && url.contains("probe-lib.jar");

        System.out.println(ok ? "RESULT=PASS" : "RESULT=FAIL");
        if (!ok) {
            System.exit(1);
        }
    }
}
