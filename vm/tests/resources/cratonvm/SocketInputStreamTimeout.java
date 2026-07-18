package cratonvm;

import java.io.InputStream;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.SocketTimeoutException;

/**
 * A peer that stays connected but sends no bytes must make every
 * SocketInputStream.read overload throw SocketTimeoutException after
 * Socket.setSoTimeout, never report EOF (-1) or a zero-byte read.
 */
public final class SocketInputStreamTimeout {

    private static final int READ_TIMEOUT_MILLIS = 100;
    private static final int PEER_IDLE_MILLIS = 600;

    private SocketInputStreamTimeout() {
    }

    public static void main(String[] args) throws Exception {
        expectTimeout("read()");
        expectTimeout("read(byte[], off, len)");
        expectTimeout("read(byte[])");
        System.out.println("SOCKET_INPUT_STREAM_TIMEOUT_OK");
    }

    private static void expectTimeout(String overload) throws Exception {
        try (ServerSocket listener = new ServerSocket(0)) {
            Throwable[] serverFailure = new Throwable[1];
            Thread peer = new Thread(() -> {
                try (Socket ignored = listener.accept()) {
                    Thread.sleep(PEER_IDLE_MILLIS);
                }
                catch (Throwable ex) {
                    serverFailure[0] = ex;
                }
            }, "socket-input-stream-timeout-peer");
            peer.start();

            try (Socket socket = new Socket("127.0.0.1", listener.getLocalPort())) {
                socket.setSoTimeout(READ_TIMEOUT_MILLIS);
                InputStream input = socket.getInputStream();
                try {
                    int actual;
                    if (overload.equals("read()")) {
                        actual = input.read();
                    }
                    else if (overload.equals("read(byte[], off, len)")) {
                        actual = input.read(new byte[4], 1, 2);
                    }
                    else {
                        actual = input.read(new byte[4]);
                    }
                    throw new AssertionError(overload + " returned " + actual + " instead of timing out");
                }
                catch (SocketTimeoutException expected) {
                    // This is the Java InputStream contract for SO_TIMEOUT.
                }
            }

            peer.join(PEER_IDLE_MILLIS + 2_000L);
            if (peer.isAlive()) {
                throw new AssertionError("peer did not complete for " + overload);
            }
            if (serverFailure[0] != null) {
                throw new AssertionError("peer failed for " + overload, serverFailure[0]);
            }
        }
    }
}
