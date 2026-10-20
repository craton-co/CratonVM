import java.nio.*;
import java.nio.channels.*;
import java.nio.file.*;
import java.io.*;
import java.util.Set;

/** Repro for SC-resource-io-family Cause A: Files.newByteChannel(...) must
 *  return a functional SeekableByteChannel (write/read/position/size/truncate),
 *  not a methodless interface stub (AbstractMethodError on write/read). */
public class ResourceIoRepro {
    static int pass = 0, fail = 0;
    static void check(String name, boolean ok, String detail) {
        if (ok) pass++; else fail++;
        System.out.println((ok ? "PASS " : "FAIL ") + name + (detail == null ? "" : " :: " + detail));
    }

    public static void main(String[] a) throws Exception {
        Path p = Files.createTempFile("cvchan", ".bin");

        // WRITE then verify length on disk.
        try (SeekableByteChannel ch = Files.newByteChannel(p, Set.of(StandardOpenOption.WRITE))) {
            int n = ch.write(ByteBuffer.wrap("hello world".getBytes()));
            check("write returns 11", n == 11, "n=" + n);
        }
        check("file size after write == 11", Files.size(p) == 11, "size=" + Files.size(p));

        // READ back through a fresh channel.
        try (SeekableByteChannel ch = Files.newByteChannel(p, Set.of(StandardOpenOption.READ))) {
            check("read channel size() == 11", ch.size() == 11, "size=" + ch.size());
            ByteBuffer buf = ByteBuffer.allocate(64);
            int n = ch.read(buf);
            check("read returns 11", n == 11, "n=" + n);
            buf.flip();
            byte[] got = new byte[buf.remaining()];
            buf.get(got);
            check("read content == 'hello world'", "hello world".equals(new String(got)), new String(got));
        }

        // position()/seek: re-open for read, seek to 6, read the rest.
        try (SeekableByteChannel ch = Files.newByteChannel(p, Set.of(StandardOpenOption.READ))) {
            ch.position(6);
            check("position() reports 6", ch.position() == 6, "pos=" + ch.position());
            ByteBuffer buf = ByteBuffer.allocate(64);
            ch.read(buf);
            buf.flip();
            byte[] got = new byte[buf.remaining()];
            buf.get(got);
            check("read after seek(6) == 'world'", "world".equals(new String(got)), new String(got));
        }

        // TRUNCATE_EXISTING+WRITE replaces content.
        try (SeekableByteChannel ch = Files.newByteChannel(p,
                Set.of(StandardOpenOption.WRITE, StandardOpenOption.TRUNCATE_EXISTING))) {
            ch.write(ByteBuffer.wrap("hi".getBytes()));
        }
        check("size after truncate+write == 2", Files.size(p) == 2, "size=" + Files.size(p));
        check("content after truncate == 'hi'", "hi".equals(new String(Files.readAllBytes(p))),
                new String(Files.readAllBytes(p)));

        // Missing-file contract: READ on a non-existent path → NoSuchFileException.
        Files.deleteIfExists(p);
        Path missing = p.resolveSibling("cvchan-does-not-exist-" + System.nanoTime() + ".bin");
        try {
            Files.newByteChannel(missing, Set.of(StandardOpenOption.READ));
            check("missing-file READ throws NoSuchFileException", false, "no exception");
        } catch (NoSuchFileException e) {
            check("missing-file READ throws NoSuchFileException", true, "NSFE");
        } catch (IOException e) {
            check("missing-file READ throws NoSuchFileException", false, e.getClass().getName());
        }
        // CREATE on a non-existent path SUCCEEDS and creates the file.
        try (SeekableByteChannel ch = Files.newByteChannel(missing,
                Set.of(StandardOpenOption.WRITE, StandardOpenOption.CREATE))) {
            ch.write(ByteBuffer.wrap("new".getBytes()));
        }
        check("CREATE on missing path writes file", Files.exists(missing) && Files.size(missing) == 3,
                "exists=" + Files.exists(missing));
        Files.deleteIfExists(missing);

        System.out.println("RESULT pass=" + pass + " fail=" + fail);
        if (fail != 0) System.exit(1);
    }
}
