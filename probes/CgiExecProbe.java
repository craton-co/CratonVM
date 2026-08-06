import java.io.*;

/**
 * Mirrors exactly what Tomcat's CGIServlet.CGIRunner.run() does:
 *   Runtime.getRuntime().exec(String[] cmdarray, String[] envp, File dir)
 * then parses the CGI headers off the child's stdout a byte at a time
 * (Tomcat's HTTPHeaderInputStream, which stops at the blank line so the
 * BufferedReader above it cannot over-read), re-fetches getInputStream()
 * and copies the rest as the HTTP response body.
 *
 * HotSpot prints the script's output. A VM that loses the child's stdout
 * prints an empty body — the shape of the TestSecurity2019 failure.
 */
public class CgiExecProbe {

    /** Byte-for-byte the state machine in CGIServlet.HTTPHeaderInputStream. */
    static class HeaderStream extends InputStream {
        private static final int STATE_CHARACTER = 0;
        private static final int STATE_FIRST_CR = 1;
        private static final int STATE_FIRST_LF = 2;
        private static final int STATE_SECOND_CR = 3;
        private static final int STATE_HEADER_END = 4;
        private final InputStream input;
        private int state = STATE_CHARACTER;

        HeaderStream(InputStream in) { this.input = in; }

        @Override
        public int read() throws IOException {
            if (state == STATE_HEADER_END) {
                return -1;
            }
            int i = input.read();
            if (i == 10) {
                if (state == STATE_FIRST_CR || state == STATE_CHARACTER) {
                    state = STATE_FIRST_LF;
                } else {
                    state = STATE_HEADER_END;
                }
            } else if (i == 13) {
                state = (state == STATE_FIRST_LF) ? STATE_SECOND_CR : STATE_FIRST_CR;
            } else {
                state = STATE_CHARACTER;
            }
            return i;
        }
    }

    public static void main(String[] args) throws Exception {
        File dir = new File(System.getProperty("java.io.tmpdir"), "cgiprobe" + System.nanoTime());
        dir.mkdirs();
        File script = new File(dir, "test.sh");
        try (FileWriter fw = new FileWriter(script)) {
            fw.write("#!/bin/sh\n");
            fw.write("echo \"Content-Type: text/plain\"\n");
            fw.write("echo\n");
            fw.write("echo \"Query string: $QUERY_STRING\"\n");
        }
        script.setExecutable(true);

        String[] cmdarray = { script.getAbsolutePath() };
        String[] envp = { "QUERY_STRING=firstName=Dimitris", "PATH=/usr/bin:/bin" };

        Process proc = Runtime.getRuntime().exec(cmdarray, envp, dir);
        System.out.println("PROC_CLASS=" + proc.getClass().getName());

        InputStream in = proc.getInputStream();
        System.out.println("STDOUT_STREAM_CLASS=" + in.getClass().getName());

        BufferedReader hdr = new BufferedReader(new InputStreamReader(new HeaderStream(in)));
        String line;
        int headerCount = 0;
        StringBuilder headers = new StringBuilder();
        while ((line = hdr.readLine()) != null && !line.isEmpty()) {
            headerCount++;
            headers.append('[').append(line).append(']');
        }
        System.out.println("HEADER_COUNT=" + headerCount + " HEADERS=" + headers);

        // ...then re-fetches getInputStream() for the body.
        InputStream body = proc.getInputStream();
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        byte[] buf = new byte[2048];
        int n;
        while ((n = body.read(buf)) != -1) {
            bos.write(buf, 0, n);
        }
        String bodyText = bos.toString("UTF-8");
        System.out.println("BODY_LEN=" + bodyText.length());
        System.out.println("BODY=" + bodyText.replace("\n", "\\n"));

        System.out.println("WAIT_FOR=" + proc.waitFor());
        System.out.println("EXIT_VALUE=" + proc.exitValue());
        System.out.println("PID_NONZERO=" + (proc.pid() > 0));

        InputStream err = proc.getErrorStream();
        System.out.println("STDERR_EOF=" + (err.read() == -1));

        System.out.println("VERDICT=" + ((headerCount == 1
                && bodyText.contains("Query string: firstName=Dimitris"))
                ? "CGI_OK" : "CGI_BROKEN"));
    }
}
