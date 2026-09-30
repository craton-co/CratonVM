import java.net.InetSocketAddress;
import java.net.ServerSocket;

/** Regression probe for pre-bind ServerSocket option configuration. */
public final class ServerSocketReceiveBufferProbe {
    public static void main(String[] args) throws Exception {
        try (ServerSocket socket = new ServerSocket()) {
            socket.setReceiveBufferSize(1024);
            if (socket.getReceiveBufferSize() <= 0) {
                throw new AssertionError("ServerSocket receive-buffer getter returned no usable value");
            }
            socket.bind(new InetSocketAddress(0));
            if (!socket.isBound() || socket.getLocalPort() <= 0) {
                throw new AssertionError("ServerSocket did not bind after pre-bind configuration");
            }
            System.out.println("PASS port=" + socket.getLocalPort());
        }
    }
}
