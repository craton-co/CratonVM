// Interpreter round i4 wave 98, lane L1
// (docs/internal/fixed-bugs/interpreter-i98-L1-reference-handler-reads-in-native-between-ticks-FIXED-20261130.md):
// the `Reference Handler` thread of an IDLE VM, sampled through JMX. HotSpot's
// handler sits in `JVM_WaitForReferencePendingList` until a collection
// discovers references, so with no allocation in the sampling window it never
// reads in native. (While collections run it briefly passes through its
// native wrapper, `_thread_in_native`; whether a sampler catches that is a
// race on HotSpot too -- w104 true, w105 false -- so this probe samples only
// idle windows.) The pre-wave-98 CratonVM left its wait every 100 ms even
// when idle and could read in-native (the w102 `L1W66UnrecordedWaitKinds`
// flake).
//
// The sampling allocates only a few ThreadInfo objects per window (60 in
// all), far too little to trigger a collection on either VM, and each window
// starts with a 200 ms settle sleep.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// Reference Handler: present
// idle in-native-seen=false
// done
// HOTSPOT_EXPECTED_END
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;

public class L1W98ReferenceHandlerSampled {
    public static void main(String[] args) throws Exception {
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        Thread handler = null;
        for (Thread t : Thread.getAllStackTraces().keySet()) {
            if (t.getName().equals("Reference Handler")) {
                handler = t;
            }
        }
        if (handler == null) {
            System.out.println("Reference Handler: absent");
            System.out.println("done");
            return;
        }
        System.out.println("Reference Handler: present");
        long id = handler.threadId();
        boolean inNative = false;
        // 12 idle windows of 250 ms (3 s): settle, then 5 spaced samples.
        // Pre-wave-98 CratonVM cycled every 100 ms, so these windows straddle
        // ~30 of its cycles; HotSpot (and CratonVM now) has none while idle.
        for (int w = 0; w < 12; w++) {
            Thread.sleep(200);
            for (int i = 0; i < 5; i++) {
                ThreadInfo info = mx.getThreadInfo(id);
                if (info != null && info.isInNative()) {
                    inNative = true;
                }
                Thread.sleep(10);
            }
        }
        System.out.println("idle in-native-seen=" + inNative);
        System.out.println("done");
    }
}
