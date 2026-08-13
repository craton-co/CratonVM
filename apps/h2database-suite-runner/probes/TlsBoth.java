import java.net.ServerSocket;
import java.net.Socket;

/**
 * H2's own {@code NetUtils} server and client in ONE process, timestamped.
 *
 * The shape of {@code TcpServer.isRunning()}, but writing a byte so the TLS
 * handshake actually runs. H2 supplies the certificate (a DSA key baked into
 * {@code org.h2.security.CipherFactory}), which both VMs reject -- only the
 * report differs, and that is the point of the probe.
 *
 * This one-process arm is what exposed the connect/handshake defect fixed on
 * 2026-08-10 (`fixed-suite-bugs/h2-suite-bugs/bug-cratonvm-tls-client-handshake-reported-as-interrupted-...`):
 * {@code SSLSocket.connect()} opened a TCP connection, DROPPED it, and let the
 * deferred handshake dial a second time. A server that accepts one connection
 * per client accepts the dead first one (whose handshake reads EOF) and is no
 * longer in {@code accept()} when the real one arrives, so the client waits out
 * its full 30 s socket timeout.
 *
 * Before the fix: server failed at 629 ms, client at 30 980 ms with
 * {@code IOException: TLS handshake failed: the handshake process was interrupted}.
 * After: client at 605 ms with {@code SSLHandshakeException} naming the
 * certificate error, and the server sees alert 42. HotSpot 25: 1055 ms,
 * {@code SSLHandshakeException} (PKIX).
 *
 * Neither leg is slow on its own -- split it into a separate server process and
 * client process (or run one leg against HotSpot) and every arm answers in a
 * few hundred milliseconds. Only the one-process arm shows the defect.
 *
 *   cd apps/h2database/h2
 *   CP="target/classes:target/test-classes:$(cat craton-testcp.txt)"
 *   javac -cp "$CP" -d . TlsBoth.java
 *   java -cp "$CP:." TlsBoth 9431
 *   <cratonvm> --java-home $JDK25 --nojit -c "$CP:." TlsBoth 9433
 */
public class TlsBoth {
    static long t0 = System.nanoTime();

    static String ts() {
        return "[" + (System.nanoTime() - t0) / 1000000L + "ms] ";
    }

    public static void main(String[] a) throws Exception {
        int port = Integer.parseInt(a[0]);
        final ServerSocket ss = org.h2.util.NetUtils.createServerSocket(port, true);
        System.out.println(ts() + "server socket created");
        Thread t = new Thread(() -> {
            try (Socket s = ss.accept()) {
                System.out.println(ts() + "SERVER accepted");
                int b = s.getInputStream().read();
                System.out.println(ts() + "SERVER read " + b);
            } catch (Throwable e) {
                System.out.println(ts() + "SERVER: " + e.getClass().getName() + ": " + e.getMessage());
            }
        });
        t.setDaemon(true);
        t.start();
        Thread.sleep(500);
        System.out.println(ts() + "client connecting");
        try (Socket c = org.h2.util.NetUtils.createLoopbackSocket(port, true)) {
            c.getOutputStream().write(7);
            System.out.println(ts() + "CLIENT: wrote byte");
        } catch (Throwable e) {
            System.out.println(ts() + "CLIENT: " + e.getClass().getName() + ": " + e.getMessage());
        }
        Thread.sleep(1500);
        System.out.println(ts() + "done");
        System.exit(0);
    }
}
