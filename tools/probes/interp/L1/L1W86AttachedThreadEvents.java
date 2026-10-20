// Interpreter round i4 wave 86, lane L1: a C JVMTI agent's ThreadStart and
// ThreadEnd for a thread that attaches through JNI AttachCurrentThread and
// detaches (item 5 of
// docs/internal/fixed-bugs/interpreter-i84-L1-jdwp-event-set-order-and-instance-filters-FIXED-20261119.md).
//
// Needs the shim tools/probes/interp/L1/L1W86AttachedThreadEvents.c, loaded
// as an agent and with System.load:
//   gcc -shared -fPIC -pthread -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w86attach.so tools/probes/interp/L1/L1W86AttachedThreadEvents.c
//   (Windows: cl /LD /I"%JAVA_HOME%\include" /I"%JAVA_HOME%\include\win32" ... /Fe:l1w86attach.dll)
//   javac -d /tmp/l1w86attach tools/probes/interp/L1/L1W86AttachedThreadEvents.java
//   java     -agentpath:/tmp/libl1w86attach.so -cp /tmp/l1w86attach L1W86AttachedThreadEvents /tmp/libl1w86attach.so
//   cratonvm -agentpath:/tmp/libl1w86attach.so -cp /tmp/l1w86attach L1W86AttachedThreadEvents /tmp/libl1w86attach.so
//   cratonvm --compatible -agentpath:/tmp/libl1w86attach.so -cp /tmp/l1w86attach L1W86AttachedThreadEvents /tmp/libl1w86attach.so
// (a cratonvm built with --features experimental-debug)
//
// HOTSPOT_EXPECTED_BEGIN
// attached thread: ThreadStart=1 ThreadEnd=1
// HOTSPOT_EXPECTED_END
//
// Derived from the HotSpot sources (jni.cpp attach_current_thread posts
// JvmtiExport::post_thread_start; JavaThread::exit posts post_thread_end),
// not measured. Before wave 86 CratonVM printed ThreadStart=0 ThreadEnd=0.
public class L1W86AttachedThreadEvents {
    static native String run();

    public static void main(String[] args) {
        if (args.length != 1) {
            System.out.println("usage: L1W86AttachedThreadEvents <path of the shim library>");
            return;
        }
        System.load(args[0]);
        System.out.println(run());
    }
}
