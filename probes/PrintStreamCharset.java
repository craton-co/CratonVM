import java.io.*;
import java.nio.charset.*;

/** JDK 19+ OutputStreamWriter(OutputStream) asks a PrintStream for its
 *  charset(); StreamEncoder then calls newEncoder() on whatever comes back.
 *  If that is a fabricated bare java.nio.charset.Charset, newEncoder() is
 *  abstract and the call dies with "has no Code attribute". */
public class PrintStreamCharset {
    static int checks = 0, bad = 0;
    static void show(String what, PrintStream ps) {
        checks++;
        try {
            Charset c = ps.charset();
            String cls = (c == null) ? "null" : c.getClass().getName();
            boolean concrete = c != null && !cls.equals("java.nio.charset.Charset");
            if (!concrete) { bad++; System.out.println("  DIFF " + what + ".charset() -> " + cls); }
            else System.out.println("  ok   " + what + ".charset() -> " + cls + " / " + c.name());
            checks++;
            try { c.newEncoder(); System.out.println("  ok   " + what + ".charset().newEncoder()"); }
            catch (Throwable t) { bad++; System.out.println("  DIFF " + what
                    + ".charset().newEncoder() -> " + t.getClass().getName()); }
        } catch (Throwable t) { bad++; System.out.println("  DIFF " + what + ".charset() threw " + t); }
    }
    public static void main(String[] a) throws Exception {
        show("System.out", System.out);
        show("System.err", System.err);
        show("new PrintStream(baos)", new PrintStream(new ByteArrayOutputStream()));
        show("new PrintStream(baos,true,UTF-8)",
             new PrintStream(new ByteArrayOutputStream(), true, "UTF-8"));
        checks++;
        try { new OutputStreamWriter(System.err); System.out.println("  ok   new OutputStreamWriter(System.err)"); }
        catch (Throwable t) { bad++; System.out.println("  DIFF new OutputStreamWriter(System.err) -> "
                + t.getClass().getName() + ": " + t.getMessage()); }
        System.out.println(bad == 0 ? "PASS PrintStreamCharset (" + checks + " checks)"
                                    : "FAIL PrintStreamCharset (" + bad + " of " + checks + " wrong)");
    }
}
