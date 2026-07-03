package cratonvm;

import java.io.InputStream;
import java.io.OutputStream;
import java.net.ServerSocket;
import java.net.Socket;

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
        int received;

        try (ServerSocket ss = new ServerSocket(0)) {
            ss.setSoTimeout(10_000);
            int port = ss.getLocalPort();

            try (Socket client = new Socket("127.0.0.1", port);
                 Socket conn = ss.accept()) {
                conn.setSoTimeout(10_000);

                OutputStream out = client.getOutputStream();
                out.write(0x42);
                out.flush();

                InputStream in = conn.getInputStream();
                received = in.read();
            }

            System.out.println("r:port_positive=" + (port > 0));
        }
        System.out.println("r:received=" + Integer.toHexString(received & 0xFF));
        System.out.println("REAL_NET_SOCKETS_OK 2");
    }
}
