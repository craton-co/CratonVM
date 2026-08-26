import java.nio.charset.*;
import java.io.*;
import java.util.logging.*;

/** Narrow the ConsoleHandler <init> failure. The stack was
 *    ConsoleHandler.<init> -> StreamHandler.setOutputStream
 *      -> OutputStreamWriter.<init>(OutputStream,String) : 110
 *      -> StreamEncoder.forOutputStreamWriter -> Charset.newEncoder [no Code]
 *  so the STRING-named overload and whatever charset name it resolves are the
 *  suspects, not newEncoder() in general. */
public class CharsetEncoderReach2 {
    static int checks = 0, bad = 0;
    static void probe(String what, Runnable r) {
        checks++;
        try { r.run(); System.out.println("  ok   " + what); }
        catch (Throwable t) { bad++; System.out.println("  DIFF " + what + " -> "
                + t.getClass().getName() + ": " + t.getMessage()); }
    }
    public static void main(String[] a) {
        System.out.println("  -- stdout.encoding=" + System.getProperty("stdout.encoding")
                         + " stderr.encoding=" + System.getProperty("stderr.encoding")
                         + " native.encoding=" + System.getProperty("native.encoding")
                         + " file.encoding=" + System.getProperty("file.encoding"));
        probe("new OutputStreamWriter(baos, \"UTF-8\")  [String overload]", () -> {
            try { new OutputStreamWriter(new ByteArrayOutputStream(), "UTF-8"); }
            catch (UnsupportedEncodingException e) { throw new RuntimeException(e); } });
        for (String n : new String[]{"UTF-8", "windows-1251", "Cp1252", "US-ASCII"}) {
            probe("Charset.forName(\"" + n + "\") class", () -> {
                Charset c = Charset.forName(n);
                System.out.println("        -> " + c.getClass().getName() + " / " + c.name()); });
            probe("forName(\"" + n + "\").newEncoder()", () -> Charset.forName(n).newEncoder());
        }
        probe("new StreamHandler(baos, new SimpleFormatter())", () -> {
            new StreamHandler(new ByteArrayOutputStream(), new SimpleFormatter()); });
        probe("new ConsoleHandler()", () -> new ConsoleHandler());
        System.out.println(bad == 0 ? "PASS CharsetEncoderReach2 (" + checks + " checks)"
                                    : "FAIL CharsetEncoderReach2 (" + bad + " of " + checks + " wrong)");
    }
}
