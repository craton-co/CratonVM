import java.io.File;
import java.io.InputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.util.ArrayList;
import java.util.Enumeration;
import java.util.List;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;

/**
 * SBR5 lane clload: where does the cold class-load cost go? Times the public pieces of the JDK
 * loader's path over the same class names as Sbr5ClloadLoadProbe, one phase at a time, in order:
 * getJarEntry, getInputStream+readAllBytes, appLoader.getResource (URLClassPath lookup + URL),
 * a fresh URLClassLoader.loadClass (findClass + defineClass), appLoader Class.forName.
 */
public class Sbr5ClloadParts {
    static long t;
    static void lap(String what, int n) {
        long now = System.nanoTime();
        System.out.println("PART " + what + " ms=" + (now - t) / 1_000_000 + " us/class=" + (now - t) / 1000 / Math.max(1, n));
        t = System.nanoTime();
    }

    public static void main(String[] a) throws Exception {
        int total = a.length > 0 ? Integer.parseInt(a[0]) : 1500;
        String[] jars = System.getProperty("java.class.path").split(File.pathSeparator);
        int per = Math.max(1, total / Math.max(1, jars.length));
        List<String> names = new ArrayList<>();
        List<Integer> jarOf = new ArrayList<>();
        List<JarFile> jfs = new ArrayList<>();
        List<URL> urls = new ArrayList<>();
        for (String j : jars) {
            if (!j.endsWith(".jar")) continue;
            JarFile jf = new JarFile(j);
            int ji = jfs.size();
            jfs.add(jf);
            urls.add(new File(j).toURI().toURL());
            int n = 0;
            for (Enumeration<JarEntry> e = jf.entries(); e.hasMoreElements() && n < per; ) {
                String en = e.nextElement().getName();
                if (!en.endsWith(".class") || en.startsWith("META-INF/") || en.equals("module-info.class")
                        || en.endsWith("package-info.class")) continue;
                names.add(en.substring(0, en.length() - 6));
                jarOf.add(ji);
                n++;
            }
        }
        int N = names.size();
        System.out.println("classes=" + N + " jars=" + jfs.size());
        t = System.nanoTime();
        int cnt = 0;
        for (int rep = 0; rep < 6; rep++) {
            cnt = 0;
            for (int i = 0; i < N; i++) {
                if (jfs.get(jarOf.get(i)).getJarEntry(names.get(i) + ".class") != null) cnt++;
            }
            lap("getJarEntry(own jar) rep" + rep, N);
        }
        long bytes = 0;
        for (int i = 0; i < N; i++) {
            JarFile jf = jfs.get(jarOf.get(i));
            JarEntry je = jf.getJarEntry(names.get(i) + ".class");
            try (InputStream in = jf.getInputStream(je)) { bytes += in.readAllBytes().length; }
        }
        lap("getInputStream+readAll", N);
        ClassLoader app = ClassLoader.getSystemClassLoader();
        int found = 0;
        for (int i = 0; i < N; i++) {
            if (app.getResource(names.get(i) + ".class") != null) found++;
        }
        lap("appLoader.getResource", N);
        int ok = 0;
        for (int i = 0; i < N; i++) {
            try { Class.forName(names.get(i).replace('/', '.'), false, app); ok++; } catch (Throwable x) { }
        }
        lap("appLoader Class.forName", N);
        // defineClass alone: bytes pre-read, a bare loader whose parent is the (already warm) app loader.
        byte[][] all = new byte[N][];
        for (int i = 0; i < N; i++) {
            JarFile jf = jfs.get(jarOf.get(i));
            try (InputStream in = jf.getInputStream(jf.getJarEntry(names.get(i) + ".class"))) { all[i] = in.readAllBytes(); }
        }
        t = System.nanoTime();
        class Bare extends ClassLoader {
            Bare(ClassLoader p) { super(p); }
            Class<?> def(String n, byte[] b) { return defineClass(n, b, 0, b.length); }
        }
        Bare bare = new Bare(app);
        int dok = 0;
        for (int i = 0; i < N; i++) {
            try { bare.def(names.get(i).replace('/', '.'), all[i]); dok++; } catch (Throwable x) { }
        }
        lap("defineClass(bare, pre-read)=" + dok, N);
        for (int rep = 0; rep < 5; rep++) {
            URLClassLoader ucl = new URLClassLoader(urls.toArray(new URL[0]), null);
            for (int i = 0; i < N; i++) {
                try { ucl.loadClass(names.get(i).replace('/', '.')); ok++; } catch (Throwable x) { }
            }
            lap("URLClassLoader.loadClass rep" + rep, N);
        }
        System.out.println("check " + cnt + " " + bytes + " " + found + " " + ok);
    }
}
