// Interpreter round i5 wave 104, lane L1: JVMTI `ThreadEnd` for `main` and
// `ThreadStart` for `DestroyJavaVM` when `main` returns while a non-daemon
// thread still runs.
//
// HotSpot's launcher detaches `main` (`JavaThread::exit` posts `ThreadEnd`)
// and attaches the same OS thread anew as `DestroyJavaVM`
// (`attach_current_thread` posts `ThreadStart`), which waits for the last
// non-daemon thread; no `ThreadEnd` follows for `DestroyJavaVM` (the VM is
// dead by then; measured: it does get one, see below). CratonVM posted neither (vm/src/vm/vm_exec.rs
// `end_main_thread_java_side`, `list_destroy_java_vm_thread`); since wave 104
// `debug::note_main_thread_ended` / `note_destroy_java_vm_attached` post both
// (`CRATONVM_DEBUG_DESTROY_JAVA_VM_IDENTITY`, default on). Not built or run.
// Page: docs/internal/fixed-bugs/interpreter-i101-L6-main-thread-stays-alive-during-destroy-java-vm-FIXED-20261016.md
//
// Needs the agent tools/probes/interp/L1/L1W104JvmtiDestroyJavaVmThreadEvents.c
// (tools/jvmti/run-jvmti-shims.sh builds and runs it; it is loaded with
// -agentpath only, the probe has no natives and ignores its argument):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w104djvm.so tools/probes/interp/L1/L1W104JvmtiDestroyJavaVmThreadEvents.c
//   javac -d /tmp/l1w104djvm tools/probes/interp/L1/L1W104JvmtiDestroyJavaVmThreadEvents.java
//   java     -agentpath:/tmp/libl1w104djvm.so -cp /tmp/l1w104djvm L1W104JvmtiDestroyJavaVmThreadEvents
//   cratonvm -agentpath:/tmp/libl1w104djvm.so -cp /tmp/l1w104djvm L1W104JvmtiDestroyJavaVmThreadEvents
//
// The agent prints a line per `ThreadStart` / `ThreadEnd` of a thread named
// `main`, `DestroyJavaVM` or `lingerer`, flushing stdout each time. `main`
// waits until `lingerer`'s `run()` began, so its `ThreadStart` comes first.
//
// Expected stdout (HotSpot 25, MEASURED on the Linux host, w104): `main`'s
// own `ThreadStart` comes right after `VMInit`, and `DestroyJavaVM` gets its
// `ThreadEnd` before `VMDeath`:
//
//   ThreadStart main
//   ThreadStart lingerer
//   main returns
//   ThreadEnd main
//   ThreadStart DestroyJavaVM
//   lingerer ends
//   ThreadEnd lingerer
//   ThreadEnd DestroyJavaVM
import java.util.concurrent.CountDownLatch;

public class L1W104JvmtiDestroyJavaVmThreadEvents {
    public static void main(String[] args) throws Exception {
        CountDownLatch running = new CountDownLatch(1);
        Thread lingerer = new Thread(() -> {
            running.countDown();
            try {
                Thread.sleep(1500);
            } catch (InterruptedException ignored) {
                // Ends early.
            }
            System.out.println("lingerer ends");
        }, "lingerer");
        lingerer.start();
        running.await();
        System.out.println("main returns");
    }
}
