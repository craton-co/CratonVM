import java.io.BufferedWriter;
import java.io.File;
import java.io.FileOutputStream;
import java.io.OutputStreamWriter;
import java.io.Writer;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;

/**
 * `bw_delegate_out` (native-builtins/src/phases_late.rs) decides whether a
 * `java.io.BufferedWriter` is CratonVM's own fd-backed object or a real one by
 * asking a single question: **is raw slot 0 an `Int`?**
 *
 * Its doc says a real BufferedWriter's slot 0 "holds an object reference (the
 * `lock`/`out` Writer set by the JDK constructor)". On the real JDK layout slot
 * 0 is `java.io.Writer.writeBuffer`, a `char[]` — `lock` is slot 1 and `out` is
 * slot 2. So the discriminator is not asking what it thinks it is asking, and
 * its answer for a real BufferedWriter depends entirely on what an
 * **unwritten** reference slot reads back as.
 *
 * If that is `Int(0)`, every real BufferedWriter is misclassified as fd-backed
 * with fd 0. This probe drives every BufferedWriter shape through the natives
 * that consult the discriminator and reports what came out the other end — all
 * paired properties, diffable against the host JDK.
 */
public final class BufferedWriterDiscriminatorProbe {

    private static void out(String key, Object value) {
        System.out.println("BWDISC " + key + "=" + value);
    }

    private static String slurp(Path p) throws Exception {
        return Files.readString(p, StandardCharsets.UTF_8).replace("\n", "\\n").replace("\r", "\\r");
    }

    public static void main(String[] args) throws Exception {
        File dir = Files.createTempDirectory("bwdisc").toFile();
        dir.deleteOnExit();

        // 1. Files.newBufferedWriter — the object the fd fast-path was written
        // for. `write(String,int,int)` is the overload the native shadows.
        Path a = new File(dir, "a.txt").toPath();
        try (BufferedWriter bw = Files.newBufferedWriter(a, StandardCharsets.UTF_8)) {
            bw.write("one", 0, 3);
            bw.newLine();
            bw.write("two");
            bw.flush();
        }
        out("newBufferedWriter.bytes", Files.size(a));
        out("newBufferedWriter.content", slurp(a));

        // 2. new BufferedWriter(new OutputStreamWriter(FileOutputStream)) —
        // the real three-layer chain. Same natives, different object shape.
        Path b = new File(dir, "b.txt").toPath();
        try (FileOutputStream fos = new FileOutputStream(b.toFile());
                OutputStreamWriter osw = new OutputStreamWriter(fos, StandardCharsets.UTF_8);
                BufferedWriter bw = new BufferedWriter(osw)) {
            bw.write("three", 0, 5);
            bw.newLine();
            bw.write("four");
            bw.flush();
        }
        out("chain.bytes", Files.size(b));
        out("chain.content", slurp(b));

        // 3. A BufferedWriter over a plain Writer that is neither — the
        // discriminator has no fd and no FileOutputStream to fall back on.
        java.io.StringWriter sw = new java.io.StringWriter();
        try (BufferedWriter bw = new BufferedWriter(sw)) {
            bw.write("five", 0, 4);
            bw.newLine();
            bw.write("six");
            bw.flush();
            out("stringwriter.content", sw.toString().replace("\n", "\\n").replace("\r", "\\r"));
        }

        // 4. Files.write / Files.writeString, the sibling entry points, so a
        // regression in the fd path shows up beside the BufferedWriter one.
        Path c = new File(dir, "c.txt").toPath();
        Files.writeString(c, "seven\neight\n", StandardCharsets.UTF_8);
        out("writeString.bytes", Files.size(c));
        out("writeString.content", slurp(c));

        // 5. Interleave: write, flush, write again, close. A discriminator that
        // answers differently once `writeBuffer` has been allocated by real
        // `Writer.write(String)` bytecode would diverge here and nowhere else.
        Path d = new File(dir, "d.txt").toPath();
        try (BufferedWriter bw = Files.newBufferedWriter(d, StandardCharsets.UTF_8)) {
            bw.write("nine");
            bw.flush();
            out("interleave.afterFirstFlush", Files.size(d));
            bw.write("-ten");
            bw.flush();
            out("interleave.afterSecondFlush", Files.size(d));
            bw.append('!');
        }
        out("interleave.content", slurp(d));

        // 6. The append and truncate OpenOptions, which reopen the same path.
        Path e = new File(dir, "e.txt").toPath();
        try (BufferedWriter bw = Files.newBufferedWriter(e, StandardCharsets.UTF_8)) {
            bw.write("head\n");
        }
        try (BufferedWriter bw =
                Files.newBufferedWriter(
                        e, StandardCharsets.UTF_8, java.nio.file.StandardOpenOption.APPEND)) {
            bw.write("tail\n");
        }
        out("append.content", slurp(e));

        try (BufferedWriter bw = Files.newBufferedWriter(e, StandardCharsets.UTF_8)) {
            bw.write("fresh\n");
        }
        out("truncate.content", slurp(e));

        // 7. `new BufferedWriter(new FileWriter(f))` — the shape the L4 census
        // probe uses, and the one whose slot-0 read logged `Int(0)` (i.e. the
        // discriminator classifying a REAL BufferedWriter as fd-backed, on
        // fd 0). Verify the bytes, not just that it did not throw.
        File g = new File(dir, "g.txt");
        try (Writer w = new BufferedWriter(new java.io.FileWriter(g))) {
            for (int i = 0; i < 5; i++) {
                w.write("line " + i + "\n");
            }
        }
        out("filewriter.bytes", g.length());
        out("filewriter.content", slurp(g.toPath()));

        // 8. Same shape, but read back through the chain the census uses, so a
        // write that lands on the wrong fd shows up as a short read here even
        // if the file length happens to look right.
        int lines = 0;
        int chars = 0;
        try (java.io.BufferedReader r =
                new java.io.BufferedReader(new java.io.FileReader(g))) {
            String s;
            while ((s = r.readLine()) != null) {
                lines++;
                chars += s.length();
            }
        }
        out("filewriter.readback", lines + "/" + chars);

        System.out.println("BWDISC-COMPLETE");
    }
}
