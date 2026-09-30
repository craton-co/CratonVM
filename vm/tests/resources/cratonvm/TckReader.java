package cratonvm;
import java.io.*;
public class TckReader {
    public static int stringreader_read() {
        try {
            StringReader sr = new StringReader("hello");
            return sr.read() == 'h' ? 1 : 0;
        } catch (Exception e) { return 0; }
    }
    public static int stringreader_readArray() {
        try {
            StringReader sr = new StringReader("hello");
            char[] buf = new char[5];
            int n = sr.read(buf);
            return n == 5 && new String(buf).equals("hello") ? 1 : 0;
        } catch (Exception e) { return 0; }
    }
    public static int stringreader_mark_reset() {
        try {
            StringReader sr = new StringReader("abcdef");
            sr.read(); // a
            sr.mark(10);
            sr.read(); // b
            sr.read(); // c
            sr.reset();
            return sr.read() == 'b' ? 1 : 0;
        } catch (Exception e) { return 0; }
    }
    public static int stringreader_ready() {
        try {
            StringReader sr = new StringReader("hello");
            return sr.ready() ? 1 : 0;
        } catch (Exception e) { return 0; }
    }
    public static int stringreader_close() {
        try {
            StringReader sr = new StringReader("hello");
            sr.close();
            try { sr.read(); return 0; } catch (IOException e) { return 1; }
        } catch (Exception e) { return 0; }
    }
    public static int bufferedreader_readLine() {
        try {
            BufferedReader br = new BufferedReader(new StringReader("line1\nline2\nline3"));
            String l1 = br.readLine();
            String l2 = br.readLine();
            return "line1".equals(l1) && "line2".equals(l2) ? 1 : 0;
        } catch (Exception e) { return 0; }
    }
    public static int bufferedreader_mark_reset() {
        try {
            BufferedReader br = new BufferedReader(new StringReader("abcdef"));
            br.read(); // a
            br.mark(10);
            br.read(); // b
            br.reset();
            return br.read() == 'b' ? 1 : 0;
        } catch (Exception e) { return 0; }
    }
    public static int inputstreamreader_basic() {
        try {
            byte[] bytes = "hello".getBytes("UTF-8");
            InputStreamReader isr = new InputStreamReader(new ByteArrayInputStream(bytes), "UTF-8");
            return isr.read() == 'h' ? 1 : 0;
        } catch (Exception e) { return 0; }
    }
}
