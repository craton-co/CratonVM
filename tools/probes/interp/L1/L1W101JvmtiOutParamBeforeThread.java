// Interpreter round i5 wave 101, lane L1: the order of the C JVMTI table's
// argument checks -- a NULL out-parameter against a bad thread handle.
//
// HotSpot 25's generated entry (`jvmtiEnter`) pre-checks the pointer
// parameters (and a negative `depth`) before the function runs; the thread
// parameter is `noconvert` (virtual-thread aware since JDK 19) and checked
// inside the function, after them. So a NULL out-parameter answers
// `JVMTI_ERROR_NULL_POINTER` (100) whatever the thread handle names. CratonVM's
// `GetThreadState` already answered so; `GetThreadInfo`,
// `GetCurrentContendedMonitor`, `GetOwnedMonitorInfo` and
// `GetOwnedMonitorStackDepthInfo` checked the thread first (10
// `INVALID_THREAD` for a String, 15 `THREAD_NOT_ALIVE` for a terminated
// thread). Fixed in vm/src/jvmti/native_env.rs, not built or run.
//
// Needs the native shim tools/probes/interp/L1/L1W101JvmtiOutParamBeforeThread.c,
// loaded both as an agent and with `System.load`:
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w101order.so tools/probes/interp/L1/L1W101JvmtiOutParamBeforeThread.c
//   javac -d /tmp/l1w101order tools/probes/interp/L1/L1W101JvmtiOutParamBeforeThread.java
//   java     -agentpath:/tmp/libl1w101order.so -cp /tmp/l1w101order L1W101JvmtiOutParamBeforeThread /tmp/libl1w101order.so
//   cratonvm -agentpath:/tmp/libl1w101order.so -cp /tmp/l1w101order L1W101JvmtiOutParamBeforeThread /tmp/libl1w101order.so
//
// Without the argument it prints only the usage line. HotSpot also prints
// its restricted-method WARNING lines for `System.load` on stderr.
//
// Expected stdout (HotSpot 25; derived from jvmtiEnter.xsl / jvmtiEnv.cpp,
// NOT measured -- the orchestrator should run the HotSpot line once and
// correct this block if anything differs). For each of not-a-thread, dead,
// live, in that order:
//
//   <who> GetThreadInfo NULL: err=100
//   <who> GetThreadState NULL: err=100
//   <who> GetCurrentContendedMonitor NULL: err=100
//   <who> GetOwnedMonitorInfo NULL: err=100
//   <who> GetOwnedMonitorStackDepthInfo NULL: err=100
//   <who> GetFrameCount NULL: err=100
//   <who> GetFrameLocation NULL: err=100
//   <who> GetFrameLocation -1: err=103
//   done
public class L1W101JvmtiOutParamBeforeThread {
    static native String rows(Object notThread, Thread dead, Thread live);

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W101JvmtiOutParamBeforeThread <path of the shim>");
            return;
        }
        System.load(args[0]);
        Thread dead = new Thread(() -> { }, "dead");
        dead.start();
        dead.join();
        Object lock = new Object();
        Thread live = new Thread(() -> {
            synchronized (lock) {
                try {
                    lock.wait();
                } catch (InterruptedException ignored) {
                    // Ends the thread.
                }
            }
        }, "live");
        live.setDaemon(true);
        live.start();
        System.out.print(rows("not a thread", dead, live));
        live.interrupt();
        live.join();
        System.out.println("done");
    }
}
