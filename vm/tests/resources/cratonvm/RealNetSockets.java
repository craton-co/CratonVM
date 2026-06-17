package cratonvm;

import java.io.InputStream;
import java.io.OutputStream;
import java.net.ServerSocket;
import java.net.Socket;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * Smoke test for the CRATONVM_REAL_NET_SOCKETS real-path gate.
 *
 * Run under CRATONVM_REAL_NET_SOCKETS=1 to verify that the socket layer
 * runs real JDK bytecode instead of the synthetic overlay. The test opens
 * a loopback ServerSocket on an ephemeral port, connects a client from the
 * same JVM, sends a byte, and verifies the exchange.
 */
public class RealNetSockets {
    public static void main(String[] args) throws Exception {
        CountDownLatch serverDone = new CountDownLatch(1);
        byte[] received = {0};

        ServerSocket ss = new ServerSocket(0);
        int port = ss.getLocalPort();

        Thread server = new Thread(() -> {
            try (Socket conn = ss.accept()) {
                received[0] = (byte) conn.getInputStream().read();
                serverDone.countDown();
            } catch (Exception e) {
                e.printStackTrace();
            } finally {
                try { ss.close(); } catch (Exception ignored) {}
            }
        });
        server.setDaemon(true);
        server.start();

        try (Socket client = new Socket("127.0.0.1", port)) {
            client.getOutputStream().write(0x42);
            client.getOutputStream().flush();
        }

        if (!serverDone.await(10, TimeUnit.SECONDS)) {
            throw new RuntimeException("server thread timed out");
        }
        server.join(5_000);

        System.out.println("r:port_positive=" + (port > 0));
        System.out.println("r:received=" + Integer.toHexString(received[0] & 0xFF));
        System.out.println("REAL_NET_SOCKETS_OK 2");
    }
}
