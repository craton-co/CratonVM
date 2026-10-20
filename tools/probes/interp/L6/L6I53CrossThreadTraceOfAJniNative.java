// Interpreter round i2, the i53 retirement: another thread's view of a
// thread whose JNI native spins in C, calling nothing
// (docs/internal/fixed-bugs/interpreter-L6-a-thread-suspended-in-a-running-native-lists-no-frames-FIXED-20261007.md).
//
// `worker` calls `L6I53JdiRunningNativeShapes.spin(3000)` (libl6i53jni, built
// from L6I53JdiRunningNativeShapes.c) in a loop; `main` reads its trace with
// `Thread.getStackTrace()` and its JMX `ThreadInfo` twice, printing the frames
// from the top down to the probe's `body`, whether each read came back
// promptly (a call spins ~3 s; HotSpot answers in milliseconds), and the JMX
// in-native flag. A JNI native's C code raised no native-excuse word, so a
// handshake pause waited for it to return and the thread listed itself
// where it stood then, its caller on top.
//
//   javac -d out L6I53JdiRunningNativeShapes.java L6I53CrossThreadTraceOfAJniNative.java
//   java -Dl6i53.dir=<dir of libl6i53jni.so> -cp out L6I53CrossThreadTraceOfAJniNative
//
// Expected output (HotSpot 25.0.4, default flags, the Linux host):
//
//   round 1
//   prompt: true
//   trace: spin(native) < body
//   jmx: RUNNABLE inNative=true
//   round 2
//   prompt: true
//   trace: spin(native) < body
//   jmx: RUNNABLE inNative=true
//   done
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;

public class L6I53CrossThreadTraceOfAJniNative {
    static volatile boolean inCall;
    static volatile boolean stop;

    static void body() {
        while (!stop) {
            inCall = true;
            L6I53JdiRunningNativeShapes.spin(3000);
        }
    }

    static String trace(Thread t) {
        StackTraceElement[] st = t.getStackTrace();
        StringBuilder sb = new StringBuilder("trace:");
        int jdk = 0;
        boolean first = true;
        for (StackTraceElement e : st) {
            String c = e.getClassName();
            if (!first && (c.startsWith("java.") || c.startsWith("jdk."))) {
                jdk++;
                continue;
            }
            if (jdk > 0) {
                sb.append(" < ").append(jdk).append(" JDK");
                jdk = 0;
            }
            sb.append(first ? " " : " < ").append(e.getMethodName());
            if (e.isNativeMethod()) {
                sb.append("(native)");
            }
            first = false;
            if (e.getMethodName().equals("body")) {
                break;
            }
        }
        return sb.toString();
    }

    public static void main(String[] args) throws Exception {
        // By absolute path, from -Dl6i53.dir (default: the working directory):
        // `jdk-only` drops an explicit -Djava.library.path (filed separately).
        System.load(new java.io.File(System.getProperty("l6i53.dir", "."),
                System.mapLibraryName("l6i53jni")).getAbsolutePath());
        Thread worker = new Thread(L6I53CrossThreadTraceOfAJniNative::body, "worker");
        worker.setDaemon(true);
        worker.start();
        while (!inCall) {
            Thread.sleep(1);
        }
        for (int round = 1; round <= 2; round++) {
            Thread.sleep(300);
            System.out.println("round " + round);
            long start = System.nanoTime();
            String trace = trace(worker);
            ThreadInfo info = ManagementFactory.getThreadMXBean().getThreadInfo(worker.getId());
            System.out.println("prompt: " + ((System.nanoTime() - start) / 1_000_000 < 1500));
            System.out.println(trace);
            System.out.println("jmx: " + info.getThreadState() + " inNative=" + info.isInNative());
        }
        stop = true;
        System.out.println("done");
    }
}
