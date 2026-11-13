// Interpreter round i5 wave 102, lane L1: JVMTI's thread lists hold live
// PLATFORM threads only, in attach order.
//
// JDK 21+ JVMTI: `GetAllThreads`, `GetAllStackTraces` and
// `GetThreadGroupChildren` return live platform threads; virtual threads are
// not returned (HotSpot walks its threads list, where a virtual thread is
// never an entry; new threads are appended, so the list keeps attach order).
// CratonVM registers each virtual thread as a registry entry of its own whose
// mirror is the `VirtualThread`, so all three listed every live virtual
// thread, and in the registry map's hash order. JDWP's
// `ThreadGroupReference.Children` (`L1W102JdiGroupChildren`) had the same
// defect. Fixed in vm/src/jvmti/native_env.rs (+ the registry's
// `alive_thread_objects_in_id_order`), not built or run.
//
// Needs the native shim tools/probes/interp/L1/L1W102JvmtiPlatformThreadsOnly.c,
// loaded both as an agent and with `System.load`:
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w102threads.so tools/probes/interp/L1/L1W102JvmtiPlatformThreadsOnly.c
//   javac -d /tmp/l1w102threads tools/probes/interp/L1/L1W102JvmtiPlatformThreadsOnly.java
//   java     -agentpath:/tmp/libl1w102threads.so -cp /tmp/l1w102threads L1W102JvmtiPlatformThreadsOnly /tmp/libl1w102threads.so
//   cratonvm -agentpath:/tmp/libl1w102threads.so -cp /tmp/l1w102threads L1W102JvmtiPlatformThreadsOnly /tmp/libl1w102threads.so
//
// (Windows: cl /LD with the JDK's include and include\win32, as the other
// L1 JVMTI shims.) Without the argument it prints only the usage line.
//
// Expected stdout (HotSpot 25; derived from the JVMTI specification and
// jvmtiEnvBase.cpp `get_live_threads`, NOT measured -- run the HotSpot line
// once and correct this block if anything differs):
//
//   GetAllThreads err=0 virtual=0 listed=p-alpha,p-beta,p-gamma
//   GetAllStackTraces err=0 virtual=0 listed=p-alpha,p-beta,p-gamma
//   GetThreadGroupChildren(main) err=0 virtual=0 listed=p-alpha,p-beta,p-gamma
//   GetThreadGroupChildren(VirtualThreads) err=0 threads=0 virtual=0
//   done
import java.util.concurrent.CountDownLatch;

public class L1W102JvmtiPlatformThreadsOnly {
    static native String rows(ThreadGroup platformGroup, ThreadGroup virtualGroup);

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W102JvmtiPlatformThreadsOnly <path of the shim>");
            return;
        }
        System.load(args[0]);
        CountDownLatch release = new CountDownLatch(1);
        CountDownLatch started = new CountDownLatch(6);
        Runnable hold = () -> {
            started.countDown();
            try {
                release.await();
            } catch (InterruptedException ignored) {
                // Ends the thread.
            }
        };
        Thread[] all = new Thread[6];
        String[] platform = {"p-alpha", "p-beta", "p-gamma"};
        for (int i = 0; i < 3; i++) {
            all[i] = new Thread(hold, platform[i]);
            all[i].setDaemon(true);
            all[i].start();
            // Attach order is start order: let each one register first.
            Thread.sleep(20);
        }
        for (int i = 0; i < 3; i++) {
            all[3 + i] = Thread.ofVirtual().name("v-" + i).start(hold);
        }
        started.await();
        ThreadGroup virtualGroup = all[3].getThreadGroup();
        System.out.print(rows(Thread.currentThread().getThreadGroup(), virtualGroup));
        release.countDown();
        for (Thread t : all) {
            t.join();
        }
        System.out.println("done");
    }
}
