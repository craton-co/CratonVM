// Interpreter round i2 wave 61, lane L6: the C JVMTI table's `GetThreadState`
// (17) of a thread RUNNING a JDK `native` method outside any blocking region
// (docs/internal/fixed-bugs/interpreter-L6-jvmti-thread-state-misses-a-running-native-FIXED-20261025.md).
//
// Each row prints the whole `jvmtiThreadState` word of one thread:
//
// * `deflate`: a thread compressing 16 MiB at level 9 in one
//   `Deflater.deflate` call, in a loop (the `deflateBytesBytes` native runs
//   for seconds, in C, never blocking);
// * `fill`: a thread filling a 16M-int array with `Arrays.fill` in a loop (a
//   Java method: CratonVM may serve it from a registered native, which must
//   not read as in native);
// * `running`: a thread spinning in Java (the control).
//
// Two rounds (the first-call rows again).
//
// Needs the native shim tools/probes/interp/L6/L6W61JvmtiRunningNativeState.c,
// loaded both as an agent and with `System.load` (for the natives):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl6w61state.so tools/probes/interp/L6/L6W61JvmtiRunningNativeState.c
//   javac -d /tmp/l6w61state tools/probes/interp/L6/L6W61JvmtiRunningNativeState.java
//   java     -agentpath:/tmp/libl6w61state.so -cp /tmp/l6w61state L6W61JvmtiRunningNativeState /tmp/libl6w61state.so
//   cratonvm -agentpath:/tmp/libl6w61state.so -cp /tmp/l6w61state L6W61JvmtiRunningNativeState /tmp/libl6w61state.so
//
// Without the argument it prints only the usage line. The Rust port of the
// shim is tools/probes/interp/L6/rust-ports/L6W61JvmtiRunningNativeState
// (built and run as the ports of tools/probes/interp/L1/rust-ports/README.md).
//
// Expected stdout (HotSpot 25.0.3, default flags): MEASURED on the Windows box
// with the Rust port of the C shim, three runs, the same each time:
//
//   round 1
//   deflate: err=0 state=0x400005
//   fill: err=0 state=0x5
//   running: err=0 state=0x5
//   round 2
//   deflate: err=0 state=0x400005
//   fill: err=0 state=0x5
//   running: err=0 state=0x5
//   done
//
// (ALIVE 0x1, RUNNABLE 0x4, IN_NATIVE 0x400000.)
//
// CratonVM before wave 61 (code read: `ThreadRegistry::thread_in_native`
// never counts a funnel native running outside a blocking region): `deflate`
// reads `0x5`.
import java.util.Arrays;
import java.util.zip.Deflater;

public class L6W61JvmtiRunningNativeState {
    static native String state(String label, Thread t);

    static volatile boolean stop;
    static volatile boolean inCall;

    static void deflate() {
        // 1 MiB of pseudo-random letters, copied 16 times (each copy lies
        // beyond deflate's 32 KiB window, so it compresses as slowly as the
        // first); generated quickly even by an interpreter.
        byte[] in = new byte[16 << 20];
        long seed = 12345;
        for (int i = 0; i < (1 << 20); i++) {
            seed = seed * 6364136223846793005L + 1442695040888963407L;
            in[i] = (byte) ('a' + (int) ((seed >>> 33) & 7));
        }
        for (int k = 1; k < 16; k++) {
            System.arraycopy(in, 0, in, k << 20, 1 << 20);
        }
        byte[] out = new byte[in.length + (1 << 20)];
        Deflater d = new Deflater(Deflater.BEST_COMPRESSION);
        while (!stop) {
            d.reset();
            d.setInput(in);
            d.finish();
            inCall = true;
            d.deflate(out);
        }
        d.end();
    }

    static void fill() {
        int[] a = new int[16 << 20];
        int v = 0;
        while (!stop) {
            inCall = true;
            Arrays.fill(a, v++);
        }
    }

    static void spin() {
        long x = 0;
        while (!stop) {
            inCall = true;
            x++;
        }
        inCall = x < 0;
    }

    static void row(String label, Runnable body, long n) throws InterruptedException {
        stop = false;
        inCall = false;
        Thread t = new Thread(body, "l6w61-" + label + "-" + n);
        t.setDaemon(true);
        t.start();
        long deadline = System.nanoTime() + 120_000_000_000L;
        while (!inCall) {
            if (System.nanoTime() > deadline) {
                System.out.println("timeout waiting for " + label);
                break;
            }
            Thread.sleep(1);
        }
        Thread.sleep(300);
        System.out.print(state(label, t));
        stop = true;
        t.join();
    }

    static void round(long n) throws InterruptedException {
        System.out.println("round " + n);
        row("deflate", L6W61JvmtiRunningNativeState::deflate, n);
        row("fill", L6W61JvmtiRunningNativeState::fill, n);
        row("running", L6W61JvmtiRunningNativeState::spin, n);
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L6W61JvmtiRunningNativeState <path of the shim library>");
            return;
        }
        System.load(args[0]);
        round(1);
        round(2);
        System.out.println("done");
    }
}
