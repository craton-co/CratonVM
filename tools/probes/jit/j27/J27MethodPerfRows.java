// TestMethodPerformance's two loops at probe size (docs/known-issues/jit/small-jdk-method-call-overhead-...).
// Needs the Tomcat classes on the class path (host: -cp "$(cat /root/tc-cp.txt):<dir>").
//   mb      mb.setBytes(IN,0,3); mb.toStringType()        (the test's first loop)
//   method  Method.bytesToString(IN,0,3)                  (the second loop)
//   java -Dj27.timing=true -cp ... J27MethodPerfRows [iterations [reps [row]]]
// Facts (HotSpot and CratonVM must agree): the converted string and its identity class.
import java.nio.charset.StandardCharsets;

import org.apache.tomcat.util.buf.MessageBytes;
import org.apache.tomcat.util.http.Method;

public class J27MethodPerfRows {
    static final byte[] IN = "GET /context-path/servlet-path/path-info HTTP/1.1".getBytes(StandardCharsets.UTF_8);
    static final MessageBytes mb = MessageBytes.newInstance();
    static Object sink;

    static long mb(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            mb.setBytes(IN, 0, 3);
            String r = mb.toStringType();
            sink = r;
            s += r.length();
        }
        return s;
    }

    static long method(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            String m = Method.bytesToString(IN, 0, 3);
            sink = m;
            s += m.length();
        }
        return s;
    }

    public static void main(String[] a) {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 1_000_000;
        int reps = a.length > 1 ? Integer.parseInt(a[1]) : 5;
        String only = a.length > 2 ? a[2] : "";
        mb.setBytes(IN, 0, 3);
        System.out.println("fact mb " + mb.toStringType() + " " + Method.bytesToString(IN, 0, 3));
        boolean timing = Boolean.getBoolean("j27.timing");
        long chk = 0;
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            if (!only.equals("method")) chk += mb(n);
            long t1 = System.nanoTime();
            if (!only.equals("mb")) chk += method(n);
            long t2 = System.nanoTime();
            if (timing) System.out.println("time mb-rep" + r + " " + (t1 - t0) / (double) n);
            if (timing) System.out.println("time method-rep" + r + " " + (t2 - t1) / (double) n);
        }
        System.out.println("fact checksum " + chk);
    }
}
