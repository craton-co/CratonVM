import java.lang.reflect.Field;
import java.security.CodeSource;
import java.security.PermissionCollection;
import java.security.Principal;
import java.security.ProtectionDomain;
import java.net.URL;

/**
 * A layout probe for {@code java.security.ProtectionDomain}, diffed against the
 * host JDK.
 *
 * CratonVM's fabricated model was in the JDK CONSTRUCTOR's argument order —
 * {@code (CodeSource, PermissionCollection, ClassLoader, Principal[])} — while
 * the class DECLARES {@code codesource, classloader, principals, permissions}.
 * Three of the four sat at the wrong index. All four are references, so a
 * value-tag overlay check could never see it.
 *
 * The four accessors are read TOGETHER after every construction, because a
 * rotation moves them as a group: checking {@code getClassLoader()} alone would
 * pass on a model that had merely swapped the other two.
 *
 * Prints VALUES, never "ok". Run under HotSpot first; its output is expected.
 */
public class ProtectionDomainLayoutProbe {
    static int sections = 0, failed = 0;

    static void section(String name, Runnable body) {
        sections++;
        try {
            body.run();
        } catch (Throwable t) {
            failed++;
            System.out.println("SECTION-FAILED " + name + ": " + t);
        }
    }

    public static void main(String[] args) throws Exception {
        section("reflected", ProtectionDomainLayoutProbe::reflected);
        section("constructed", ProtectionDomainLayoutProbe::constructed);
        section("staticPerms", ProtectionDomainLayoutProbe::staticPerms);
        section("fields", ProtectionDomainLayoutProbe::fields);
        section("errors", ProtectionDomainLayoutProbe::errors);
        System.out.println("PDLAYOUT sections=" + sections + " failed=" + failed);
    }

    /** All four accessors of one domain, as one line. */
    static void describe(String tag, ProtectionDomain pd) {
        if (pd == null) {
            System.out.println(tag + " pd=null");
            return;
        }
        CodeSource cs = pd.getCodeSource();
        ClassLoader cl = pd.getClassLoader();
        Principal[] ps = pd.getPrincipals();
        PermissionCollection pc = pd.getPermissions();
        System.out.println(tag
                + " csNull=" + (cs == null)
                + " csIsCodeSource=" + (cs instanceof CodeSource)
                + " clNull=" + (cl == null)
                + " clIsLoader=" + (cl instanceof ClassLoader)
                + " psNull=" + (ps == null)
                + " psIsArray=" + (ps != null && ps.getClass().isArray())
                + " psLen=" + (ps == null ? -1 : ps.length)
                + " pcNull=" + (pc == null)
                + " pcIsPermColl=" + (pc instanceof PermissionCollection));
    }

    /** The domain the VM builds for a loaded class — the site this fix touches. */
    static void reflected() {
        describe("reflected app", ProtectionDomainLayoutProbe.class.getProtectionDomain());
        describe("reflected jdk", String.class.getProtectionDomain());
        ProtectionDomain pd = ProtectionDomainLayoutProbe.class.getProtectionDomain();
        CodeSource cs = pd.getCodeSource();
        URL loc = (cs == null) ? null : cs.getLocation();
        System.out.println("reflected locationIsUrl=" + (loc instanceof URL)
                + " locationNull=" + (loc == null));
        // The identity that a rotation breaks: the loader on the domain must be
        // the loader that actually defined the class.
        System.out.println("reflected loaderMatchesDefining="
                + (pd.getClassLoader() == ProtectionDomainLayoutProbe.class.getClassLoader()));
    }

    /** A domain built through the public 4-arg constructor. */
    static void constructed() {
        ClassLoader cl = ProtectionDomainLayoutProbe.class.getClassLoader();
        CodeSource cs = new CodeSource(null, (java.security.cert.Certificate[]) null);
        Principal[] ps = new Principal[0];
        ProtectionDomain pd = new ProtectionDomain(cs, null, cl, ps);
        describe("constructed", pd);
        System.out.println("constructed csIdentity=" + (pd.getCodeSource() == cs)
                + " clIdentity=" + (pd.getClassLoader() == cl)
                + " psContentEqual=" + java.util.Arrays.equals(pd.getPrincipals(), ps));
        ProtectionDomain two = new ProtectionDomain(cs, null);
        describe("constructed 2arg", two);
        System.out.println("constructed 2argClNull=" + (two.getClassLoader() == null));
    }

    /**
     * The static-permissions flag lives past the four modelled fields, and
     * `implies` is the only behaviour that reads it. A domain built with the
     * 2-arg ctor is static; the 4-arg one is not.
     */
    static void staticPerms() {
        CodeSource cs = new CodeSource(null, (java.security.cert.Certificate[]) null);
        java.security.Permissions p = new java.security.Permissions();
        p.add(new java.util.PropertyPermission("java.version", "read"));
        ProtectionDomain stat = new ProtectionDomain(cs, p);
        ProtectionDomain dyn = new ProtectionDomain(cs, p, null, null);
        java.security.Permission probe = new java.util.PropertyPermission("java.version", "read");
        System.out.println("staticPerms staticImplies=" + stat.implies(probe)
                + " dynImplies=" + dyn.implies(probe));
        System.out.println("staticPerms toStringHasCodeSource="
                + String.valueOf(stat).contains("CodeSource"));
    }

    /** Natives and reflection must agree about the same object. */
    static void fields() {
        ProtectionDomain pd = ProtectionDomainLayoutProbe.class.getProtectionDomain();
        for (String fn : new String[] {"codesource", "classloader", "principals",
                                       "permissions", "hasAllPerm", "staticPermissions"}) {
            try {
                Field f = ProtectionDomain.class.getDeclaredField(fn);
                f.setAccessible(true);
                Object v = f.get(pd);
                String shown;
                if (v == null) {
                    shown = "null";
                } else if (v instanceof CodeSource) {
                    shown = "CodeSource";
                } else if (v instanceof ClassLoader) {
                    shown = "ClassLoader";
                } else if (v instanceof PermissionCollection) {
                    shown = "PermissionCollection";
                } else if (v.getClass().isArray()) {
                    shown = "array[" + java.lang.reflect.Array.getLength(v) + "]";
                } else {
                    shown = v.getClass().getName() + ":" + v;
                }
                System.out.println("field " + fn + "=" + shown
                        + " type=" + f.getType().getName());
            } catch (NoSuchFieldException e) {
                System.out.println("field " + fn + "=NO_SUCH_FIELD");
            } catch (Throwable t) {
                System.out.println("field " + fn + "=THREW " + t.getClass().getName());
            }
        }
    }

    static void errors() {
        try {
            ProtectionDomain pd = new ProtectionDomain(null, null, null, null);
            describe("errors allNull", pd);
        } catch (Throwable t) {
            System.out.println("errors allNull=THREW " + t.getClass().getName());
        }
    }
}
