package cratonvm;

import java.io.IOException;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import javax.net.ssl.SSLSocketFactory;

/**
 * Regression probe for every abstract SocketFactory overload redeclared by
 * SSLSocketFactory. A refused loopback connection must produce IOException,
 * never an AbstractMethodError from the abstract declaration.
 */
public final class SslSocketFactoryInetAddress {

    @FunctionalInterface
    private interface SocketCall {
        Socket call() throws IOException;
    }

    private static void expectIOException(String name, SocketCall call) throws Exception {
        try {
            Socket socket = call.call();
            if (socket != null) {
                socket.close();
            }
            throw new AssertionError(name + " unexpectedly connected");
        }
        catch (AbstractMethodError error) {
            throw new AssertionError(name + " resolved to an abstract SocketFactory method", error);
        }
        catch (IOException expected) {
            System.out.println("r:" + name + "=ioexception");
        }
    }

    public static void main(String[] args) throws Exception {
        int closedPort;
        try (ServerSocket reservation = new ServerSocket(0)) {
            closedPort = reservation.getLocalPort();
        }
        InetAddress loopback = InetAddress.getLoopbackAddress();
        SSLSocketFactory factory = (SSLSocketFactory) SSLSocketFactory.getDefault();

        expectIOException("inet", () -> factory.createSocket(loopback, closedPort));
        expectIOException("string-local", () -> factory.createSocket("127.0.0.1", closedPort, loopback, 0));
        expectIOException("inet-local", () -> factory.createSocket(loopback, closedPort, loopback, 0));
        System.out.println("SSL_SOCKET_FACTORY_INET_ADDRESS_OK");
    }
}
