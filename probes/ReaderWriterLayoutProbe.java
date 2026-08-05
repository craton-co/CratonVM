import java.io.BufferedReader;
import java.io.BufferedWriter;
import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.InputStreamReader;
import java.io.OutputStreamWriter;
import java.io.Reader;
import java.io.Writer;
import java.lang.reflect.Field;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;

/**
 * The java.io Reader/Writer chain, observed the way the L4 shadow-layout diff
 * says it should be: not "does a round-trip work" (it does, that is why the
 * overlay survived) but "what is actually sitting in the slots the fabricated
 * model hands out".
 *
 * CratonVM's model for these four classes parks VM-internal values in slot 0
 * (and, for InputStreamReader, slot 1). On a real JDK layout those indices are
 * `Reader.lock` / `Reader.skipBuffer` / `Writer.writeBuffer` — fields the JDK
 * owns. Every line below prints a PAIRED property so the transcript can be
 * diffed byte-for-byte against the host JDK; a divergence is the finding.
 *
 * Run with:
 *   java --add-opens java.base/java.io=ALL-UNNAMED -cp . ReaderWriterLayoutProbe
 * The --add-opens is ignored by CratonVM and required by HotSpot; where it is
 * refused the probe prints the refusal, which is itself a paired property.
 */
public final class ReaderWriterLayoutProbe {

    private static void out(String key, Object value) {
        System.out.println("RWLAYOUT " + key + "=" + value);
    }

    /** Describe a value by KIND, never by identity — addresses are not stable. */
    private static String kind(Object v) {
        if (v == null) {
            return "null";
        }
        Class<?> c = v.getClass();
        if (c.isArray()) {
            return c.getComponentType().getName() + "[" + java.lang.reflect.Array.getLength(v) + "]";
        }
        return c.getName();
    }

    /**
     * Read a declared field of `owner` off `target` and report its KIND.
     *
     * The point is the field the fabricated model overlays, so the read has to
     * go through the real declaration — `getDeclaredField` scans the real
     * class's metadata and finds nothing a native invented.
     */
    private static void fieldKind(String label, Class<?> owner, Object target, String name) {
        try {
            Field f = owner.getDeclaredField(name);
            f.setAccessible(true);
            out(label, kind(f.get(target)));
        } catch (NoSuchFieldException e) {
            out(label, "NoSuchField");
        } catch (RuntimeException | IllegalAccessException e) {
            // An inaccessible-object refusal is a paired property too: HotSpot
            // without --add-opens and CratonVM must at least agree on which.
            out(label, e.getClass().getName());
        }
    }

    public static void main(String[] args) throws Exception {
        File dir = Files.createTempDirectory("rwlayout").toFile();
        dir.deleteOnExit();
        Path p = new File(dir, "probe.txt").toPath();

        // ---- 1. Files.newBufferedWriter — the one LIVE real-JDK-mode writer
        // of BufferedWriter slot 0 in this VM.
        try (BufferedWriter bw = Files.newBufferedWriter(p, StandardCharsets.UTF_8)) {
            fieldKind("nbw.writeBuffer.beforeWrite", Writer.class, bw, "writeBuffer");
            fieldKind("nbw.lock.beforeWrite", Writer.class, bw, "lock");
            bw.write("alpha\n");
            bw.write("beta\n");
            // `Writer.write(String)` is the method that allocates writeBuffer on
            // a real JDK. Whether it has been allocated by now is exactly the
            // accident the BufferedWriter finding rests on.
            fieldKind("nbw.writeBuffer.afterWrite", Writer.class, bw, "writeBuffer");
            out("nbw.class", bw.getClass().getName());
        }
        out("nbw.bytes", Files.size(p));
        out("nbw.content", Files.readString(p, StandardCharsets.UTF_8).replace("\n", "\\n"));

        // ---- 2. OutputStreamWriter over a FileOutputStream.
        Path p2 = new File(dir, "probe2.txt").toPath();
        try (FileOutputStream fos = new FileOutputStream(p2.toFile());
                OutputStreamWriter osw = new OutputStreamWriter(fos, StandardCharsets.UTF_8)) {
            fieldKind("osw.writeBuffer", Writer.class, osw, "writeBuffer");
            fieldKind("osw.lock", Writer.class, osw, "lock");
            fieldKind("osw.se", OutputStreamWriter.class, osw, "se");
            osw.write("gamma\n");
            osw.flush();
            fieldKind("osw.writeBuffer.afterWrite", Writer.class, osw, "writeBuffer");
        }
        out("osw.bytes", Files.size(p2));
        out("osw.content", Files.readString(p2, StandardCharsets.UTF_8).replace("\n", "\\n"));

        // ---- 3. BufferedWriter wrapping an OutputStreamWriter — the classic
        // three-layer chain, and the one `native_bw_init` was written for.
        Path p3 = new File(dir, "probe3.txt").toPath();
        try (FileOutputStream fos = new FileOutputStream(p3.toFile());
                OutputStreamWriter osw = new OutputStreamWriter(fos, StandardCharsets.UTF_8);
                BufferedWriter bw = new BufferedWriter(osw)) {
            fieldKind("chain.bw.writeBuffer", Writer.class, bw, "writeBuffer");
            fieldKind("chain.bw.lock", Writer.class, bw, "lock");
            bw.write("delta\n");
            bw.newLine();
            bw.flush();
            fieldKind("chain.bw.writeBuffer.afterWrite", Writer.class, bw, "writeBuffer");
        }
        out("chain.bytes", Files.size(p3));
        out("chain.content", Files.readString(p3, StandardCharsets.UTF_8).replace("\n", "\\n"));

        // ---- 4. InputStreamReader over a FileInputStream, then a
        // BufferedReader over that. Slots 0 and 1 of the ISR model.
        try (FileInputStream fis = new FileInputStream(p.toFile());
                InputStreamReader isr = new InputStreamReader(fis, StandardCharsets.UTF_8)) {
            fieldKind("isr.lock", Reader.class, isr, "lock");
            fieldKind("isr.skipBuffer", Reader.class, isr, "skipBuffer");
            fieldKind("isr.sd", InputStreamReader.class, isr, "sd");
            out("isr.encoding", isr.getEncoding());
            int first = isr.read();
            out("isr.firstChar", first);
            fieldKind("isr.skipBuffer.afterRead", Reader.class, isr, "skipBuffer");
            char[] rest = new char[16];
            int n = isr.read(rest, 0, rest.length);
            out("isr.restCount", n);
            out("isr.rest", new String(rest, 0, Math.max(n, 0)).replace("\n", "\\n"));
        }

        try (FileInputStream fis = new FileInputStream(p.toFile());
                InputStreamReader isr = new InputStreamReader(fis, StandardCharsets.UTF_8);
                BufferedReader br = new BufferedReader(isr)) {
            fieldKind("br.lock", Reader.class, br, "lock");
            fieldKind("br.skipBuffer", Reader.class, br, "skipBuffer");
            out("br.line1", br.readLine());
            out("br.line2", br.readLine());
            out("br.line3", br.readLine());
            fieldKind("br.lock.afterRead", Reader.class, br, "lock");
        }

        // ---- 5. `skip` is the one public API that forces a real
        // `Reader.skipBuffer` allocation — the ISR slot-1 overlay's blast
        // radius, if it has one.
        try (FileInputStream fis = new FileInputStream(p.toFile());
                InputStreamReader isr = new InputStreamReader(fis, StandardCharsets.UTF_8)) {
            out("skip.skipped", isr.skip(3));
            fieldKind("skip.skipBuffer.afterSkip", Reader.class, isr, "skipBuffer");
            out("skip.nextChar", isr.read());
        }

        // ---- 6. `getEncoding()` reports the JDK's HISTORICAL charset name,
        // not the canonical one — `StreamDecoder.encodingName()` asks
        // `HistoricallyNamedCharset`, which most of java.base implements. The
        // single divergence in this probe's first run: CratonVM said "UTF-8"
        // where HotSpot says "UTF8".
        String[] charsets = {
            "UTF-8", "UTF-16", "UTF-16BE", "UTF-16LE", "US-ASCII", "ISO-8859-1",
            "ISO-8859-15", "windows-1252", "KOI8-R", "IBM850", "Shift_JIS",
            "EUC-JP", "GB2312", "GBK", "Big5",
        };
        for (String cn : charsets) {
            try {
                java.nio.charset.Charset cs = java.nio.charset.Charset.forName(cn);
                try (InputStreamReader r =
                                new InputStreamReader(new java.io.ByteArrayInputStream(new byte[0]), cs);
                        OutputStreamWriter w =
                                new OutputStreamWriter(new java.io.ByteArrayOutputStream(), cs)) {
                    out("enc." + cn, cs.name() + "|" + r.getEncoding() + "|" + w.getEncoding());
                }
            } catch (Exception e) {
                out("enc." + cn, e.getClass().getName());
            }
        }

        System.out.println("RWLAYOUT-COMPLETE");
    }
}
