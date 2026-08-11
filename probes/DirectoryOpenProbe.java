// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.File;
import java.io.FileInputStream;
import java.io.FileNotFoundException;
import java.io.FileOutputStream;
import java.io.RandomAccessFile;

/**
 * Opening a <em>directory</em> as a byte stream must fail.
 *
 * <p>On Linux the raw {@code open(2)} of a directory with {@code O_RDONLY}
 * succeeds — it is {@code read(2)} that returns {@code EISDIR}. The JDK closes
 * that gap in {@code io_util_md.c handleOpen}, which fstats the fresh fd and
 * fails with {@code EISDIR} when {@code S_ISDIR}, so
 * {@code new FileInputStream(dir)} throws {@link FileNotFoundException} on
 * every platform.
 *
 * <p>Tomcat's {@code FileResource.doGetInputStream()} depends on exactly that:
 * it calls {@code new FileInputStream(resource)} unconditionally and treats the
 * {@link FileNotFoundException} as "not a readable resource", returning
 * {@code null}. A VM whose open succeeds for a directory hands the servlet
 * layer a live stream for a directory instead
 * ({@code AbstractTestResourceSet.testGetResourceDirWithTrailingFileSeperator}).
 *
 * <p>Both the plain and the trailing-separator spelling of the same directory
 * are probed, because the suite failure named only the trailing-separator form
 * and the two spellings must behave identically.
 */
public final class DirectoryOpenProbe {

    private static int failures = 0;

    private static void check(String what, boolean ok, String detail) {
        System.out.println((ok ? "PASS " : "FAIL ") + what + " -> " + detail);
        if (!ok) {
            failures++;
        }
    }

    /** Every read-open API must reject {@code dir}; {@code label} names the spelling. */
    private static void probeDirectory(File dir, String label) {
        check(label + " isDirectory()", dir.isDirectory(), dir.getPath());

        try (FileInputStream in = new FileInputStream(dir)) {
            check(label + " new FileInputStream(dir) throws", false, "opened " + in);
        } catch (FileNotFoundException e) {
            check(label + " new FileInputStream(dir) throws", true, String.valueOf(e.getMessage()));
        } catch (Exception e) {
            check(label + " new FileInputStream(dir) throws FileNotFoundException", false,
                    e.getClass().getName() + ": " + e.getMessage());
        }

        try (FileInputStream in = new FileInputStream(dir.getPath())) {
            check(label + " new FileInputStream(String) throws", false, "opened " + in);
        } catch (FileNotFoundException e) {
            check(label + " new FileInputStream(String) throws", true,
                    String.valueOf(e.getMessage()));
        } catch (Exception e) {
            check(label + " new FileInputStream(String) throws FileNotFoundException", false,
                    e.getClass().getName() + ": " + e.getMessage());
        }

        try (RandomAccessFile raf = new RandomAccessFile(dir, "r")) {
            check(label + " new RandomAccessFile(dir, \"r\") throws", false, "opened " + raf);
        } catch (FileNotFoundException e) {
            check(label + " new RandomAccessFile(dir, \"r\") throws", true,
                    String.valueOf(e.getMessage()));
        } catch (Exception e) {
            check(label + " new RandomAccessFile(dir, \"r\") throws FileNotFoundException", false,
                    e.getClass().getName() + ": " + e.getMessage());
        }

        try (FileOutputStream out = new FileOutputStream(dir)) {
            check(label + " new FileOutputStream(dir) throws", false, "opened " + out);
        } catch (FileNotFoundException e) {
            check(label + " new FileOutputStream(dir) throws", true, String.valueOf(e.getMessage()));
        } catch (Exception e) {
            check(label + " new FileOutputStream(dir) throws FileNotFoundException", false,
                    e.getClass().getName() + ": " + e.getMessage());
        }
    }

    public static void main(String[] args) throws Exception {
        File base = new File(args.length > 0 ? args[0] : ".");
        File dir = new File(base, "probe-dir-" + ProcessHandle.current().pid());
        if (!dir.mkdirs() && !dir.isDirectory()) {
            System.out.println("FAIL could not create " + dir);
            System.exit(1);
        }
        try {
            probeDirectory(dir, "plain");
            probeDirectory(new File(dir.getPath() + File.separator), "trailing-separator");

            // A regular file must still open, so the fix cannot be "reject
            // everything": this is the control arm.
            File file = new File(dir, "f.txt");
            try (FileOutputStream out = new FileOutputStream(file)) {
                out.write("hello".getBytes("UTF-8"));
            }
            try (FileInputStream in = new FileInputStream(file)) {
                check("control: regular file still opens and reads", in.read() == 'h', file.getPath());
            } catch (Exception e) {
                check("control: regular file still opens and reads", false,
                        e.getClass().getName() + ": " + e.getMessage());
            }
            file.delete();
        } finally {
            dir.delete();
        }

        System.out.println(failures == 0 ? "PROBE OK" : "PROBE FAILURES=" + failures);
        if (failures != 0) {
            System.exit(1);
        }
    }
}
