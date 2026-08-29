import java.io.*;
import java.nio.file.*;

public class L4Diag2 {
    static void t(String tag, R r) {
        try { r.run(); System.out.println(tag + " |no-throw|"); }
        catch (Throwable e) { System.out.println(tag + " |THREW " + e.getClass().getName() + ": " + e.getMessage() + "|"); }
    }
    interface R { void run() throws Throwable; }

    public static void main(String[] a) throws Exception {
        Path base = Paths.get("l4diag2");
        Files.createDirectories(base);
        Path missing = base.resolve("nope.txt");
        t("Files.lines(missing)", () -> Files.lines(missing).count());
        t("Files.lines(missing) no terminal", () -> Files.lines(missing));
        t("Files.readAllLines(missing)", () -> Files.readAllLines(missing));
        t("Files.readString(missing)", () -> Files.readString(missing));

        Path f = base.resolve("f.bin");
        Files.write(f, new byte[]{1, 2});
        FileInputStream in = new FileInputStream(f.toFile());
        System.out.println("read1 |" + in.read() + "|");
        System.out.println("read2 |" + in.read() + "|");
        System.out.println("read3 (EOF) |" + in.read() + "|");
        System.out.println("skip(4) at EOF |" + in.skip(4) + "|");
        System.out.println("channel position |" + in.getChannel().position() + "|");
        in.close();
        FileInputStream in2 = new FileInputStream(f.toFile());
        System.out.println("skip(1) fresh |" + in2.skip(1) + "|");
        System.out.println("skip(10) past end |" + in2.skip(10) + "|");
        in2.close();

        Files.walk(base).sorted(java.util.Comparator.reverseOrder()).forEach(p -> {
            try { Files.deleteIfExists(p); } catch (IOException e) { }
        });
        System.out.println("DONE L4Diag2");
    }
}
