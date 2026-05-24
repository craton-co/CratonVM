import java.util.jar.*;
import java.io.File;

public class JarManifestProbe {
    public static void main(String[] a) throws Exception {
        String f = a.length > 0 ? a[0] : "apps/wlp/bin/tools/ws-schemagen.jar";
        System.out.println("path=" + f);
        File file = new File(f);
        System.out.println("file.exists=" + file.exists() + " size=" + file.length());
        JarFile jf = new JarFile(file);
        Manifest m = jf.getManifest();
        System.out.println("manifest=" + m);
        if (m != null) {
            Attributes mainA = m.getMainAttributes();
            System.out.println("mainAttributes=" + mainA);
            if (mainA != null) {
                System.out.println("Main-Class=" + mainA.getValue("Main-Class"));
                System.out.println("Command-Class=" + mainA.getValue("Command-Class"));
            }
        }
        jf.close();

        System.out.println("---");
        System.out.println("java.class.path=" + System.getProperty("java.class.path"));
    }
}
