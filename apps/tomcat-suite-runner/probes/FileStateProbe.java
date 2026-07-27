import java.io.File;
import java.io.FileOutputStream;

/**
 * Does File.exists()/isDirectory()/lastModified() go stale after the path is
 * deleted and/or recreated with a different kind? A cached stat would make
 * Tomcat's expand-then-undeploy-then-redeploy cycle early-return without
 * expanding.
 */
public class FileStateProbe {
    static int bad = 0;

    public static void main(String[] args) throws Exception {
        File base = new File(System.getProperty("java.io.tmpdir"), "fs-probe-" + System.nanoTime());
        base.mkdirs();
        File p = new File(base, "myapp");

        check("initially absent      ", !p.exists(), p.exists());
        p.mkdir();
        check("after mkdir exists    ", p.exists(), p.exists());
        check("after mkdir isDir     ", p.isDirectory(), p.isDirectory());
        deleteRec(p);
        check("after delete absent   ", !p.exists(), p.exists());
        check("after delete !isDir   ", !p.isDirectory(), p.isDirectory());

        // Recreate as a FILE at the same path.
        try (FileOutputStream out = new FileOutputStream(p)) {
            out.write(1);
        }
        check("recreated as file     ", p.exists() && p.isFile() && !p.isDirectory(),
                "exists=" + p.exists() + " isFile=" + p.isFile() + " isDir=" + p.isDirectory());
        long m1 = p.lastModified();
        Thread.sleep(1500);
        try (FileOutputStream out = new FileOutputStream(p)) {
            out.write(new byte[] { 1, 2, 3 });
        }
        long m2 = p.lastModified();
        check("lastModified advances ", m2 > m1, "m1=" + m1 + " m2=" + m2);
        check("length refreshed      ", p.length() == 3, "len=" + p.length());

        p.delete();
        base.delete();
        System.out.println(bad == 0 ? "ALL OK" : (bad + " MISMATCHES"));
    }

    static void check(String label, boolean ok, Object detail) {
        if (!ok) {
            bad++;
        }
        System.out.println(label + " : " + (ok ? "ok " : "BAD") + "   [" + detail + "]");
    }

    static void deleteRec(File f) {
        File[] kids = f.listFiles();
        if (kids != null) {
            for (File k : kids) {
                deleteRec(k);
            }
        }
        f.delete();
    }
}
