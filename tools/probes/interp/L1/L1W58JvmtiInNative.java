// Interpreter round i2 wave 58, lane L1: the C JVMTI table's `GetThreadState`
// (17) of a thread running a JNI native method, against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-jvmti-thread-state-lacks-in-native-FIXED-20261022.md).
//
// Each row prints the whole `jvmtiThreadState` word of one thread:
//
// * `spin`: a thread inside the shim's native `spin`, which loops in C on a
//   flag (no JNI call, no blocking) until `main` calls `release` (first, and
//   again last);
// * `upcall`: a thread inside the shim's native `upcall`, which calls a
//   `Runnable` that spins in Java (Java above a native);
// * `self(null)` / `self(thread)`: `main` asks for itself from inside the
//   native `state`, by NULL and by its own `Thread`;
// * `accept`: a thread blocked in `ServerSocket.accept` (a JDK native that
//   blocks in C), asked once `getState()` reads RUNNABLE and its stack shows
//   `accept` (or after two seconds); released by closing the socket;
// * `running`: a thread spinning in Java (the control).
//
// Two rounds (the first-call rows again).
//
// Needs the native shim tools/probes/interp/L1/L1W58JvmtiInNative.c, loaded
// both as an agent and with `System.load` (for the natives):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w58native.so tools/probes/interp/L1/L1W58JvmtiInNative.c
//   javac -d /tmp/l1w58native tools/probes/interp/L1/L1W58JvmtiInNative.java
//   java     -agentpath:/tmp/libl1w58native.so -cp /tmp/l1w58native L1W58JvmtiInNative /tmp/libl1w58native.so
//   cratonvm -agentpath:/tmp/libl1w58native.so -cp /tmp/l1w58native L1W58JvmtiInNative /tmp/libl1w58native.so
//
// Without the argument it prints only the usage line. The Rust port of the
// shim is tools/probes/interp/L1/rust-ports/L1W58JvmtiInNative (README in
// tools/probes/interp/L1/rust-ports/).
//
// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with the Rust
// port of the C shim (same calls, same order, same output), three runs, the
// same each time:
//
//   round 1
//   spin: err=0 state=0x400005
//   upcall: err=0 state=0x5
//   self(null): err=0 state=0x5
//   self(thread): err=0 state=0x5
//   accept: err=0 state=0x400005
//   running: err=0 state=0x5
//   spin: err=0 state=0x400005
//   round 2
//   (the seven rows again, the same)
//
// (ALIVE 0x1, RUNNABLE 0x4, IN_NATIVE 0x400000: a thread in C code, a JNI
// native's or the JDK's own blocking `accept`, is in native; Java a native
// calls is not; a thread asking for itself from a native is in the VM.)
//
// CratonVM before wave 58 (code read: `jvmti::native_env::live_thread_state`
// never set IN_NATIVE): every row `0x5`. Wave 58 sets it behind
// `CRATONVM_JVMTI_C_IN_NATIVE` for a JNI native's C code
// (`jvmti::native_env::CInNativeMark`).
import java.net.InetAddress;
import java.net.ServerSocket;

public class L1W58JvmtiInNative {
    static native String state(String label, Thread t);

    static native String stateOfSelf(String label);

    static native void spin();

    static native boolean spinning();

    static native void release();

    static native void upcall(Runnable r);

    static volatile boolean javaSpin;
    static volatile boolean inUpcall;

    static void awaitRunnable(Thread t) throws InterruptedException {
        long deadline = System.nanoTime() + 10_000_000_000L;
        while (t.getState() != Thread.State.RUNNABLE) {
            if (System.nanoTime() > deadline) {
                System.out.println("timeout waiting for RUNNABLE (is " + t.getState() + ")");
                return;
            }
            Thread.sleep(1);
        }
    }

    static void spinRow(long n) throws InterruptedException {
        Thread t = new Thread(L1W58JvmtiInNative::spin, "l1w58-spin-" + n);
        t.start();
        long deadline = System.nanoTime() + 10_000_000_000L;
        while (!spinning()) {
            if (System.nanoTime() > deadline) {
                System.out.println("timeout waiting for the native spin");
                break;
            }
            Thread.sleep(1);
        }
        System.out.print(state("spin", t));
        release();
        t.join();
    }

    static void upcallRow(long n) throws InterruptedException {
        javaSpin = true;
        inUpcall = false;
        Thread t = new Thread(() -> upcall(() -> {
            long x = 0;
            inUpcall = true;
            while (javaSpin) {
                x++;
            }
            inUpcall = x < 0;
        }), "l1w58-upcall-" + n);
        t.start();
        long deadline = System.nanoTime() + 10_000_000_000L;
        while (!inUpcall) {
            if (System.nanoTime() > deadline) {
                System.out.println("timeout waiting for the upcall");
                break;
            }
            Thread.sleep(1);
        }
        System.out.print(state("upcall", t));
        javaSpin = false;
        t.join();
    }

    static boolean inAccept(Thread t) {
        for (StackTraceElement e : t.getStackTrace()) {
            if (e.getMethodName().contains("accept")) {
                return true;
            }
        }
        return false;
    }

    static void acceptRow(long n) throws Exception {
        ServerSocket server = new ServerSocket(0, 1, InetAddress.getLoopbackAddress());
        Thread t = new Thread(() -> {
            try {
                server.accept().close();
            } catch (Exception e) {
                // released by the close
            }
        }, "l1w58-accept-" + n);
        t.start();
        awaitRunnable(t);
        long deadline = System.nanoTime() + 2_000_000_000L;
        while (!inAccept(t) && System.nanoTime() < deadline) {
            Thread.sleep(1);
        }
        Thread.sleep(200);
        System.out.print(state("accept", t));
        server.close();
        t.join();
    }

    static void runningRow(long n) throws InterruptedException {
        javaSpin = true;
        Thread t = new Thread(() -> {
            long x = 0;
            while (javaSpin) {
                x++;
            }
            inUpcall = x < 0;
        }, "l1w58-running-" + n);
        t.start();
        awaitRunnable(t);
        System.out.print(state("running", t));
        javaSpin = false;
        t.join();
    }

    static void round(long n) throws Exception {
        System.out.println("round " + n);
        spinRow(n);
        upcallRow(n);
        System.out.print(stateOfSelf("self(null)"));
        System.out.print(state("self(thread)", Thread.currentThread()));
        acceptRow(n);
        runningRow(n);
        spinRow(n);
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W58JvmtiInNative <path of the shim library>");
            return;
        }
        System.load(args[0]);
        round(1);
        round(2);
    }
}
