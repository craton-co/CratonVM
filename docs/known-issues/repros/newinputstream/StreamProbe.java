import java.io.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;

/**
 * Differential probe for Files.newInputStream / FileSystemProvider.newInputStream.
 *
 * The headline case is Lucene's StringHelper.<clinit>: eight bytes out of
 * /dev/urandom. A newInputStream that slurps the whole "file" first never
 * returns on a character device.
 */
public class StreamProbe {
    static void p(String label, Object v) {
        System.out.println(label + "=" + v);
        System.out.flush();
    }

    static <T> void check(String label, ThrowingSupplier<T> s) {
        try {
            p(label, s.get());
        } catch (Throwable t) {
            p(label, "THREW " + t.getClass().getName()
                + (t.getMessage() == null ? "" : ": " + t.getMessage()));
        }
    }

    interface ThrowingSupplier<T> {
        T get() throws Exception;
    }

    public static void main(String[] a) throws Exception {
        // 1. The Lucene pattern, verbatim: 8 bytes from an infinite device.
        check("urandom.readLong.terminates", () -> {
            try (DataInputStream in =
                     new DataInputStream(Files.newInputStream(Paths.get("/dev/urandom")))) {
                long v = in.readLong();
                return v != 0 || true; // value is random; only termination matters
            }
        });
        check("urandom.twoReadsDiffer", () -> {
            long x, y;
            try (DataInputStream in =
                     new DataInputStream(Files.newInputStream(Paths.get("/dev/urandom")))) {
                x = in.readLong();
            }
            try (DataInputStream in =
                     new DataInputStream(Files.newInputStream(Paths.get("/dev/urandom")))) {
                y = in.readLong();
            }
            return x != y;
        });
        check("zero.first8", () -> {
            try (InputStream in = Files.newInputStream(Paths.get("/dev/zero"))) {
                byte[] b = new byte[8];
                int n = in.readNBytes(b, 0, 8);
                return n + ":" + b[0] + b[7];
            }
        });

        // 2. Same thing through the provider SPI (a separate registration).
        check("provider.urandom.readLong.terminates", () -> {
            Path p = Paths.get("/dev/urandom");
            try (DataInputStream in =
                     new DataInputStream(p.getFileSystem().provider().newInputStream(p))) {
                in.readLong();
                return true;
            }
        });

        // 3. Ordinary files must still stream correctly end to end.
        Path f = Path.of("streamprobe-data.bin");
        byte[] payload = new byte[300_000];
        for (int i = 0; i < payload.length; i++) {
            payload[i] = (byte) (i * 31);
        }
        Files.write(f, payload);
        check("file.readAllBytes.roundTrip", () -> {
            try (InputStream in = Files.newInputStream(f)) {
                return java.util.Arrays.equals(in.readAllBytes(), payload);
            }
        });
        check("file.singleByteReads", () -> {
            try (InputStream in = Files.newInputStream(f)) {
                int b0 = in.read(), b1 = in.read(), b2 = in.read();
                return b0 + "," + b1 + "," + b2;
            }
        });
        check("file.skipThenRead", () -> {
            try (InputStream in = Files.newInputStream(f)) {
                long skipped = in.skip(100);
                int b = in.read();
                return skipped + ":" + b + " expect=100:" + (payload[100] & 0xff);
            }
        });
        check("file.readPastEofIsMinusOne", () -> {
            try (InputStream in = Files.newInputStream(f)) {
                in.readAllBytes();
                return in.read();
            }
        });
        check("file.availableAtStart", () -> {
            try (InputStream in = Files.newInputStream(f)) {
                return in.available();
            }
        });
        check("file.bufferedReaderLines", () -> {
            Path t = Path.of("streamprobe-text.txt");
            Files.write(t, "alpha\nbeta\ngamma\n".getBytes(StandardCharsets.UTF_8));
            try (BufferedReader r = new BufferedReader(
                     new InputStreamReader(Files.newInputStream(t), StandardCharsets.UTF_8))) {
                return r.readLine() + "|" + r.readLine() + "|" + r.readLine() + "|" + r.readLine();
            }
        });

        // 4. Error contract.
        check("missing.throwsNoSuchFile", () -> {
            try (InputStream in = Files.newInputStream(Path.of("no-such-file-here.bin"))) {
                return "NO THROW";
            }
        });

        // 5. Jar entries go through the in-memory VFS path, not an fd.
        check("jarEntry.readsThroughFileSystems", () -> {
            String cp = System.getProperty("java.class.path");
            String jar = null;
            for (String e : cp.split(java.io.File.pathSeparator)) {
                if (e.endsWith(".jar")) {
                    jar = e;
                    break;
                }
            }
            if (jar == null) {
                return "no-jar-on-classpath";
            }
            try (FileSystem fs = FileSystems.newFileSystem(Path.of(jar))) {
                Path mf = fs.getPath("META-INF/MANIFEST.MF");
                if (!Files.exists(mf)) {
                    return "no-manifest";
                }
                try (InputStream in = Files.newInputStream(mf)) {
                    return "manifestBytes=" + in.readAllBytes().length;
                }
            }
        });

        p("DONE", "ok");
    }
}
