import java.io.File;
import java.util.ArrayList;
import java.util.Enumeration;
import java.util.List;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;

/**
 * SBR5 lane clload: COLD class loading. Class.forName(name, false, appLoader) over the first N class
 * names of every jar on java.class.path (a multi-jar class path, split per jar so the lookup walks
 * the jars the way a Jersey / Spring class path does). No initialization.
 *
 * usage: java -cp <jars...> Sbr5ClloadLoadProbe.java-compiled [N per jar, default 1500 total] [verify]
 * Prints the total ms, ms/class, and a digest of defined packages / code sources so a HotSpot diff
 * catches a lost CodeSource, ProtectionDomain or sealed package.
 */
public class Sbr5ClloadLoadProbe {
    public static void main(String[] a) throws Exception {
        int total = a.length > 0 ? Integer.parseInt(a[0]) : 1500;
        String cp = System.getProperty("sbr5.jars", System.getProperty("java.class.path"));
        List<String> names = new ArrayList<>();
        String[] jars = cp.split(File.pathSeparator);
        int per = Math.max(1, total / Math.max(1, jars.length));
        for (String j : jars) {
            if (!j.endsWith(".jar")) continue;
            int n = 0;
            try (JarFile jf = new JarFile(j)) {
                for (Enumeration<JarEntry> e = jf.entries(); e.hasMoreElements() && n < per; ) {
                    String en = e.nextElement().getName();
                    if (!en.endsWith(".class") || en.startsWith("META-INF/") || en.equals("module-info.class")
                            || en.endsWith("package-info.class")) continue;
                    names.add(en.substring(0, en.length() - 6).replace('/', '.'));
                    n++;
                }
            }
        }
        ClassLoader l = ClassLoader.getSystemClassLoader();
        long t0 = System.nanoTime();
        int ok = 0, fail = 0;
        List<Class<?>> loaded = new ArrayList<>();
        StringBuilder batches = new StringBuilder();
        long tb = t0;
        int idx = 0;
        for (String n : names) {
            try {
                loaded.add(Class.forName(n, false, l));
                ok++;
            } catch (Throwable t) {
                fail++;
            }
            if (++idx % 250 == 0) {
                long nb = System.nanoTime();
                batches.append((nb - tb) / 1_000_000).append(' ');
                tb = nb;
            }
        }
        System.err.println("BATCHES(ms per 250) " + batches);
        long t1 = System.nanoTime();
        System.out.println("classes=" + names.size() + " ok=" + ok + " fail=" + fail);
        // semantic digest: stable under HotSpot
        int withCs = 0, withPd = 0, withPkg = 0, sealed = 0, defLoader = 0;
        for (Class<?> c : loaded) {
            if (c.getProtectionDomain().getCodeSource() != null) withCs++;
            if (c.getProtectionDomain() != null) withPd++;
            Package p = c.getPackage();
            if (p != null) withPkg++;
            if (p != null && p.isSealed()) sealed++;
            if (c.getClassLoader() == l) defLoader++;
        }
        System.out.println("codeSource=" + withCs + " pd=" + withPd + " pkg=" + withPkg + " sealed=" + sealed
                + " appLoader=" + defLoader);
        System.err.println("LOADPROBE ms=" + (t1 - t0) / 1_000_000 + " per-class-us=" + (t1 - t0) / 1000 / Math.max(1, names.size()));
    }
}
