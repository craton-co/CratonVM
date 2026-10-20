// The two residuals of
// docs/internal/fixed-bugs/interpreter-L6-a-blocked-synchronized-method-entrant-is-missing-from-its-stack-trace-FIXED-20261007.md
// that wave 58 left: a thread BLOCKED entering a `synchronized native`
// method, and a synchronized method entered while a C JVMTI agent listens
// for `MethodEntry`.
//
// Sections (each: a holder owns the monitor, a thread calls into it and
// blocks; the main thread waits for BLOCKED, then prints the blocked
// thread's top two frames from `Thread.getStackTrace()` and from the agent's
// `GetStackTrace`, then releases):
//
//   static native    `static synchronized native void staticNative()`
//   instance native  `synchronized native void instanceNative()`
//   entry order      `static synchronized void contended()` with the agent's
//                    `MethodEntry` enabled on that thread; the callback
//                    records, for `contended`, how many monitors the
//                    thread owns when the event is posted (HotSpot posts it
//                    after `lock_method`: 1)
//
// Needs the native shim tools/probes/interp/L6/L6K54BlockedSyncNativeAndEntryOrder.c,
// loaded both as an agent and with `System.load` (for the natives):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl6k54.so tools/probes/interp/L6/L6K54BlockedSyncNativeAndEntryOrder.c
//   javac -d /tmp/l6k54 tools/probes/interp/L6/L6K54BlockedSyncNativeAndEntryOrder.java
//   java     -agentpath:/tmp/libl6k54.so -cp /tmp/l6k54 L6K54BlockedSyncNativeAndEntryOrder /tmp/libl6k54.so
//   cratonvm -agentpath:/tmp/libl6k54.so -cp /tmp/l6k54 L6K54BlockedSyncNativeAndEntryOrder /tmp/libl6k54.so
//
// The Rust port of the shim is tools/probes/interp/L1/rust-ports/L6K54BlockedSyncNativeAndEntryOrder
// (the same calls, the same lines).
//
// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with the Rust
// port, default flags and -Xint, three runs each, the same each time:
//
//   static native:
//     blocked: true
//     java: staticNative(native) < viaStaticNative
//     jvmti: err=0 staticNative@-1 < viaStaticNative@0
//   instance native:
//     blocked: true
//     java: instanceNative(native) < viaInstanceNative
//     jvmti: err=0 instanceNative@-1 < viaInstanceNative@1
//   entry order:
//     blocked: true
//     java: contended < viaContended
//     jvmti: err=0 contended@0 < viaContended@12
//     entry: contended err=0 owned=1 entries=1
//   static native again:
//     blocked: true
//     java: staticNative(native) < viaStaticNative
//     jvmti: err=0 staticNative@-1 < viaStaticNative@0
//   counter=1
//
// CratonVM before the i54-L6 retirement (MEASURED, debug build, both modes,
// --nojit too): the `java:` rows of the natives were `viaStaticNative < run`
// / `viaInstanceNative < run`, and of `entry order` `viaContended < run` (a
// `MethodEntry` listener kept the lock-before-push order); the `jvmti:` rows
// and the `entry:` row were HotSpot's already.
public class L6K54BlockedSyncNativeAndEntryOrder {
    static native void arm(Thread t);

    static native String disarm(Thread t);

    static native String top(Thread t);

    static synchronized native void staticNative();

    synchronized native void instanceNative();

    static int counter;
    static volatile boolean go;
    static volatile boolean holding;
    static volatile boolean release;

    static synchronized void contended() {
        counter++;
    }

    static void viaStaticNative() {
        staticNative();
    }

    void viaInstanceNative() {
        instanceNative();
    }

    static void viaContended() {
        while (!go) {
            Thread.onSpinWait();
        }
        contended();
    }

    static String javaTop(Thread t) {
        StackTraceElement[] st = t.getStackTrace();
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < Math.min(2, st.length); i++) {
            if (i > 0) {
                sb.append(" < ");
            }
            sb.append(st[i].getMethodName());
            if (st[i].isNativeMethod()) {
                sb.append("(native)");
            }
        }
        return sb.toString();
    }

    static void hold(Object lock) {
        synchronized (lock) {
            holding = true;
            while (!release) {
                try {
                    Thread.sleep(2);
                } catch (InterruptedException e) {
                    return;
                }
            }
        }
    }

    static boolean awaitBlocked(Thread t) throws InterruptedException {
        long deadline = System.currentTimeMillis() + 60_000;
        while (t.getState() != Thread.State.BLOCKED) {
            if (System.currentTimeMillis() > deadline) {
                return false;
            }
            Thread.sleep(1);
        }
        return true;
    }

    static void section(String name, Object lock, Runnable body, boolean entryEvents) throws Exception {
        holding = false;
        release = false;
        go = false;
        Thread holder = new Thread(() -> hold(lock), "holder");
        holder.start();
        while (!holding) {
            Thread.sleep(1);
        }
        Thread blocked = new Thread(body, "blocked");
        blocked.start();
        if (entryEvents) {
            arm(blocked);
        }
        go = true;
        boolean ok = awaitBlocked(blocked);
        System.out.println(name + ":");
        System.out.println("  blocked: " + ok);
        System.out.println("  java: " + javaTop(blocked));
        System.out.println("  jvmti: " + top(blocked));
        release = true;
        holder.join();
        blocked.join();
        if (entryEvents) {
            System.out.println("  entry: " + disarm(blocked));
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length < 1) {
            System.out.println("usage: L6K54BlockedSyncNativeAndEntryOrder <path of the shim>");
            return;
        }
        System.load(args[0]);
        L6K54BlockedSyncNativeAndEntryOrder self = new L6K54BlockedSyncNativeAndEntryOrder();
        Object klass = L6K54BlockedSyncNativeAndEntryOrder.class;
        section("static native", klass, L6K54BlockedSyncNativeAndEntryOrder::viaStaticNative, false);
        section("instance native", self, self::viaInstanceNative, false);
        section("entry order", klass, L6K54BlockedSyncNativeAndEntryOrder::viaContended, true);
        section("static native again", klass, L6K54BlockedSyncNativeAndEntryOrder::viaStaticNative, false);
        System.out.println("counter=" + counter);
    }
}
