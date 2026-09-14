import java.io.OutputStream;
import java.io.PrintStream;

/** Does a flush/checkError AFTER a CLEAN close set trouble on HotSpot? */
public class PsCleanCloseProbe {
    static final StringBuilder T = new StringBuilder();
    static final class Quiet extends OutputStream {
        @Override public void write(int b) { mark("write"); }
        @Override public void flush() { mark("flush"); }
        @Override public void close() { mark("close"); }
        static void mark(String s) { if (T.length() > 0) T.append(','); T.append(s); }
    }

    public static void main(String[] a) {
        PrintStream p = new PrintStream(new Quiet());
        p.print("x");
        System.out.println("after print       : trace=" + T + " checkError=" + p.checkError());
        p.close();
        System.out.println("after close       : trace=" + T);
        System.out.println("checkError post   : " + p.checkError() + "  trace=" + T);
        p.flush();
        System.out.println("explicit flush    : trace=" + T);
        System.out.println("checkError again  : " + p.checkError() + "  trace=" + T);
        p.print("y");
        System.out.println("print after close : trace=" + T + "  checkError=" + p.checkError());
        System.out.println("RESULT done");
    }
}
