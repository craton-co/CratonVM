import java.util.jar.JarFile;
import java.util.jar.JarEntry;
import java.util.zip.ZipFile;
import java.util.zip.ZipEntry;
import java.io.InputStream;

public class JarEntryProbe {
    public static void main(String[] args) throws Exception {
        String jar = "/data/tmp/kc-dist/keycloak-26.6.1/lib/quarkus/generated-bytecode.jar";
        String[] names = {
            "io/quarkus/value/registry/ValueRegistry_Vjm2hphPTUShgv9MdpQ7IxocVcI_Synthetic_Bean.class",
            "io/quarkus/value/registry/ValueRegistry_Observer_Synthetic_i8SkhdhAV_w_PxAJmk32fKwyL0I.class",
            "io/quarkus/runner/recorded/ArcProcessor$initializeContainer643029769.class",
        };
        try (ZipFile z = new ZipFile(jar)) {
            System.out.println("ZipFile size=" + z.size());
            for (String n : names) {
                ZipEntry e = z.getEntry(n);
                int len = -1;
                if (e != null) {
                    try (InputStream in = z.getInputStream(e)) {
                        len = in.readAllBytes().length;
                    }
                }
                System.out.println("  zip entry=" + (e != null) + " bytes=" + len + "  " + n);
            }
        }
        try (JarFile j = new JarFile(jar)) {
            for (String n : names) {
                JarEntry e = j.getJarEntry(n);
                int len = -1;
                if (e != null) {
                    try (InputStream in = j.getInputStream(e)) {
                        len = in.readAllBytes().length;
                    }
                }
                System.out.println("  jar entry=" + (e != null) + " bytes=" + len + "  " + n);
            }
        }
    }
}
