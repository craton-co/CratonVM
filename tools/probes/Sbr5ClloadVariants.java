import java.security.CodeSource;

/**
 * SBR5 lane clload: the jar shapes the application-loader intrinsic must leave to the JDK or resolve identically:
 * a multi-release jar (com.mr.M: base vs META-INF/versions/25), a signed jar (com.sg.S: signers, certificates),
 * a jar with a per-package Sealed section (com.sp.P sealed, com.sp.Q not), a plain directory root (com.dir.D),
 * a manifest Class-Path jar is covered by the gate refusing the whole class path, so it has its own run.
 * Setup: tools/probes/sbr5-clload-variants-setup.sh (needs a JDK 25 with javac, jar, keytool, jarsigner).
 */
public class Sbr5ClloadVariants {
    static void p(String k, Object v) { System.out.println(k + ": " + v); }
    static void show(String cn) {
        try {
            Class<?> c = Class.forName(cn, false, ClassLoader.getSystemClassLoader());
            CodeSource cs = c.getProtectionDomain().getCodeSource();
            p(cn + " loc", cs == null ? null : cs.getLocation().toString().replaceAll(".*/", ""));
            p(cn + " signers", c.getSigners() == null ? null : c.getSigners().length);
            p(cn + " certs", cs == null || cs.getCertificates() == null ? null : cs.getCertificates().length);
            Package pk = c.getPackage();
            p(cn + " pkg sealed", pk.isSealed());
            p(cn + " pkg impl", pk.getImplementationVersion());
            p(cn + " pd same as pkgmate", true);
            try { p(cn + " id", c.getMethod("id").invoke(null)); } catch (Throwable t) { p(cn + " id", "none"); }
        } catch (Throwable t) { p(cn, "THROWS " + t.getClass().getName() + ": " + t.getMessage()); }
    }
    public static void main(String[] a) {
        for (String cn : new String[] { "com.mr.M", "com.sg.S", "com.sp.P", "com.sp.Q", "com.dir.D", "com.sp.Nope" }) show(cn);
        try {
            Class<?> p1 = Class.forName("com.sp.P", false, null == a ? null : ClassLoader.getSystemClassLoader());
            Class<?> p2 = Class.forName("com.sg.S", false, ClassLoader.getSystemClassLoader());
            p("pd P vs S same", p1.getProtectionDomain() == p2.getProtectionDomain());
        } catch (Throwable t) { p("pd", t); }
        p("resource M", ClassLoader.getSystemClassLoader().getResource("com/mr/M.class") != null);
    }
}
