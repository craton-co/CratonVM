import java.io.File;
import java.io.InputStream;
import java.util.ArrayList;
import java.util.Enumeration;
import java.util.List;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;

/**
 * SBR5 lane clload: the cost of ClassLoader.defineClass alone (bytes pre-read), repeated with a
 * fresh loader per round so each round defines the same classes again. Isolates the native
 * defineClass1 path from the jar/URLClassPath lookup. usage: Sbr5ClloadDefine [N] [rounds]
 */
public class Sbr5ClloadDefine {
    static class Bare extends ClassLoader {
        Bare(ClassLoader p) { super(p); }
        Class<?> def(String n, byte[] b) { return defineClass(n, b, 0, b.length); }
    }
    public static void main(String[] a) throws Exception {
        int total = a.length > 0 ? Integer.parseInt(a[0]) : 1500;
        int rounds = a.length > 1 ? Integer.parseInt(a[1]) : 4;
        String[] jars = System.getProperty("java.class.path").split(File.pathSeparator);
        int per = Math.max(1, total / Math.max(1, jars.length));
        List<String> names = new ArrayList<>();
        List<byte[]> bytes = new ArrayList<>();
        for (String j : jars) {
            if (!j.endsWith(".jar")) continue;
            int n = 0;
            try (JarFile jf = new JarFile(j)) {
                for (Enumeration<JarEntry> e = jf.entries(); e.hasMoreElements() && n < per; ) {
                    JarEntry je = e.nextElement();
                    String en = je.getName();
                    if (!en.endsWith(".class") || en.startsWith("META-INF/") || en.equals("module-info.class")
                            || en.endsWith("package-info.class")) continue;
                    try (InputStream in = jf.getInputStream(je)) { bytes.add(in.readAllBytes()); }
                    names.add(en.substring(0, en.length() - 6).replace('/', '.'));
                    n++;
                }
            }
        }
        ClassLoader app = ClassLoader.getSystemClassLoader();
        // warm the app loader with the supertypes by loading everything once through it
        for (String n : names) { try { Class.forName(n, false, app); } catch (Throwable t) { } }
        int N = names.size();
        for (int r = 0; r < rounds; r++) {
            Bare b = new Bare(app);
            long t0 = System.nanoTime();
            int ok = 0;
            for (int i = 0; i < N; i++) {
                try { b.def(names.get(i), bytes.get(i)); ok++; } catch (Throwable t) { }
            }
            long t1 = System.nanoTime();
            System.out.println("DEFINE round " + r + " ok=" + ok + " ms=" + (t1 - t0) / 1_000_000 + " us/class=" + (t1 - t0) / 1000 / Math.max(1, N));
        }
    }
}
