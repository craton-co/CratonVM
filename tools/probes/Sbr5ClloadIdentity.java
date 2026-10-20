import java.net.URL;
import java.security.CodeSource;
import java.security.ProtectionDomain;

/**
 * SBR5 lane clload: the observable identities the JDK's defineClass(String, Resource) gives a class-path class,
 * to diff HotSpot / retired loadClass / native loadClass. Needs pkg.jar (Sealed: true, com.sbr2w9pkg.A/B, see
 * Sbr3RefmiscSealed) and any other jar on the class path.
 */
public class Sbr5ClloadIdentity {
    static void p(String k, Object v) { System.out.println(k + ": " + v); }
    public static void main(String[] a) throws Exception {
        ClassLoader l = ClassLoader.getSystemClassLoader();
        Class<?> ca = Class.forName("com.sbr2w9pkg.A", false, l);
        Class<?> cb = l.loadClass("com.sbr2w9pkg.B");
        ProtectionDomain pa = ca.getProtectionDomain(), pb = cb.getProtectionDomain();
        p("pd same", pa == pb);
        CodeSource cs = pa.getCodeSource();
        p("cs loc", cs == null ? null : cs.getLocation().toString().replaceAll(".*/", ""));
        p("cs same", pa.getCodeSource() == pb.getCodeSource());
        p("cs equals", pa.getCodeSource().equals(pb.getCodeSource()));
        p("pd loader", pa.getClassLoader() == l);
        p("pd perms", pa.getPermissions() == null ? null : pa.getPermissions().getClass().getName());
        Package pk = ca.getPackage();
        p("pkg", pk);
        p("pkg sealed", pk.isSealed());
        p("pkg same", pk == cb.getPackage());
        p("pkg == defined", pk == l.getDefinedPackage("com.sbr2w9pkg"));
        p("pkg impl", pk.getImplementationTitle() + "/" + pk.getImplementationVersion() + "/" + pk.getImplementationVendor()
                + "/" + pk.getSpecificationTitle() + "/" + pk.getSpecificationVersion() + "/" + pk.getSpecificationVendor());
        p("loader", ca.getClassLoader() == l);
        p("module", ca.getModule().isNamed());
        URL r = l.getResource("com/sbr2w9pkg/A.class");
        p("resource", r == null ? null : r.toString().replaceAll(".*/", ""));
        p("signers", ca.getSigners());
        p("loadClass twice same", l.loadClass("com.sbr2w9pkg.A") == ca);
        try { Class.forName("com.sbr2w9pkg.Nope", false, l); } catch (ClassNotFoundException e) { p("cnfe", e.getMessage()); }
        // a class of another jar: package of the app loader
        Class<?> s = Class.forName("org.slf4j.Logger", false, l);
        p("other pkg sealed", s.getPackage().isSealed());
        p("other pd loc", s.getProtectionDomain().getCodeSource().getLocation().toString().replaceAll(".*/", ""));
        p("other pkg impl", s.getPackage().getImplementationTitle());
    }
}
