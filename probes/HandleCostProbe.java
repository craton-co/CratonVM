import java.io.File;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;

/**
 * Isolates the per-call cost of individual JarFile natives. Each loop lives in
 * its OWN static method so the JIT can compile it (a loop inline in main() is
 * refused OSR in this VM and every arm would report the interpreter floor).
 */
public class HandleCostProbe {
    static long sink;

    static long loopGetName(JarFile jf, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) { sink += jf.getName().length(); }
        return System.nanoTime() - t0;
    }

    static long loopSize(JarFile jf, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) { sink += jf.size(); }
        return System.nanoTime() - t0;
    }

    static long loopGetEntry(JarFile jf, String name, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) { sink += jf.getJarEntry(name) != null ? 1 : 0; }
        return System.nanoTime() - t0;
    }

    static long loopGetEntryMiss(JarFile jf, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) { sink += jf.getJarEntry("no/such/entry") != null ? 1 : 0; }
        return System.nanoTime() - t0;
    }

    public static void main(String[] a) throws Exception {
        File f = new File(a[0]);
        int n = Integer.parseInt(a[1]);
        try (JarFile jf = new JarFile(f, true, JarFile.OPEN_READ, Runtime.version())) {
            String name = null;
            java.util.Enumeration<JarEntry> en = jf.entries();
            while (en.hasMoreElements()) {
                JarEntry e = en.nextElement();
                if (!e.isDirectory()) { name = e.getName(); break; }
            }
            System.out.println("probe entry=" + name + " size=" + jf.size());
            for (int round = 0; round < 4; round++) {
                // Order rotates so a warm-up artifact cannot masquerade as a
                // per-call cost.
                long sz, gn, ge, gm;
                if (round % 2 == 0) {
                    gn = loopGetName(jf, n);
                    sz = loopSize(jf, n);
                    ge = loopGetEntry(jf, name, n);
                    gm = loopGetEntryMiss(jf, n);
                } else {
                    gm = loopGetEntryMiss(jf, n);
                    ge = loopGetEntry(jf, name, n);
                    sz = loopSize(jf, n);
                    gn = loopGetName(jf, n);
                }
                System.out.println("ROUND " + round
                        + " getName_ns=" + (gn / n)
                        + " size_ns=" + (sz / n)
                        + " getJarEntry_ns=" + (ge / n)
                        + " getJarEntryMiss_ns=" + (gm / n)
                        + " sink=" + sink);
            }
        }
    }
}
