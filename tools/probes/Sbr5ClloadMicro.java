import java.io.File;
import java.net.URL;
import java.security.CodeSigner;
import java.security.CodeSource;
import java.util.concurrent.ConcurrentHashMap;
import java.util.jar.Attributes;
import java.util.jar.JarFile;
import java.util.jar.Manifest;

/**
 * SBR5 lane clload: per-class Java operations of BuiltinClassLoader.defineClass, timed alone
 * (3 rounds of 3000, us/op). The cold-load profile's top Java rows were these.
 */
public class Sbr5ClloadMicro {
    static long sink;
    interface Op { void run(int i); }
    static void time(String what, int n, Op op) {
        StringBuilder sb = new StringBuilder("MICRO " + what + " us/op:");
        for (int r = 0; r < 3; r++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < n; i++) op.run(r * n + i);
            sb.append(' ').append((System.nanoTime() - t0) / 1000.0 / n);
        }
        System.out.println(sb);
    }
    public static void main(String[] a) throws Exception {
        int n = 3000;
        String[] names = new String[3 * n];
        for (int i = 0; i < names.length; i++) names[i] = "org.example.pkg" + (i % 40) + ".Klass" + i;
        ConcurrentHashMap<String, Object> locks = new ConcurrentHashMap<>();
        time("lockmap putIfAbsent", n, i -> { Object nl = new Object(); Object l = locks.putIfAbsent(names[i], nl); sink += (l == null ? nl : l).hashCode(); });
        time("replace+concat", n, i -> sink += names[i].replace('.', '/').concat(".class").length());
        URL base = new URL("jar:file:/data/x/some.jar!/");
        time("new URL(base, path)", n, i -> { try { sink += new URL(base, "org/example/pkg" + (i % 40) + "/Klass" + i + ".class").hashCode(); } catch (Exception e) { } });
        URL u = new URL("file:/data/x/some.jar");
        time("new CodeSource+hash", n, i -> { CodeSource cs = new CodeSource(u, (CodeSigner[]) null); sink += cs.hashCode(); });
        String jar = System.getProperty("java.class.path").split(File.pathSeparator)[0];
        Manifest man;
        try (JarFile jf = new JarFile(jar)) { man = jf.getManifest(); }
        Manifest m = man == null ? new Manifest() : man;
        time("manifest 8 getValue", n, i -> {
            Attributes at = m.getMainAttributes();
            sink += String.valueOf(at.getValue(Attributes.Name.SPECIFICATION_TITLE)).length()
                + String.valueOf(at.getValue(Attributes.Name.SPECIFICATION_VERSION)).length()
                + String.valueOf(at.getValue(Attributes.Name.SPECIFICATION_VENDOR)).length()
                + String.valueOf(at.getValue(Attributes.Name.IMPLEMENTATION_TITLE)).length()
                + String.valueOf(at.getValue(Attributes.Name.IMPLEMENTATION_VERSION)).length()
                + String.valueOf(at.getValue(Attributes.Name.IMPLEMENTATION_VENDOR)).length()
                + String.valueOf(at.getValue(Attributes.Name.SEALED)).length();
        });
        time("lastIndexOf+substring", n, i -> { String cn = names[i]; int p = cn.lastIndexOf('.'); sink += cn.substring(0, p).length(); });
        System.out.println("sink " + sink);
    }
}
