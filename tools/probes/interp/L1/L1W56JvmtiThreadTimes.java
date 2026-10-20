// Interpreter round i2 wave 56, lane L1: the C JVMTI table's thread CPU-time
// functions (134-137), `GetThreadGroupChildren` (15) and `IsMethodObsolete`
// (91), against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-lacks-the-functions-and-events-agents-call-first-FIXED-20261007.md).
//
// The shim's `Agent_OnLoad` prints whether `can_get_current_thread_cpu_time`
// and `can_get_thread_cpu_time` are potential there and adds both; the native
// `facts` prints both timer infos, whether the current thread's CPU time is
// positive and advances over a busy loop, `GetThreadCpuTime` of the current,
// another live, an unstarted and a terminated thread and of an object that
// is no thread, the NULL out-parameter checks, what an env obtained in the
// live phase may hold and answers without the capability, the children of a
// thread group (one thread, one subgroup) and of a thread, and
// `IsMethodObsolete` of a method that was never redefined. Twice (the
// first-call row again).
//
// Needs the native shim tools/probes/interp/L1/L1W56JvmtiThreadTimes.c, loaded
// both as an agent and with `System.load` (for the native):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w56times.so tools/probes/interp/L1/L1W56JvmtiThreadTimes.c
//   javac -d /tmp/l1w56times tools/probes/interp/L1/L1W56JvmtiThreadTimes.java
//   java     -agentpath:/tmp/libl1w56times.so -cp /tmp/l1w56times L1W56JvmtiThreadTimes /tmp/libl1w56times.so
//   cratonvm -agentpath:/tmp/libl1w56times.so -cp /tmp/l1w56times L1W56JvmtiThreadTimes /tmp/libl1w56times.so
//
// Without the argument it prints only the usage line. The Rust port of the
// shim is tools/probes/interp/L1/rust-ports/L1W56JvmtiThreadTimes (README in
// tools/probes/interp/L1/rust-ports/).
//
// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with the Rust
// port of the C shim (same calls, same order, same output), three runs, the
// same each time:
//
//   onload: potential current=1 thread=1 add=0
//   current timer: err=0 max=-1 forward=0 backward=0 kind=31
//   thread timer: err=0 max=-1 forward=0 backward=0 kind=31
//   current time: err=0 err=0 positive=1 advances=1
//   thread time null: err=0 positive=1
//   thread time other: err=0 nonneg=1
//   thread time unstarted: err=15
//   thread time dead: err=15
//   thread time not a thread: err=10
//   null out: current=100 thread=100 timer=100
//   live env: potential current=1 thread=1 time=99 timer=99
//   group children: err=0 threads=1 groups=1
//     thread: l1w56-other
//     group: l1w56-child
//   group children of a thread: err=113
//   obsolete: err=0 value=0
//   again:
//   current timer: err=0 max=-1 forward=0 backward=0 kind=31
//   thread timer: err=0 max=-1 forward=0 backward=0 kind=31
//   current time: err=0 err=0 positive=1 advances=1
//   thread time null: err=0 positive=1
//   thread time other: err=0 nonneg=1
//   thread time unstarted: err=15
//   thread time dead: err=15
//   thread time not a thread: err=10
//   null out: current=100 thread=100 timer=100
//   live env: potential current=1 thread=1 time=99 timer=99
//   group children: err=0 threads=1 groups=1
//     thread: l1w56-other
//     group: l1w56-child
//   group children of a thread: err=113
//   obsolete: err=0 value=0
//
// CratonVM before wave 56 (code read): every slot of 15, 91 and 134-137 was
// `not_available` (98) and both capabilities were not potential
// (`potential current=0 thread=0 add=98`). Since wave 56: HotSpot's lines
// (`jvmti::native_env`, section "Thread CPU time, thread-group children,
// obsolete methods").
public class L1W56JvmtiThreadTimes {
    static native String facts(Thread other, Thread unstarted, Thread dead, ThreadGroup group,
            java.lang.reflect.Method method);

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W56JvmtiThreadTimes <path of the shim library>");
            return;
        }
        System.load(args[0]);
        ThreadGroup group = new ThreadGroup("l1w56-group");
        ThreadGroup child = new ThreadGroup(group, "l1w56-child");
        Object gate = new Object();
        Thread other = new Thread(group, () -> {
            synchronized (gate) {
                try {
                    gate.wait();
                } catch (InterruptedException e) {
                    // done
                }
            }
        }, "l1w56-other");
        other.setDaemon(true);
        other.start();
        Thread unstarted = new Thread(() -> { }, "l1w56-unstarted");
        Thread dead = new Thread(() -> { }, "l1w56-dead");
        dead.start();
        dead.join();
        java.lang.reflect.Method method = L1W56JvmtiThreadTimes.class.getDeclaredMethod("main", String[].class);
        System.out.print(facts(other, unstarted, dead, group, method));
        System.out.println("again:");
        System.out.print(facts(other, unstarted, dead, group, method));
        other.interrupt();
        other.join();
        if (child.getParent() != group) {
            System.out.println("child group lost");
        }
    }
}
