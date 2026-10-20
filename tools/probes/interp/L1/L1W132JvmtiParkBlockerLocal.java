// Interpreter round i6 wave 132, lane L1: the C JVMTI table's GetLocalObject of
// `LockSupport.park(Object)`'s `blocker` (slot 0) on a SUSPENDED platform thread
// parked there, from a C agent that holds `can_access_local_variables` and NO
// JDWP port (docs/known-issues/interpreter/i126-L1-a-blocked-threads-dead-reference-locals-read-null-20261227.md).
// The blocker is dead at the `Unsafe.park` call by liveness; HotSpot keeps every
// local alive under the capability and answers the blocker. CratonVM before wave
// 132 rooted dead locals only under a JDWP port, so here the slot read `null`.
// Wave 132 (`CRATONVM_JVMTI_C_AGENT_KEEPS_LOCALS_ALIVE`, default on): a startup
// agent's capability makes every local a root, as a JDWP port does.
//
// The read is done twice: once right after the thread parks, once after three
// System.gc() calls and an allocation burst (the referent must still be the
// blocker, followed through any moving collection).
//
// Needs the native shim tools/probes/interp/L1/L1W132JvmtiParkBlockerLocal.c, loaded
// as an agent and with System.load. Windows (MSVC, from a vcvars64 shell):
//
//   cl /nologo /O2 /LD /I"%JAVA_HOME%\include" /I"%JAVA_HOME%\include\win32" tools\probes\interp\L1\L1W132JvmtiParkBlockerLocal.c /Fe:%TEMP%\L1W132JvmtiParkBlockerLocal.dll
//   javac -d %TEMP%\l1w132pb tools\probes\interp\L1\L1W132JvmtiParkBlockerLocal.java
//   java     -agentpath:%TEMP%\L1W132JvmtiParkBlockerLocal.dll -cp %TEMP%\l1w132pb L1W132JvmtiParkBlockerLocal %TEMP%\L1W132JvmtiParkBlockerLocal.dll
//   cratonvm [--compatible] -agentpath:%TEMP%\L1W132JvmtiParkBlockerLocal.dll -cp %TEMP%\l1w132pb L1W132JvmtiParkBlockerLocal %TEMP%\L1W132JvmtiParkBlockerLocal.dll
//
// Linux: gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" -o /tmp/libl1w132pb.so <the .c>
//
// Negative control: CRATONVM_JVMTI_C_AGENT_KEEPS_LOCALS_ALIVE=0 prints
// `GetLocalObject blocker: err=0 null` on both rows (the pre-wave-132 answer).
// HotSpot's stderr carries the System.load native-access warning; compare stdout.
//
// HOTSPOT_EXPECTED_BEGIN
// fresh SuspendThread: err=0
// fresh frame LockSupport.park(Object): found
// fresh GetLocalObject blocker: err=0 the-blocker
// fresh ResumeThread: err=0
// after-gc SuspendThread: err=0
// after-gc frame LockSupport.park(Object): found
// after-gc GetLocalObject blocker: err=0 the-blocker
// after-gc ResumeThread: err=0
// parker saw the-blocker
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.locks.LockSupport;

public class L1W132JvmtiParkBlockerLocal {
    static native String rows(Thread parker, String label);

    static volatile boolean release;
    static volatile String saw;
    static volatile Object sink;

    static void parkIn() {
        // The blocker is reachable only from this call's argument and
        // `Thread.parkBlocker` while the thread parks.
        String blocker = new StringBuilder("the-").append("blocker").toString();
        while (!release) {
            LockSupport.park(blocker);
        }
        saw = blocker;
    }

    static void awaitState(Thread t, Thread.State s) throws InterruptedException {
        long until = System.currentTimeMillis() + 60_000;
        while (t.getState() != s && System.currentTimeMillis() < until) {
            Thread.sleep(5);
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W132JvmtiParkBlockerLocal <path of the shim library>");
            return;
        }
        System.load(args[0]);
        Thread parker = new Thread(L1W132JvmtiParkBlockerLocal::parkIn, "parker");
        parker.start();
        awaitState(parker, Thread.State.WAITING);
        Thread.sleep(200);
        System.out.print(rows(parker, "fresh"));
        for (int i = 0; i < 3; i++) {
            for (int j = 0; j < 20_000; j++) {
                sink = new byte[64];
            }
            System.gc();
        }
        awaitState(parker, Thread.State.WAITING);
        System.out.print(rows(parker, "after-gc"));
        release = true;
        LockSupport.unpark(parker);
        parker.join(10_000);
        System.out.println("parker saw " + saw);
        System.out.println("done");
    }
}
