// Interpreter round i5 wave 107, lane L1: a JNI-attached thread's detach posts
// JVMTI `ThreadEnd` BEFORE a joiner of its `Thread` wakes
// (docs/internal/fixed-bugs/interpreter-i106-L1-jni-detach-posts-thread-death-after-join-FIXED-20261016.md).
//
// HotSpot's `jni_DetachCurrentThread` runs `JavaThread::exit(false,
// jni_detach)`: `Thread.exit()`, then `JvmtiExport::post_thread_end`, then
// `ensure_join` (TERMINATED, `eetop` 0, `notifyAll`). So inside the agent's
// `ThreadEnd` the thread's `isAlive()` is still true, and a Java thread in
// `join()` on it wakes only after the callback returned. CratonVM's
// `jni_detach_current_thread` ran `Thread.exit()` AND `ensure_join` first and
// posted the events after: the callback saw `alive=false` and `joined` was
// printed inside the callback's 300 ms sleep. Fixed in wave 107
// (vm/src/native/jni.rs `jni_detach_current_thread`, vm/src/vm/vm_exec.rs
// `exit_attached_thread_java_side` / `join_attached_thread_java_side`); not
// built or run.
//
// Needs the shim tools/probes/interp/L1/L1W107JvmtiDetachThreadEndBeforeJoin.c,
// loaded as an agent and with System.load; tools/jvmti/run-jvmti-shims.sh
// runs it (a default scenario since wave 107). By hand:
//   gcc -shared -fPIC -pthread -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w107detach.so tools/probes/interp/L1/L1W107JvmtiDetachThreadEndBeforeJoin.c
//   javac -d /tmp/l1w107detach tools/probes/interp/L1/L1W107JvmtiDetachThreadEndBeforeJoin.java
//   java     -agentpath:/tmp/libl1w107detach.so -cp /tmp/l1w107detach L1W107JvmtiDetachThreadEndBeforeJoin /tmp/libl1w107detach.so
//   cratonvm -agentpath:/tmp/libl1w107detach.so -cp /tmp/l1w107detach L1W107JvmtiDetachThreadEndBeforeJoin /tmp/libl1w107detach.so
// (a cratonvm built with --features cratonvm-vm/experimental-debug)
//
// HOTSPOT_EXPECTED_BEGIN
// ThreadStart l1w107-attached
// callback on l1w107-attached
// ThreadEnd l1w107-attached alive=true
// ThreadEnd l1w107-attached returns
// joined alive=false
// attached OS thread ended
// HOTSPOT_EXPECTED_END
//
// Derived from jni.cpp / javaThread.cpp (JDK 25), not measured. Before the
// fix CratonVM printed `alive=false` and `joined alive=false` between the two
// `ThreadEnd` lines.
import java.util.concurrent.CountDownLatch;

public class L1W107JvmtiDetachThreadEndBeforeJoin {
    static native void say(String line);

    static native boolean startAttached();

    static native void awaitAttached();

    static final CountDownLatch called = new CountDownLatch(1);
    static volatile Thread attached;

    static void fromNative() {
        Thread me = Thread.currentThread();
        say("callback on " + me.getName());
        attached = me;
        called.countDown();
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W107JvmtiDetachThreadEndBeforeJoin <path of the shim library>");
            return;
        }
        System.load(args[0]);
        if (!startAttached()) {
            say("no agent");
            return;
        }
        called.await();
        Thread t = attached;
        t.join();
        say("joined alive=" + t.isAlive());
        awaitAttached();
        say("attached OS thread ended");
    }
}
