import java.io.InputStream;
import java.lang.module.ModuleDescriptor;
import java.net.URL;
import java.util.*;

/**
 * Interpreter round i4 wave 99, lane L3: getResource / getResources /
 * getResourceAsStream("module-info.class") through the built-in loaders
 * (page interpreter-i98-L3-builtin-loader-resource-rows-differ-from-hotspot-FIXED-20261015.md,
 * row 4).
 *
 * Run from a directory with no module-info.class on the class path:
 *   java -cp . L3W99ModuleInfoResources
 *
 * HotSpot lists one jrt:/<m>/module-info.class per boot-layer module: the boot
 * loader's, then the platform loader's, then the application loader's
 * (BuiltinClassLoader.findMiscResource over each loader's nameToModule, a
 * ConcurrentHashMap<>(32) with 64 bins). Inside a loader the order is
 * ascending bin, (h ^ h>>>16) & 63 of String.hashCode, and inside a shared
 * bin it is insertion order: ModuleBootstrap's loadModules over a Set.of,
 * which depends on ImmutableCollections.SALT32L and identity hashes. With the
 * default CDS archive that order is fixed per JDK build (the loaders are
 * restored from dump time), but it is not a function of the module names and
 * under -Xshare:off it changes per run. So the lists are printed CANONICAL:
 * each run of consecutive entries in one bin sorted by name. The raw first
 * answer varies between HotSpot runs, so only its membership is printed.
 *
 * Expected HotSpot 25 (Temurin 25.0.3) output, default CDS:
 *   app getResource listed: true
 *   app getResource in first bin [java.rmi, jdk.management.jfr]: true
 *   app getResources count: 62
 *   app getResources all jrt module-info: true
 *   app getResources bin descents: 2
 *   app getResources canonical: [java.rmi, jdk.management.jfr, java.logging, java.xml, jdk.jfr,
 *     java.datatransfer, jdk.net, java.desktop, java.naming, java.prefs, java.security.sasl,
 *     jdk.naming.rmi, java.base, jdk.management.agent, java.management, jdk.sctp, jdk.unsupported,
 *     java.instrument, jdk.management, jdk.nio.mapmode, java.management.rmi, java.sql, jdk.charsets,
 *     java.transaction.xa, java.xml.crypto, jdk.xml.dom, jdk.httpserver, jdk.crypto.cryptoki,
 *     java.net.http, jdk.zipfs, jdk.crypto.mscapi, java.sql.rowset, jdk.jsobject, java.smartcardio,
 *     java.security.jgss, java.compiler, jdk.security.auth, java.scripting, jdk.dynalink,
 *     jdk.accessibility, jdk.naming.dns, jdk.security.jgss, jdk.localedata, jdk.jdi, jdk.internal.md,
 *     jdk.jstatd, jdk.internal.ed, jdk.compiler, jdk.internal.opt, jdk.jconsole, jdk.attach,
 *     jdk.javadoc, jdk.jshell, jdk.editpad, jdk.internal.le, jdk.jlink, jdk.jdwp.agent,
 *     jdk.internal.jvmstat, jdk.jdeps, jdk.unsupported.desktop, jdk.jartool, jdk.jpackage]
 *     (one line)
 *   app getResources first == getResource: true
 *   app stream: descriptor of getResource's module: true
 *   platform: the same rows, count 43, bin descents 1, canonical = the first 43 names above
 *   context: the app rows
 *   system: getSystemResource / getSystemResources, the app rows
 *   app-child (a URLClassLoader with the application loader as parent): the app rows
 */
public class L3W99ModuleInfoResources {
    static final String MI = "module-info.class";

    static int bin(String name) {
        int h = name.hashCode();
        return ((h ^ (h >>> 16)) & 0x7fffffff) & 63;
    }

    static String moduleOf(URL u) {
        if (u == null) return null;
        String s = u.toString();
        String suffix = "/" + MI;
        if (!s.startsWith("jrt:/") || !s.endsWith(suffix)) return null;
        return s.substring("jrt:/".length(), s.length() - suffix.length());
    }

    static List<String> canonical(List<String> modules) {
        List<String> out = new ArrayList<>();
        int i = 0;
        while (i < modules.size()) {
            int j = i;
            while (j < modules.size() && bin(modules.get(j)) == bin(modules.get(i))) j++;
            List<String> run = new ArrayList<>(modules.subList(i, j));
            Collections.sort(run);
            out.addAll(run);
            i = j;
        }
        return out;
    }

    static List<String> firstBin(List<String> modules) {
        List<String> run = new ArrayList<>();
        for (String m : modules) {
            if (!run.isEmpty() && bin(m) != bin(run.get(0))) break;
            run.add(m);
        }
        Collections.sort(run);
        return run;
    }

    static void rows(String label, URL one, Enumeration<URL> all, InputStream in) {
        List<URL> urls = Collections.list(all);
        List<String> modules = new ArrayList<>();
        boolean allJrt = true;
        for (URL u : urls) {
            String m = moduleOf(u);
            if (m == null) allJrt = false; else modules.add(m);
        }
        String oneModule = moduleOf(one);
        List<String> first = firstBin(modules);
        // The raw first URL varies between HotSpot runs (in-bin insertion
        // order); print only whether it is one of the listed URLs.
        boolean listed = false;
        for (URL u : urls) if (one != null && u.toString().equals(one.toString())) listed = true;
        System.out.println(label + " getResource listed: " + listed);
        System.out.println(label + " getResource in first bin " + first + ": " + (oneModule != null && first.contains(oneModule)));
        System.out.println(label + " getResources count: " + urls.size());
        System.out.println(label + " getResources all jrt module-info: " + allJrt);
        int descents = 0;
        for (int i = 1; i < modules.size(); i++) if (bin(modules.get(i)) < bin(modules.get(i - 1))) descents++;
        System.out.println(label + " getResources bin descents: " + descents);
        System.out.println(label + " getResources canonical: " + canonical(modules));
        System.out.println(label + " getResources first == getResource: "
                + (!urls.isEmpty() && one != null && urls.get(0).toString().equals(one.toString())));
        String streamRow;
        if (in == null) {
            streamRow = "null";
        } else {
            try (InputStream s = in) {
                streamRow = String.valueOf(ModuleDescriptor.read(s).name().equals(oneModule));
            } catch (Exception e) {
                streamRow = e.getClass().getName();
            }
        }
        System.out.println(label + " stream: descriptor of getResource's module: " + streamRow);
    }

    public static void main(String[] args) throws Exception {
        ClassLoader app = ClassLoader.getSystemClassLoader();
        ClassLoader plat = ClassLoader.getPlatformClassLoader();
        ClassLoader ctx = Thread.currentThread().getContextClassLoader();
        rows("app", app.getResource(MI), app.getResources(MI), app.getResourceAsStream(MI));
        rows("platform", plat.getResource(MI), plat.getResources(MI), plat.getResourceAsStream(MI));
        rows("context", ctx.getResource(MI), ctx.getResources(MI), ctx.getResourceAsStream(MI));
        rows("system", ClassLoader.getSystemResource(MI), ClassLoader.getSystemResources(MI),
                ClassLoader.getSystemResourceAsStream(MI));
        try (java.net.URLClassLoader child = new java.net.URLClassLoader(new URL[0], app)) {
            rows("app-child", child.getResource(MI), child.getResources(MI), child.getResourceAsStream(MI));
        }
    }
}
