import java.io.File;
import java.util.ArrayList;
import java.util.Collections;
import java.util.Enumeration;
import java.util.List;
import java.util.Random;
import java.util.concurrent.ConcurrentHashMap;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;

/**
 * SBR5 lane clload: 8 threads load the same class names through the application loader in different orders;
 * every name must resolve to ONE Class object in all threads, with the same number of failures as HotSpot.
 */
public class Sbr5ClloadConcurrent {
    public static void main(String[] a) throws Exception {
        List<String> names = new ArrayList<>();
        for (String j : System.getProperty("java.class.path").split(File.pathSeparator)) {
            if (!j.endsWith(".jar")) continue;
            int n = 0;
            try (JarFile jf = new JarFile(j)) {
                for (Enumeration<JarEntry> e = jf.entries(); e.hasMoreElements() && n < 40; ) {
                    String en = e.nextElement().getName();
                    if (!en.endsWith(".class") || en.startsWith("META-INF/") || en.endsWith("module-info.class") || en.endsWith("package-info.class")) continue;
                    names.add(en.substring(0, en.length() - 6).replace('/', '.')); n++;
                }
            }
        }
        ClassLoader l = ClassLoader.getSystemClassLoader();
        ConcurrentHashMap<String, Class<?>> seen = new ConcurrentHashMap<>();
        int T = 8;
        int[] errs = new int[T], diff = new int[T];
        Thread[] ts = new Thread[T];
        for (int t = 0; t < T; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                List<String> mine = new ArrayList<>(names);
                Collections.shuffle(mine, new Random(id));
                for (String n : mine) {
                    try {
                        Class<?> c = Class.forName(n, false, l);
                        Class<?> prev = seen.putIfAbsent(n, c);
                        if (prev != null && prev != c) diff[id]++;
                    } catch (Throwable x) { errs[id]++; }
                }
            });
            ts[t].start();
        }
        for (Thread t : ts) t.join();
        int e = 0, d = 0; for (int i = 0; i < T; i++) { e += errs[i]; d += diff[i]; }
        System.out.println("names=" + names.size() + " loaded=" + seen.size() + " identityMismatches=" + d + " errorsPerThreadEqual=" + (e % T == 0) + " errorsPerThread=" + (e / T));
    }
}
