import java.io.*;
import java.net.*;
import java.nio.file.*;
import java.nio.file.attribute.*;
import java.util.*;

/** Bootstrap dependencies exposed by SSLSessionImpl initialization and explicit engine SNI. */
public class SslCarrierResiduals {
    interface Call { Object run() throws Exception; }
    static String result(Call c) {
        try { return String.valueOf(c.run()); }
        catch (Exception e) { return e.getClass().getName() + ":" + e.getMessage(); }
    }
    public static void main(String[] a) throws Exception {
        System.out.println("IDN=" + IDN.toASCII("localhost") + "/" + IDN.toASCII("b\u00fccher.example"));
        System.out.println("encapsulated=" + (Class.forName("jdk.internal.icu.impl.ICUBinary")
                .getResourceAsStream("data/icudt76b/uprops.icu") == null));
        Class.forName("javax.crypto.JceSecurity");
        Class.forName("sun.security.ssl.SSLSessionImpl");
        System.out.println("JCE_SESSION_INIT_OK");
        Path dir = Path.of(System.getProperty("java.home"), "conf/security/policy/unlimited");
        for (String glob : new String[]{"*", "default_*.policy", "{default,exempt}_*.policy"}) {
            try (DirectoryStream<Path> stream = Files.newDirectoryStream(dir, glob)) {
                List<String> names = new ArrayList<>();
                for (Path entry : stream) names.add(entry.getFileName().toString());
                Collections.sort(names);
                System.out.println("policy " + glob + "=" + names);
                System.out.println("second-iterator=" + result(() -> stream.iterator()));
                if (stream instanceof SecureDirectoryStream<Path> secure) {
                    BasicFileAttributeView view = secure.getFileAttributeView(Path.of("default_local.policy"), BasicFileAttributeView.class);
                    System.out.println("secure-attributes=" + view.readAttributes().isRegularFile());
                }
                stream.close();
                System.out.println("closed-iterator=" + result(() -> stream.iterator()));
            }
        }
        System.out.println("done");
    }
}
