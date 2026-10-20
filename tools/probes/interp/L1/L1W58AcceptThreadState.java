// Interpreter round i2 wave 58, lane L1: `Thread.getState()` of a platform
// thread blocked in a JDK blocking native (`ServerSocket.accept`, a
// `Socket` read with no data), against HotSpot; found by the C JVMTI table's
// `GetThreadState` row `accept` of `L1W58JvmtiInNative`
// (docs/internal/fixed-bugs/interpreter-L6-a-thread-blocked-in-a-native-lists-no-native-and-reads-waiting-FIXED-20261023.md).
//
// Each row starts a thread that blocks, waits until its stack shows the
// blocking method (or two seconds), waits 200 ms more, prints its state and
// whether it shows the blocking method, then releases it by closing the
// socket. Two rounds (the first-call rows again).
//
// Expected stdout (HotSpot 25.0.3, default flags; MEASURED on the Windows
// box, three runs, the same each time):
//
//   round 1
//   accept: RUNNABLE
//   read: RUNNABLE
//   round 2
//   accept: RUNNABLE
//   read: RUNNABLE
//
// CratonVM (MEASURED, debug build, wave 58, both modes, through the C table's
// `GetThreadState`: `0x291`, WAITING | PARKED): `accept: WAITING` -- the
// blocking socket call parks the thread.
import java.io.InputStream;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;

public class L1W58AcceptThreadState {
    interface Blocker {
        void run() throws Exception;
    }

    static void row(String label, String method, Blocker blocker, AutoCloseable release) throws Exception {
        Thread t = new Thread(() -> {
            try {
                blocker.run();
            } catch (Exception e) {
                // released by the close
            }
        }, "l1w58-" + label);
        t.start();
        long deadline = System.nanoTime() + 2_000_000_000L;
        while (System.nanoTime() < deadline) {
            boolean in = false;
            for (StackTraceElement e : t.getStackTrace()) {
                if (e.getMethodName().contains(method)) {
                    in = true;
                }
            }
            if (in) {
                break;
            }
            Thread.sleep(1);
        }
        Thread.sleep(200);
        System.out.println(label + ": " + t.getState());
        release.close();
        t.join();
    }

    static void round(long n) throws Exception {
        System.out.println("round " + n);
        ServerSocket server = new ServerSocket(0, 1, InetAddress.getLoopbackAddress());
        row("accept", "accept", () -> server.accept().close(), server);
        ServerSocket listener = new ServerSocket(0, 1, InetAddress.getLoopbackAddress());
        Socket client = new Socket(InetAddress.getLoopbackAddress(), listener.getLocalPort());
        Socket peer = listener.accept();
        InputStream in = client.getInputStream();
        row("read", "read", () -> in.read(), client);
        peer.close();
        listener.close();
    }

    public static void main(String[] args) throws Exception {
        round(1);
        round(2);
    }
}
