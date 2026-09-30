// Write the encoding properties to a FILE, so the measurement can be taken
// while stdout/stderr/stdin are still attached to a real console. Redirecting
// stdout to capture it is precisely what changes the answer.
//
//   javac -d . WinEnc.java
//   java -cp . WinEnc <outfile>
import java.io.PrintWriter;
import java.nio.charset.Charset;

public class WinEnc {
    public static void main(String[] a) throws Exception {
        try (PrintWriter w = new PrintWriter(a[0], "UTF-8")) {
            for (String k : new String[] {
                    "file.encoding", "native.encoding", "sun.jnu.encoding",
                    "stdout.encoding", "stderr.encoding", "stdin.encoding" }) {
                w.println("prop " + k + " = " + System.getProperty(k));
            }
            w.println("cset System.out = " + name(System.out.charset()));
            w.println("cset System.err = " + name(System.err.charset()));
            w.println("cset default    = " + name(Charset.defaultCharset()));
            w.println("console         = " + (System.console() == null ? "null" : "present"));
        }
    }
    static String name(Charset c) { return c == null ? "null" : c.name(); }
}
