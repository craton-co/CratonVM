import java.io.*;
import java.net.*;
import java.util.concurrent.CountDownLatch;

/// Timeline probe for TestNonBlockingAPI.testNonBlockingReadAsync's exact
/// client-side path: HttpURLConnection.setFixedLengthStreamingMode + a tight
/// os.write()+os.flush() loop of small chunks, against a plain ServerSocket
/// that only reads (no real Tomcat). Prints timestamps for each phase so a
/// slow phase (connect/header-send vs. steady-state body write) can be told
/// apart from an aggregate-throughput explanation.
///
/// Usage: cratonvm --jdk-only -cp <dir> HttpUrlConnStreamingTimeline [iters] [chunkBytes]
public class HttpUrlConnStreamingTimeline {
    public static void main(String[] args) throws Exception {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 2000000;
        int chunkBytes = args.length > 1 ? Integer.parseInt(args[1]) : 8;
        byte[] chunk = new byte[chunkBytes];

        ServerSocket ss = new ServerSocket(0);
        int port = ss.getLocalPort();
        CountDownLatch acceptLatch = new CountDownLatch(1);
        long[] serverAcceptNanos = new long[1];
        long[] serverFirstByteNanos = new long[1];
        Thread server = new Thread(() -> {
            try {
                Socket s = ss.accept();
                serverAcceptNanos[0] = System.nanoTime();
                acceptLatch.countDown();
                InputStream is = s.getInputStream();
                byte[] buf = new byte[65536];
                long total = 0;
                boolean gotFirst = false;
                int rd;
                while ((rd = is.read(buf)) > 0) {
                    if (!gotFirst) {
                        serverFirstByteNanos[0] = System.nanoTime();
                        gotFirst = true;
                    }
                    total += rd;
                    // Drain and respond minimally like a servlet would eventually,
                    // but this probe only cares about the request side timeline.
                    if (total >= (long) iters * chunkBytes) {
                        break;
                    }
                }
                OutputStream os = s.getOutputStream();
                os.write("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK".getBytes());
                os.flush();
                System.out.println("server total read = " + total);
                s.close();
            } catch (Exception e) {
                System.out.println("server exception: " + e);
            }
        });
        server.setDaemon(true);
        server.start();

        long t0 = System.nanoTime();
        URL url = URI.create("http://127.0.0.1:" + port + "/").toURL();
        HttpURLConnection conn = (HttpURLConnection) url.openConnection();
        conn.setDoOutput(true);
        conn.setRequestMethod("POST");
        conn.setFixedLengthStreamingMode((long) iters * chunkBytes);
        long t1 = System.nanoTime();
        System.out.println("openConnection+setup: " + (t1 - t0) / 1_000_000.0 + " ms");

        conn.connect();
        long t2 = System.nanoTime();
        System.out.println("connect(): " + (t2 - t1) / 1_000_000.0 + " ms");
        System.out.println("accept latch count before getOutputStream(): " + acceptLatch.getCount());

        OutputStream os = conn.getOutputStream();
        long t4 = System.nanoTime();
        System.out.println("getOutputStream(): " + (t4 - t2) / 1_000_000.0 + " ms");
        System.out.println("accept latch count after getOutputStream(): " + acceptLatch.getCount());

        acceptLatch.await();
        long t3 = System.nanoTime();
        System.out.println("server accept observed at +" + (serverAcceptNanos[0] - t0) / 1_000_000.0 + " ms from t0");

        long firstWriteStart = System.nanoTime();
        os.write(chunk);
        os.flush();
        long firstWriteEnd = System.nanoTime();
        System.out.println("first write+flush: " + (firstWriteEnd - firstWriteStart) / 1_000_000.0 + " ms");

        long tenStart = System.nanoTime();
        for (int i = 1; i < 10 && i < iters; i++) {
            os.write(chunk);
            os.flush();
        }
        long tenEnd = System.nanoTime();
        System.out.println("writes 2..10: " + (tenEnd - tenStart) / 1_000_000.0 + " ms total, " +
                (tenEnd - tenStart) / 1_000_000.0 / 9 + " ms/write avg");

        long hundredStart = System.nanoTime();
        int already = Math.min(10, iters);
        for (int i = already; i < 100 && i < iters; i++) {
            os.write(chunk);
            os.flush();
        }
        long hundredEnd = System.nanoTime();
        int did = Math.min(100, iters) - already;
        if (did > 0) {
            System.out.println("writes " + (already+1) + ".." + Math.min(100, iters) + ": " +
                    (hundredEnd - hundredStart) / 1_000_000.0 + " ms total, " +
                    (hundredEnd - hundredStart) / 1_000_000.0 / did + " ms/write avg");
        }

        long remainStart = System.nanoTime();
        int done = Math.min(100, iters);
        for (int i = done; i < iters; i++) {
            os.write(chunk);
            os.flush();
        }
        long remainEnd = System.nanoTime();
        System.out.println("remaining " + (iters - done) + " writes: " + (remainEnd - remainStart) / 1_000.0 + " us total");
        os.close();

        long t5 = System.nanoTime();
        System.out.println("total client time since t0: " + (t5 - t0) / 1_000.0 + " us");

        int rc = conn.getResponseCode();
        long t6 = System.nanoTime();
        System.out.println("getResponseCode()=" + rc + " took " + (t6 - t5) / 1_000_000.0 + " ms");

        server.join(5000);
        ss.close();
        System.out.println("DONE OK");
    }
}
