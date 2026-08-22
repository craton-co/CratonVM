import java.util.ArrayList;
import java.util.List;

/**
 * `ClassLoader.getDefinedPackage` and `ClassLoader.definePackage` are two
 * halves of ONE piece of state — the loader's own package map. Groovy's
 * `GroovyClassLoader.definePackageInternal` is the canonical reader of that
 * contract:
 *
 * <pre>
 *   if (getDefinedPackage(pkgName) == null) definePackage(pkgName, ...);
 * </pre>
 *
 * If the query half cannot see what the definition half wrote, the SECOND
 * class defined in any one package throws
 * `IllegalArgumentException: &lt;pkg&gt;` out of `definePackage`. That is the whole
 * Spring Groovy cluster, and this probe reproduces it with no Groovy on the
 * classpath at all.
 *
 * Every row is a fact about the loader's own bookkeeping, identical on any
 * correct JVM.
 */
public class DefinedPackageProbe {

    static void p(String k, Object v) {
        System.out.println(k + " = " + v);
    }

    /** A loader that defines classes from BYTES, the way Groovy defines them from source. */
    static final class BytesLoader extends ClassLoader {
        BytesLoader() { super(DefinedPackageProbe.class.getClassLoader()); }

        Package definePkg(String name) {
            return definePackage(name, null, null, null, null, null, null, null);
        }

        /** Verbatim shape of GroovyClassLoader.definePackageInternal. */
        String groovyDefine(String className) {
            int i = className.lastIndexOf('.');
            if (i == -1) {
                return "no-package";
            }
            String pkgName = className.substring(0, i);
            Package pkg = getDefinedPackage(pkgName);
            if (pkg == null) {
                try {
                    definePkg(pkgName);
                    return "defined";
                }
                catch (IllegalArgumentException ex) {
                    return "THREW IAE: " + ex.getMessage();
                }
            }
            return "already-defined";
        }
    }

    public static void main(String[] args) {
        BytesLoader cl = new BytesLoader();
        String pkg = "probe.pkg.alpha";

        // ---- P: the query half must see what the definition half wrote ----
        p("P01 getDefinedPackage before define", cl.getDefinedPackage(pkg));
        Package first = cl.definePkg(pkg);
        p("P02 definePackage returned non-null", first != null);
        p("P03 returned package name", first == null ? null : first.getName());
        Package seen = cl.getDefinedPackage(pkg);
        p("P04 getDefinedPackage after define is non-null", seen != null);
        p("P05 name matches", seen == null ? null : seen.getName());
        p("P06 identity is stable across calls", cl.getDefinedPackage(pkg) == seen);
        p("P07 same object definePackage returned", seen == first);

        // ---- D: defining twice is an IllegalArgumentException -------------
        String second;
        try {
            cl.definePkg(pkg);
            second = "no-throw";
        }
        catch (IllegalArgumentException ex) {
            second = "IAE:" + ex.getMessage();
        }
        p("D01 second definePackage", second);

        // ---- G: Groovy's exact guard, twice, on the same package ----------
        // This is the row the whole cluster turns on: the second call must
        // report already-defined, never an IAE.
        BytesLoader g = new BytesLoader();
        p("G01 first class in package", g.groovyDefine("probe.pkg.beta.One"));
        p("G02 second class in SAME package", g.groovyDefine("probe.pkg.beta.Two"));
        p("G03 third class in SAME package", g.groovyDefine("probe.pkg.beta.Three"));
        p("G04 class in a different package", g.groovyDefine("probe.pkg.gamma.One"));
        p("G05 default package", g.groovyDefine("NoPackage"));

        // ---- N: a package NOT defined stays null --------------------------
        // The opposite error — answering non-null for everything — would make
        // Groovy skip definePackage entirely and is equally wrong.
        p("N01 never-defined package", g.getDefinedPackage("probe.pkg.never"));
        p("N02 empty name", g.getDefinedPackage(""));

        // ---- L: loaders do not see each other's definitions ---------------
        // getDefinedPackage is non-delegating.
        BytesLoader a = new BytesLoader();
        BytesLoader b = new BytesLoader();
        a.definePkg("probe.pkg.iso");
        p("L01 definer sees it", a.getDefinedPackage("probe.pkg.iso") != null);
        p("L02 sibling does not", b.getDefinedPackage("probe.pkg.iso"));
        p("L03 sibling can define it itself", b.definePkg("probe.pkg.iso") != null);

        // ---- S: getDefinedPackages must agree with getDefinedPackage ------
        List<String> names = new ArrayList<>();
        for (Package q : a.getDefinedPackages()) {
            names.add(q.getName());
        }
        p("S01 getDefinedPackages contains the defined one", names.contains("probe.pkg.iso"));
        p("S02 a package it reports is also queryable",
                names.isEmpty() || a.getDefinedPackage(names.get(0)) != null);

        // ---- C: a package backed by REAL class files on the classpath -----
        // The classpath-probe path must keep working; this is the Spring Boot
        // `BeanDefinitionLoader.findPackage` contract.
        ClassLoader app = DefinedPackageProbe.class.getClassLoader();
        p("C01 app loader sees java.lang via bootstrap", app.getDefinedPackage("java.lang"));
        p("C02 app loader sees its own package",
                app.getDefinedPackage("") == null ? "null-for-empty" : "non-null-for-empty");
    }
}
