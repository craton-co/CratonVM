import java.nio.charset.StandardCharsets;

import org.apache.tomcat.util.buf.MessageBytes;

/**
 * Reproduces the first hot loop in TestMethodPerformance without JUnit's
 * outer structure. The count is configurable so a failure can be narrowed
 * without changing the Tomcat fixture.
 */
public final class OsrMessageBytesProbe {
    private static final byte[] INPUT =
            "GET /context-path/servlet-path/path-info HTTP/1.1".getBytes(StandardCharsets.UTF_8);
    private static final MessageBytes MB = MessageBytes.newInstance();

    public static void main(String[] args) {
        int iterations = args.length == 0 ? 20_000_000 : Integer.parseInt(args[0]);
        long start = System.nanoTime();
        for (int i = 0; i < iterations; i++) {
            MB.setBytes(INPUT, 0, 3);
            MB.toStringType();
        }
        System.out.println("message-bytes=" + (System.nanoTime() - start));
    }
}
