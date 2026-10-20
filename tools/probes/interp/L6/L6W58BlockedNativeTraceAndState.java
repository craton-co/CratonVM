// Interpreter round i2 wave 58, lane L6: another thread's stack trace and
// state while that thread is blocked inside a blocking native method
// (`ServerSocket.accept`, which blocks in the static native
// `sun.nio.ch.Net.accept`)
// (docs/internal/fixed-bugs/interpreter-L6-a-thread-blocked-in-a-native-lists-no-native-and-reads-waiting-FIXED-20261023.md).
//
// HotSpot lists the native on top and reports the thread RUNNABLE: a thread
// in native code is RUNNABLE whatever the native waits for. Asked twice, the
// second time after a fresh block.
//
// Expected output (HotSpot 25.0.3, default flags, the Windows box):
//
//   0: sun.nio.ch.Net.accept native=true < sun.nio.ch.NioSocketImpl.accept state RUNNABLE
//   1: sun.nio.ch.Net.accept native=true < sun.nio.ch.NioSocketImpl.accept state RUNNABLE
//   done
//
// CratonVM (wave 58, debug build, jdk-only and --compatible):
// `0: sun.nio.ch.NioSocketImpl.accept native=false < java.net.ServerSocket.implAccept state WAITING`.
import java.net.ServerSocket;
import java.net.Socket;

public class L6W58BlockedNativeTraceAndState {
    static String top(StackTraceElement[] trace) {
        if (trace.length < 2) {
            return "(short: " + trace.length + ")";
        }
        return trace[0].getClassName() + "." + trace[0].getMethodName()
                + " native=" + trace[0].isNativeMethod()
                + " < " + trace[1].getClassName() + "." + trace[1].getMethodName();
    }

    public static void main(String[] args) throws Exception {
        ServerSocket ss = new ServerSocket(0);
        Thread t = new Thread(() -> {
            for (int k = 0; k < 2; k++) {
                try (Socket s = ss.accept()) {
                    // The main thread connects to end the first block.
                } catch (Exception e) {
                    return;
                }
            }
        }, "acceptor");
        t.setDaemon(true);
        t.start();
        for (int k = 0; k < 2; k++) {
            // Well inside `accept` by now.
            Thread.sleep(1000);
            System.out.println(k + ": " + top(t.getStackTrace()) + " state " + t.getState());
            if (k == 0) {
                new Socket("127.0.0.1", ss.getLocalPort()).close();
            }
        }
        ss.close();
        System.out.println("done");
    }
}
