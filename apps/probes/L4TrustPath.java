import java.io.*;
import java.security.KeyStore;

/** L4 -- the exact sequence RJdkSecurity's truststore section performs, asked
 *  one step at a time. The vector fails with `trustStorePropAnchors=122` (the
 *  default cacerts) instead of 1, which means the JDK never used the store the
 *  test wrote; this narrows WHICH step stopped being true. */
public class L4TrustPath {
    static int rows = 0;
    static void p(String k, Object v) { System.out.println(k + " |" + v + "|"); rows++; }
    public static void main(String[] a) throws Exception {
        File f = File.createTempFile("l4ts", ".jks");
        p("createTempFile.exists", f.exists());
        p("createTempFile.isFile", f.isFile());
        p("createTempFile.canRead", f.canRead());
        p("createTempFile.length0", f.length());
        p("abs.isAbsolute", f.getAbsolutePath().startsWith("/"));
        p("abs.endsWith.jks", f.getAbsolutePath().endsWith(".jks"));
        p("abs.equals.path", f.getAbsolutePath().equals(f.getPath()));
        File g = new File(f.getAbsolutePath());
        p("reopened.isFile", g.isFile());
        p("reopened.canRead", g.canRead());
        p("canonical.isFile", f.getCanonicalFile().isFile());
        p("isDirectory", f.isDirectory());

        KeyStore ks = KeyStore.getInstance(KeyStore.getDefaultType());
        ks.load(null, null);
        try (OutputStream o = new FileOutputStream(f)) { ks.store(o, "changeit".toCharArray()); }
        p("afterStore.length>0", f.length() > 0);
        p("afterStore.isFile", f.isFile());
        p("afterStore.reopened.isFile", new File(f.getAbsolutePath()).isFile());
        KeyStore back = KeyStore.getInstance(KeyStore.getDefaultType());
        try (InputStream in = new FileInputStream(f)) { back.load(in, "changeit".toCharArray()); }
        p("reloaded.size", back.size());
        p("delete", f.delete());
        System.out.println("rows " + rows);
        System.out.println("DONE L4TrustPath");
    }
}
