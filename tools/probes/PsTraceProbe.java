import java.io.IOException;
import java.io.OutputStream;
import java.io.PrintStream;

/** Which call produces the third flush in PrintStream close+checkError. */
public class PsTraceProbe {
    static final StringBuilder TRACE = new StringBuilder();

    static final class TraceOut extends OutputStream {
        private final IOException onClose;
        TraceOut(IOException onClose) { this.onClose = onClose; }
        @Override public void write(int b) { mark("write"); }
        @Override public void flush() { mark("flush"); }
        @Override public void close() throws IOException {
            mark("close");
            if (onClose != null) throw onClose;
        }
        static void mark(String s) {
            if (TRACE.length() > 0) TRACE.append(',');
            TRACE.append(s);
        }
    }

    public static void main(String[] a) {
        TraceOut sink = new TraceOut(new IOException("ps-close-io"));
        PrintStream p = new PrintStream(sink);
        System.out.println("after ctor       : " + TRACE);
        p.close();
        System.out.println("after close()    : " + TRACE);
        boolean e1 = p.checkError();
        System.out.println("after checkError : " + TRACE + "   (returned " + e1 + ")");
        boolean e2 = p.checkError();
        System.out.println("after checkError2: " + TRACE + "   (returned " + e2 + ")");
        p.close();
        System.out.println("after 2nd close(): " + TRACE);
        System.out.println("RESULT done");
    }
}
