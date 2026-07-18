package cratonvm;

import java.io.InputStream;
import java.net.Socket;
import java.net.SocketTimeoutException;

/**
 * A peer that stays connected but sends no bytes must make every
 * SocketInputStream.read overload throw SocketTimeoutException after
 * Socket.setSoTimeout, never report EOF (-1) or a zero-byte read.
 */
public final class SocketInputStreamTimeout {

    private static final int READ_TIMEOUT_MILLIS = 100;

    private SocketInputStreamTimeout() {
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            throw new IllegalArgumentException("expected the externally hosted peer port");
        }
        int port = Integer.parseInt(args[0]);
        expectTimeout(port, "read()");
        expectTimeout(port, "read(byte[], off, len)");
        expectTimeout(port, "read(byte[])");
        System.out.println("SOCKET_INPUT_STREAM_TIMEOUT_OK");
    }

    private static void expectTimeout(int port, String overload) throws Exception {
        try (Socket socket = new Socket("127.0.0.1", port)) {
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
    }
}
