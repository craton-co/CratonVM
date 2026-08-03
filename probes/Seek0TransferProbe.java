import java.io.EOFException;
import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.IOException;
import java.nio.channels.FileChannel;

/**
 * Repro for `TestManagerWebapp` — `ExpandWar` file copy fails with
 * `IOException: seek0: bad fd for rw_seek`.
 *
 * `ExpandWar.copy(File,File)` copies each regular file with
 *
 *   FileInputStream fis; FileChannel ic = fis.getChannel();
 *   FileOutputStream fos; FileChannel oc = fos.getChannel();
 *   ic.transferTo(position, size, oc)
 *
 * On Windows `FileDispatcherImpl.transferToDirectlyNeedsPositionLock()` is
 * `true`, so `FileChannelImpl.transferToDirect` brackets the transfer with
 * `long pos = position(); ... position(pos);` — and `position()` calls
 * `nd.seek(fd, -1)` on the *FileInputStream*'s fd. CratonVM registered that
 * fd as a read-only `FileEntry::FileRead`, which `FdTable::rw_seek` did not
 * accept, so the very first `transferTo` threw.
 *
 * This probe drives the same shapes directly. Every step prints PASS/FAIL and
 * the process exits non-zero if any step failed, so it is usable as a
 * regression gate. No JUnit, no Tomcat — plain `java`/`cratonvm`.
 */
public class Seek0TransferProbe {

    private static int failures = 0;

    private static void check(String what, boolean ok, String detail) {
        System.out.println((ok ? "PASS " : "FAIL ") + what + (detail.isEmpty() ? "" : " — " + detail));
        if (!ok) {
            failures++;
        }
    }

    public static void main(String[] args) throws Exception {
        File dir = new File(System.getProperty("java.io.tmpdir"), "seek0probe-" + System.nanoTime());
        if (!dir.mkdirs()) {
            throw new IOException("cannot create " + dir);
        }
        try {
            File src = new File(dir, "src.bin");
            // 96 KiB — larger than FileChannelImpl's 16 KiB
            // MAPPED_TRANSFER_THRESHOLD and larger than transferTo0's 64 KiB
            // userspace chunk, so the copy loops at least twice.
            final int total = 96 * 1024;
            byte[] payload = new byte[total];
            for (int i = 0; i < total; i++) {
                payload[i] = (byte) (i * 31 + 7);
            }
            try (FileOutputStream o = new FileOutputStream(src)) {
                o.write(payload);
            }

            inputChannelPosition(src);
            outputChannelPositionAndSize(dir);
            expandWarCopy(src, new File(dir, "dst.bin"), payload);
            smallExpandWarCopy(dir);
            transferFromCopy(src, new File(dir, "dst2.bin"), payload);
        } finally {
            deleteRecursively(dir);
        }

        if (failures > 0) {
            System.out.println("FAILURES: " + failures);
            System.exit(1);
        }
        System.out.println("ALL OK");
    }

    /** `FileInputStream.getChannel().position()` — the exact seek0 that broke. */
    private static void inputChannelPosition(File src) {
        try (FileInputStream fis = new FileInputStream(src); FileChannel ic = fis.getChannel()) {
            long p0 = ic.position();
            check("FileInputStream channel position() at open", p0 == 0, "got " + p0);
            long sz = ic.size();
            check("FileInputStream channel size()", sz == src.length(), "got " + sz + " want " + src.length());
            ic.position(1024);
            long p1 = ic.position();
            check("FileInputStream channel position(1024)", p1 == 1024, "got " + p1);
            // The seek must be honoured by the following sequential read, i.e.
            // the buffered reader's buffer must not be left stale.
            java.nio.ByteBuffer bb = java.nio.ByteBuffer.allocate(4);
            ic.read(bb);
            byte expect = (byte) (1024 * 31 + 7);
            check("read after position(1024) sees the seeked byte", bb.array()[0] == expect,
                    "got " + bb.array()[0] + " want " + expect);
            long p2 = ic.position();
            check("position() advances after read", p2 == 1028, "got " + p2);
        } catch (IOException e) {
            check("FileInputStream channel position()", false, e.toString());
        }
    }

    /** `FileOutputStream.getChannel()` position/size — the write-side twin. */
    private static void outputChannelPositionAndSize(File dir) {
        File f = new File(dir, "out.bin");
        try (FileOutputStream fos = new FileOutputStream(f); FileChannel oc = fos.getChannel()) {
            long p0 = oc.position();
            check("FileOutputStream channel position() at open", p0 == 0, "got " + p0);
            oc.write(java.nio.ByteBuffer.wrap(new byte[] { 1, 2, 3, 4 }));
            long p1 = oc.position();
            check("FileOutputStream channel position() after write", p1 == 4, "got " + p1);
            long s1 = oc.size();
            check("FileOutputStream channel size()", s1 == 4, "got " + s1);
            oc.position(2);
            oc.write(java.nio.ByteBuffer.wrap(new byte[] { 9, 9 }));
            check("FileOutputStream channel position() after reposition+write", oc.position() == 4,
                    "got " + oc.position());
        } catch (IOException e) {
            check("FileOutputStream channel position()/size()", false, e.toString());
        }
        byte[] got = readAll(f);
        check("reposition+write landed at the right offset",
                got.length == 4 && got[0] == 1 && got[1] == 2 && got[2] == 9 && got[3] == 9,
                java.util.Arrays.toString(got));
    }

    /** Byte-for-byte the loop in `org.apache.catalina.startup.ExpandWar.copy`. */
    private static void expandWarCopy(File fileSrc, File fileDest, byte[] expected) {
        try (FileInputStream fis = new FileInputStream(fileSrc);
                FileChannel ic = fis.getChannel();
                FileOutputStream fos = new FileOutputStream(fileDest);
                FileChannel oc = fos.getChannel()) {
            long size = ic.size();
            long position = 0;
            while (size > 0) {
                long count = ic.transferTo(position, size, oc);
                if (count > 0) {
                    position += count;
                    size -= count;
                } else {
                    throw new EOFException();
                }
            }
        } catch (IOException e) {
            check("ExpandWar.copy transferTo loop (96 KiB)", false, e.toString());
            return;
        }
        byte[] got = readAll(fileDest);
        check("ExpandWar.copy transferTo loop (96 KiB)", java.util.Arrays.equals(got, expected),
                "copied " + got.length + " of " + expected.length + " bytes");
    }

    /**
     * The same loop for a file below `MAPPED_TRANSFER_THRESHOLD` — Tomcat's
     * `examples/index.html` is a few hundred bytes, and a small file takes a
     * different branch inside `FileChannelImpl.transferTo`.
     */
    private static void smallExpandWarCopy(File dir) {
        File src = new File(dir, "index.html");
        byte[] expected = "<html><body>examples</body></html>\n".getBytes(java.nio.charset.StandardCharsets.UTF_8);
        try (FileOutputStream o = new FileOutputStream(src)) {
            o.write(expected);
        } catch (IOException e) {
            check("small ExpandWar.copy setup", false, e.toString());
            return;
        }
        expandWarCopyNamed("ExpandWar.copy transferTo loop (small file)", src, new File(dir, "index-copy.html"),
                expected);
    }

    private static void expandWarCopyNamed(String label, File fileSrc, File fileDest, byte[] expected) {
        try (FileInputStream fis = new FileInputStream(fileSrc);
                FileChannel ic = fis.getChannel();
                FileOutputStream fos = new FileOutputStream(fileDest);
                FileChannel oc = fos.getChannel()) {
            long size = ic.size();
            long position = 0;
            while (size > 0) {
                long count = ic.transferTo(position, size, oc);
                if (count > 0) {
                    position += count;
                    size -= count;
                } else {
                    throw new EOFException();
                }
            }
        } catch (IOException e) {
            check(label, false, e.toString());
            return;
        }
        byte[] got = readAll(fileDest);
        check(label, java.util.Arrays.equals(got, expected), "copied " + got.length + " of " + expected.length);
    }

    /** The mirror direction — `oc.transferFrom(ic, ...)`, used by other copy helpers. */
    private static void transferFromCopy(File fileSrc, File fileDest, byte[] expected) {
        try (FileInputStream fis = new FileInputStream(fileSrc);
                FileChannel ic = fis.getChannel();
                FileOutputStream fos = new FileOutputStream(fileDest);
                FileChannel oc = fos.getChannel()) {
            long size = ic.size();
            long position = 0;
            while (size > 0) {
                long count = oc.transferFrom(ic, position, size);
                if (count <= 0) {
                    throw new EOFException();
                }
                position += count;
                size -= count;
            }
        } catch (IOException e) {
            check("transferFrom copy (96 KiB)", false, e.toString());
            return;
        }
        byte[] got = readAll(fileDest);
        check("transferFrom copy (96 KiB)", java.util.Arrays.equals(got, expected),
                "copied " + got.length + " of " + expected.length + " bytes");
    }

    private static byte[] readAll(File f) {
        try (FileInputStream in = new FileInputStream(f)) {
            java.io.ByteArrayOutputStream bos = new java.io.ByteArrayOutputStream();
            byte[] buf = new byte[8192];
            int n;
            while ((n = in.read(buf)) > 0) {
                bos.write(buf, 0, n);
            }
            return bos.toByteArray();
        } catch (IOException e) {
            return new byte[0];
        }
    }

    private static void deleteRecursively(File f) {
        File[] kids = f.listFiles();
        if (kids != null) {
            for (File k : kids) {
                deleteRecursively(k);
            }
        }
        // Best effort; the temp dir is disposable.
        f.delete();
    }
}
