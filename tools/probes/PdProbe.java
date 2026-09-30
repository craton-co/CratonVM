// getClassLoader() / getProtectionDomain().getCodeSource() for boot, platform, app and runtime-image classes; compare with HotSpot.
//
// Record: fs-cluster-needtoresolveagainstdefaultdirectory-FIXED-20260923.md
// Run: cratonvm --java-home <jdk-25> -c <dir>[:<classpath>] PdProbe
// Compare with the same command on HotSpot (java -cp ...).
import java.security.*;
public class PdProbe {
    public static void main(String[] a) throws Exception {
        String[] names = {"java.lang.String", "java.lang.Object", "jdk.internal.jrtfs.SystemImage", "jdk.internal.jrtfs.JrtFileSystemProvider",
            "java.sql.Connection", "com.sun.tools.javac.api.JavacTool", "PdProbe"};
        for (String n : names) {
            Class<?> c = Class.forName(n, false, ClassLoader.getSystemClassLoader());
            ProtectionDomain pd = c.getProtectionDomain();
            CodeSource cs = pd == null ? null : pd.getCodeSource();
            System.out.println(n + " loader=" + c.getClassLoader() + " pdNull=" + (pd == null) + " cs=" + (cs == null ? "null" : String.valueOf(cs.getLocation())));
        }
    }
}
