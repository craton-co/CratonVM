import java.io.File;

/** Minimal check that File.setLastModified works for a DIRECTORY, not just a file. */
public class SetLastModProbe {
    public static void main(String[] args) throws Exception {
        File base = new File(System.getProperty("java.io.tmpdir"), "slm-probe-" + System.nanoTime());
        File dir = new File(base, "adir");
        if (!dir.mkdirs()) {
            throw new IllegalStateException("mkdirs failed: " + dir);
        }
        File file = new File(base, "afile.txt");
        try (java.io.FileOutputStream out = new java.io.FileOutputStream(file)) {
            out.write("x".getBytes("UTF-8"));
        }

        long target = 1600000000000L; // 2020-09-13T12:26:40Z

        boolean fileOk = file.setLastModified(target);
        boolean dirOk = dir.setLastModified(target);
        System.out.println("file.setLastModified -> " + fileOk + " (now " + file.lastModified() + ")");
        System.out.println("dir.setLastModified  -> " + dirOk + " (now " + dir.lastModified() + ")");
        System.out.println("file matches=" + (file.lastModified() == target)
                + " dir matches=" + (dir.lastModified() == target));

        File missing = new File(base, "nope");
        System.out.println("missing.setLastModified -> " + missing.setLastModified(target)
                + " (expect false)");

        dir.delete();
        file.delete();
        base.delete();
    }
}
