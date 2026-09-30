// Residual of `bug-printstream-charset-answers-the-abstract-base-20260825-FIXED-20260901.md`
// §5: "CratonVM answers UTF-8 where HotSpot answers the console encoding
// (Cp1251 on this host, from `stdout.encoding`)."
//
// Print every encoding-shaped system property plus the Charset objects the
// JDK derives from them, so a CratonVM run can be diffed against a HotSpot
// run under the SAME locale/console.
//
//   javac -d probes/out probes/EncodingFidelity.java
//   java -cp probes/out EncodingFidelity
import java.io.*;
import java.nio.charset.Charset;

public class EncodingFidelity {
    static void p(String k) {
        System.out.println("prop " + k + " = " + System.getProperty(k));
    }
    static void cs(String label, Charset c) {
        System.out.println("cset " + label + " = "
            + (c == null ? "null" : c.name() + " [" + c.getClass().getName() + "]"));
    }
    public static void main(String[] a) throws Exception {
        p("file.encoding");
        p("native.encoding");
        p("sun.jnu.encoding");
        p("stdout.encoding");
        p("stderr.encoding");
        p("stdin.encoding");
        p("sun.stdout.encoding");
        p("sun.stderr.encoding");
        p("sun.io.unicode.encoding");
        cs("Charset.defaultCharset", Charset.defaultCharset());
        cs("System.out.charset", System.out.charset());
        cs("System.err.charset", System.err.charset());
        Console con = System.console();
        System.out.println("console = " + (con == null ? "null" : "present"));
        if (con != null) cs("Console.charset", con.charset());
        // What an OutputStreamWriter over System.out actually encodes with.
        OutputStreamWriter w = new OutputStreamWriter(new ByteArrayOutputStream());
        System.out.println("osw.default.encoding = " + w.getEncoding());
        // A PrintStream with no explicit charset picks up stdout.encoding on
        // HotSpot only for System.out; a fresh one uses Console/default.
        cs("new PrintStream(baos).charset",
            new PrintStream(new ByteArrayOutputStream()).charset());
        System.out.println("DONE");
    }
}
