import java.io.File;
import java.io.FileInputStream;
import java.io.InputStream;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.KeyStore;
import java.security.Provider;
import java.security.Security;

/**
 * Can this VM read `${java.home}/conf/security/java.security`, and does the JCA
 * provider list come out of it?
 *
 * WHY. `HANDOFF-20260828-SCOPE` §4 lists `KeyStore.getInstance("JCEKS")` as an
 * OPEN, UNCLAIMED gap -- "a JCA format missing in BOTH modes" -- and
 * `KeyStoreFamilySweep` is 116/160 with that gap as the whole residual.
 * Meanwhile `native-builtins/src/lib.rs` carries a shim whose comment says the
 * real `Cipher.<clinit>` chain reads that file through `Security.<clinit>` and
 * fails with `IOException("Is a directory")`, so the chain is no-opped.
 *
 * On disk the path is a REGULAR FILE of 74132 bytes. So either the read still
 * fails and the reason is not what the comment says, or the read succeeds now
 * and the shim is a workaround that has outlived its defect -- and the JCEKS
 * gap may be nothing more than a provider list that was never populated.
 *
 * Every row prints on BOTH VMs so the answer is a diff, not an assertion.
 */
public final class JavaSecurityReach {
    static int n = 0;

    static void p(String label, Object v) {
        System.out.println(++n + " " + label + " |" + v + "|");
    }

    static Object attempt(String label, Callable c) {
        try {
            return c.call();
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    interface Callable { Object call() throws Exception; }

    public static void main(String[] args) {
        String home = System.getProperty("java.home");
        p("java.home", home);
        File f = new File(home, "conf/security/java.security");

        p("File.exists", attempt("", () -> f.exists()));
        p("File.isFile", attempt("", () -> f.isFile()));
        p("File.isDirectory", attempt("", () -> f.isDirectory()));
        p("File.length", attempt("", () -> f.length()));
        p("canRead", attempt("", () -> f.canRead()));

        p("FileInputStream first 16 bytes", attempt("", () -> {
            try (InputStream in = new FileInputStream(f)) {
                byte[] b = new byte[16];
                int r = in.read(b);
                return "read=" + r;
            }
        }));
        p("Files.readAllBytes length", attempt("", () -> Files.readAllBytes(Path.of(f.getPath())).length));
        p("Files.size", attempt("", () -> Files.size(Path.of(f.getPath()))));

        // The JCA surface the file is supposed to populate.
        p("Security.getProviders count", attempt("", () -> Security.getProviders().length));
        p("Security provider names", attempt("", () -> {
            StringBuilder sb = new StringBuilder();
            for (Provider pr : Security.getProviders()) {
                sb.append(pr.getName()).append(' ');
            }
            return sb.toString().trim();
        }));
        p("Security.getProperty security.provider.1",
          attempt("", () -> Security.getProperty("security.provider.1")));

        for (String type : new String[] {"JKS", "PKCS12", "JCEKS"}) {
            p("KeyStore.getInstance " + type, attempt("", () -> {
                KeyStore ks = KeyStore.getInstance(type);
                return ks.getClass().getName() + " provider=" + ks.getProvider().getName();
            }));
        }
    }
}
