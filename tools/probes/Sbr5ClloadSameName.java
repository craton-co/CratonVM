import java.io.File;
import java.io.InputStream;
import java.util.ArrayList;
import java.util.Enumeration;
import java.util.List;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;

/**
 * SBR5 lane clload: does the cost of an application-loader lookup grow with the number of
 * same-named classes OTHER loaders already defined? Set A = even-numbered jars' classes, set B =
 * odd-numbered jars'. Bare loaders (parent = null-ish bootstrap parent, so supertypes resolve
 * through a delegating loader that does NOT define A) define set A `rounds` times; then the
 * application loader loads A (fresh for it) and B (never defined elsewhere) and both are timed.
 */
public class Sbr5ClloadSameName {
    static class Bare extends ClassLoader {
        Bare(ClassLoader p) { super(p); }
        Class<?> def(String n, byte[] b) { return defineClass(n, b, 0, b.length); }
    }
    public static void main(String[] a) throws Exception {
        int total = a.length > 0 ? Integer.parseInt(a[0]) : 1500;
        int rounds = a.length > 1 ? Integer.parseInt(a[1]) : 3;
        String[] jars = System.getProperty("java.class.path").split(File.pathSeparator);
        int per = Math.max(1, total / Math.max(1, jars.length / 2));
        List<String> namesA = new ArrayList<>(), namesB = new ArrayList<>();
        List<byte[]> bytesA = new ArrayList<>();
        int ji = 0;
        for (String j : jars) {
            if (!j.endsWith(".jar")) continue;
            boolean isA = (ji++ % 2) == 0;
            int n = 0;
            try (JarFile jf = new JarFile(j)) {
                for (Enumeration<JarEntry> e = jf.entries(); e.hasMoreElements() && n < per; ) {
                    JarEntry je = e.nextElement();
                    String en = je.getName();
                    if (!en.endsWith(".class") || en.startsWith("META-INF/") || en.equals("module-info.class")
                            || en.endsWith("package-info.class")) continue;
                    String cn = en.substring(0, en.length() - 6).replace('/', '.');
                    if (isA) {
                        try (InputStream in = jf.getInputStream(je)) { bytesA.add(in.readAllBytes()); }
                        namesA.add(cn);
                    } else namesB.add(cn);
                    n++;
                }
            }
        }
        // a parent that defines nothing of set A: delegate to the platform loader only
        ClassLoader plat = ClassLoader.getPlatformClassLoader();
        for (int r = 0; r < rounds; r++) {
            Bare b = new Bare(plat);
            int ok = 0;
            long t0 = System.nanoTime();
            for (int i = 0; i < namesA.size(); i++) {
                try { b.def(namesA.get(i), bytesA.get(i)); ok++; } catch (Throwable t) { }
            }
            System.out.println("DEFINE-A round " + r + " defined=" + ok + " ms=" + (System.nanoTime() - t0) / 1_000_000);
        }
        ClassLoader app = ClassLoader.getSystemClassLoader();
        long t0 = System.nanoTime();
        int okB = 0;
        for (String n : namesB) { try { Class.forName(n, false, app); okB++; } catch (Throwable t) { } }
        long t1 = System.nanoTime();
        int okA = 0;
        for (String n : namesA) { try { Class.forName(n, false, app); okA++; } catch (Throwable t) { } }
        long t2 = System.nanoTime();
        System.out.println("APP-B (never defined elsewhere) n=" + namesB.size() + " ok=" + okB + " ms=" + (t1 - t0) / 1_000_000
                + " us/class=" + (t1 - t0) / 1000 / Math.max(1, namesB.size()));
        System.out.println("APP-A (same names defined by " + rounds + " other loaders) n=" + namesA.size() + " ok=" + okA + " ms="
                + (t2 - t1) / 1_000_000 + " us/class=" + (t2 - t1) / 1000 / Math.max(1, namesA.size()));
    }
}
