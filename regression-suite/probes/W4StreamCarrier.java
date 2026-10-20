import java.io.*;
import java.lang.reflect.Modifier;
import java.net.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;

/**
 * Does anything still hand out a BARE `java.io.InputStream`-typed receiver?
 *
 * `native_bais_read_bytes`'s receiver guard (`input_stream_has_bais_layout`)
 * admits `class_name == "java/io/InputStream"` explicitly, and the comment
 * beside it names the population it is for:
 *
 *   "synthetic streams (URL.openStream, getResourceAsStream) that materialise
 *    as bare InputStream-typed receivers but actually have the
 *    ByteArrayInputStream layout in slots 0..3"
 *
 * That population is the reason 25 section-1.4 shadows in `native-io` cannot be
 * retired (`WORKER-4-2` section 4). This probe asks whether it still exists.
 *
 * `java.io.InputStream` is ABSTRACT, so any line below reporting it — or any
 * other abstract/interface class — is a JVMS 6.5 defect with no oracle run
 * required, exactly like `W4Abstract`. Every other line is a plain
 * oracle-vs-VM class-identity diff.
 *
 * The CONTENT of each stream is printed too: a carrier that reports the right
 * class and the wrong bytes is the failure this probe must not miss.
 */
public class W4StreamCarrier {

    static int abstractOrInterface = 0;
    static int total = 0;

    static void tag(String what, Object o) {
        total++;
        if (o == null) { System.out.println("CK " + what + " null"); return; }
        Class<?> c = o.getClass();
        int m = c.getModifiers();
        String kind = c.isInterface() ? "INTERFACE"
                : Modifier.isAbstract(m) ? "ABSTRACT" : "CONCRETE";
        if (!"CONCRETE".equals(kind)) abstractOrInterface++;
        System.out.println("CK " + what + " " + c.getName() + " [" + kind + "]");
    }

    static String slurp(InputStream in) throws IOException {
        if (in == null) return "<null stream>";
        ByteArrayOutputStream bo = new ByteArrayOutputStream();
        byte[] buf = new byte[64];
        int n;
        while ((n = in.read(buf)) > 0) bo.write(buf, 0, n);
        return bo.toString("UTF-8");
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("w4carrier");
        Path f = dir.resolve("payload.txt");
        String body = "carrier-probe-payload-0123456789";
        Files.write(f, body.getBytes(StandardCharsets.UTF_8));

        // --- URL.openStream() over file: ---------------------------------
        URL fileUrl = f.toUri().toURL();
        try (InputStream in = fileUrl.openStream()) {
            tag("url.file.openStream", in);
            System.out.println("CK url.file.content " + slurp(in));
        }
        // ...and the same stream read one byte at a time, which is the path
        // `InputStream.read()` takes rather than `read(byte[],int,int)`.
        try (InputStream in = fileUrl.openStream()) {
            StringBuilder sb = new StringBuilder();
            int b;
            while ((b = in.read()) >= 0) sb.append((char) b);
            System.out.println("CK url.file.byteAtATime " + sb);
        }
        // ...and available()/skip(), the other two base-class rows.
        try (InputStream in = fileUrl.openStream()) {
            System.out.println("CK url.file.skip " + in.skip(8));
            System.out.println("CK url.file.afterSkip " + slurp(in));
        }
        try (InputStream in = fileUrl.openStream()) {
            System.out.println("CK url.file.readAllBytes "
                    + new String(in.readAllBytes(), StandardCharsets.UTF_8));
        }

        // --- URLConnection.getInputStream() ------------------------------
        URLConnection conn = fileUrl.openConnection();
        try (InputStream in = conn.getInputStream()) {
            tag("urlconn.file.getInputStream", in);
            System.out.println("CK urlconn.file.content " + slurp(in));
        }

        // --- Class / ClassLoader getResourceAsStream ---------------------
        // A resource that is certain to exist in every image.
        try (InputStream in = Object.class.getResourceAsStream("/java/lang/Object.class")) {
            tag("class.getResourceAsStream.jrt", in);
            System.out.println("CK class.getResourceAsStream.jrt.firstBytes "
                    + (in == null ? "<null>" : firstBytes(in, 8)));
        }
        try (InputStream in = ClassLoader.getSystemResourceAsStream("java/lang/Object.class")) {
            tag("loader.getSystemResourceAsStream.jrt", in);
        }
        try (InputStream in = W4StreamCarrier.class.getClassLoader()
                .getResourceAsStream("W4StreamCarrier.class")) {
            tag("loader.getResourceAsStream.cp", in);
            System.out.println("CK loader.getResourceAsStream.cp.firstBytes "
                    + (in == null ? "<null>" : firstBytes(in, 8)));
        }

        // --- The comparison points that are already known concrete -------
        try (InputStream in = Files.newInputStream(f)) { tag("files.newInputStream", in); }
        try (InputStream in = new FileInputStream(f.toFile())) { tag("new FileInputStream", in); }
        tag("new ByteArrayInputStream", new ByteArrayInputStream(new byte[4]));

        System.out.println("W4CARRIER-SUMMARY total=" + total
                + " abstractOrInterface=" + abstractOrInterface);
        System.out.println(abstractOrInterface == 0 ? "PASS W4StreamCarrier" : "FAIL W4StreamCarrier");
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }

    static String firstBytes(InputStream in, int n) throws IOException {
        byte[] b = new byte[n];
        int got = in.read(b, 0, n);
        StringBuilder sb = new StringBuilder("len=" + got + " ");
        for (int i = 0; i < Math.max(got, 0); i++) sb.append(String.format("%02x", b[i]));
        return sb.toString();
    }
}
