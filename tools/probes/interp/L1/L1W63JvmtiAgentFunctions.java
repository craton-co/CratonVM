// The C JVMTI functions the table still answered with
// JVMTI_ERROR_NOT_AVAILABLE (98) after wave 61, against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-lacks-the-functions-and-events-agents-call-first-FIXED-20261007.md,
// "Next step (from wave 61)", items 1, 2 and 5): the capabilities that go
// with them, the extension mechanism (124-126) and HotSpot's three extension
// functions, `RunAgentThread` (12), `InterruptThread` / `StopThread` (8 / 7),
// `GetObjectMonitorUsage` (59), `GetSourceDebugExtension` (90),
// `GetConstantPool` (146), `SetNativeMethodPrefix(es)` (73 / 74), the module
// functions (3, 40, 94-99), `SetVerboseFlag` (150),
// `Get/SetJNIFunctionTable` (120 / 121), `Suspend/ResumeAllVirtualThreads`
// (118 / 119) and `SetHeapSamplingInterval` (156). The tags and the heap
// walks are `L1W63JvmtiTagsAndHeap`, the events `L1W63JvmtiEvents`.
//
// The startup agent adds every capability it is offered in `Agent_OnLoad`
// and does all the calls; `caps live` is what an env obtained in the live
// phase is offered after that.
//
// Needs the native shim tools/probes/interp/L1/L1W63JvmtiAgentFunctions.c,
// loaded both as an agent and with `System.load` (for the natives):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w63fn.so tools/probes/interp/L1/L1W63JvmtiAgentFunctions.c
//   javac -d /tmp/l1w63fn tools/probes/interp/L1/L1W63JvmtiAgentFunctions.java
//   java     -agentpath:/tmp/libl1w63fn.so -cp /tmp/l1w63fn L1W63JvmtiAgentFunctions /tmp/libl1w63fn.so
//   cratonvm -agentpath:/tmp/libl1w63fn.so -cp /tmp/l1w63fn L1W63JvmtiAgentFunctions /tmp/libl1w63fn.so
//
// The Rust port of the shim is
// tools/probes/interp/L1/rust-ports/L1W63JvmtiAgentFunctions.
//
// Expected stdout (HotSpot 25.0.3): see the block at the end of this file.
import java.io.ByteArrayOutputStream;
import java.io.DataInputStream;
import java.io.InputStream;
import java.lang.invoke.MethodHandles;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.locks.LockSupport;

public class L1W63JvmtiAgentFunctions {
    /** What `Agent_OnLoad` recorded. */
    static native String onload();

    /** The capability rows of the live phase. */
    static native String caps();

    /** The extension functions and events, and calls of the functions. */
    static native String extensions(Thread platform, Thread parkedVirtual);

    /** `GetCarrierThread` / `GetVirtualThread` asked by a mounted virtual
     *  thread about itself and its carrier. */
    static native String mounted();

    /** `SetVerboseFlag(flag, value)`'s error. */
    static native int verbose(int flag, boolean value);

    /** `GetJNIFunctionTable` / `SetJNIFunctionTable` rows. */
    static native String jniTable();

    /** `GetVersion` through this native's `JNIEnv`, and the count of calls
     *  the replaced table saw. */
    static native String jniVersion();

    /** `GetSourceDebugExtension`: `=text`, or `!err`. */
    static native String sourceDebugExtension(Class<?> c);

    /** `GetConstantPool`: `err count bytes` with the bytes in hex. */
    static native String constantPool(Class<?> c);

    /** `GetObjectMonitorUsage`: the owner, the entry count, the waiters. */
    static native String monitorUsage(Object o);

    /** `InterruptThread(t)`'s error. */
    static native int interrupt(Thread t);

    /** `StopThread(t, e)`'s error. */
    static native int stop(Thread t, Throwable e);

    /** `RunAgentThread(t, proc, arg, priority)`, then what the proc saw. */
    static native String runAgentThread(Thread t, int priority);

    /** The `RunAgentThread` argument errors. */
    static native String runAgentThreadErrors(Thread started, Thread fresh);

    /** The module functions' rows. */
    static native String modules(Module base, Module logging, Module unnamed, Class<?> service, Class<?> impl, Object notModule);

    /** `SuspendAllVirtualThreads` / `ResumeAllVirtualThreads` rows. */
    static native String suspendAllVirtual(Thread[] vts, Thread except);

    /** `SetHeapSamplingInterval(n)`'s error. */
    static native int sampling(int interval);

    /** `SetNativeMethodPrefix(prefix)`'s error (NULL for a null String). */
    static native int prefix(String prefix);

    /** `SetNativeMethodPrefixes` with these prefixes. */
    static native int prefixes(String[] prefixes);

    /** A native whose name carries the prefix the shim registers; the shim
     *  exports only `Java_L1W63JvmtiAgentFunctions_twice`. */
    static native int $$L1W63$$twice(int x);

    static int twice(int x) {
        return $$L1W63$$twice(x);
    }

    /** The same, called before the prefix is registered. */
    static native int $$L1W63$$thrice(int x);

    static int thrice(int x) {
        return $$L1W63$$thrice(x);
    }

    // ---------------------------------------------------------------------

    static void park(Thread t) throws InterruptedException {
        for (int i = 0; i < 500 && t.getState() != Thread.State.WAITING
                && t.getState() != Thread.State.TIMED_WAITING; i++) {
            Thread.sleep(10);
        }
    }

    static void blocked(Thread t) throws InterruptedException {
        for (int i = 0; i < 500 && t.getState() != Thread.State.BLOCKED; i++) {
            Thread.sleep(10);
        }
    }

    static void extensionRows() throws Exception {
        Object gate = new Object();
        Thread vt = Thread.ofVirtual().name("l1w63-parked").unstarted(() -> LockSupport.park());
        vt.start();
        park(vt);
        System.out.print(extensions(Thread.currentThread(), vt));
        LockSupport.unpark(vt);
        vt.join();
        String[] out = new String[1];
        Thread mountedVt = Thread.ofVirtual().name("l1w63-mounted").start(() -> out[0] = mounted());
        mountedVt.join();
        System.out.print(out[0]);
    }

    static void verboseRows() {
        System.out.println("verbose other false: err=" + verbose(0, false));
        System.out.println("verbose jni false: err=" + verbose(4, false));
        System.out.println("verbose 3 false: err=" + verbose(3, false));
        System.out.println("verbose 8 false: err=" + verbose(8, false));
        System.out.println("verbose -1 false: err=" + verbose(-1, false));
    }

    static void jniRows() {
        System.out.print(jniTable());
        System.out.print(jniVersion());
    }

    // ---------------------------------------------------------------------
    // Class-file attributes

    public static class Plain {
        public static int value = 42;

        public String hello(String who) {
            return "hello " + who + value;
        }
    }

    static byte[] bytesOf(Class<?> c) throws Exception {
        String name = "/" + c.getName().replace('.', '/') + ".class";
        try (InputStream in = c.getResourceAsStream(name)) {
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            in.transferTo(out);
            return out.toByteArray();
        }
    }

    /** The constant pool of `bytes`: its count and its entries' bytes. */
    static Object[] constantPoolOf(byte[] bytes) throws Exception {
        DataInputStream in = new DataInputStream(new java.io.ByteArrayInputStream(bytes));
        in.readInt();
        in.readUnsignedShort();
        in.readUnsignedShort();
        int count = in.readUnsignedShort();
        int start = 10;
        int pos = start;
        for (int i = 1; i < count; i++) {
            int tag = bytes[pos] & 0xff;
            switch (tag) {
                case 1: pos += 3 + (((bytes[pos + 1] & 0xff) << 8) | (bytes[pos + 2] & 0xff)); break;
                case 3: case 4: case 9: case 10: case 11: case 12: case 17: case 18: pos += 5; break;
                case 5: case 6: pos += 9; i++; break;
                case 7: case 8: case 16: case 19: case 20: pos += 3; break;
                case 15: pos += 4; break;
                default: throw new IllegalStateException("tag " + tag);
            }
        }
        byte[] pool = java.util.Arrays.copyOfRange(bytes, start, pos);
        return new Object[] {count, pool, pos};
    }

    static String hex(byte[] b) {
        StringBuilder s = new StringBuilder();
        for (byte x : b) {
            s.append(String.format("%02x", x & 0xff));
        }
        return s.toString();
    }

    /** `bytes` with a `SourceDebugExtension` attribute appended to the
     *  class's attributes (javac never writes one); its name is added to the
     *  constant pool. */
    static byte[] withSourceDebugExtension(byte[] bytes, String text) throws Exception {
        Object[] cp = constantPoolOf(bytes);
        int count = (Integer) cp[0];
        int end = (Integer) cp[2];
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        java.io.DataOutputStream d = new java.io.DataOutputStream(out);
        d.write(bytes, 0, 8);
        d.writeShort(count + 1);
        d.write(bytes, 10, end - 10);
        byte[] name = "SourceDebugExtension".getBytes("UTF-8");
        d.writeByte(1);
        d.writeShort(name.length);
        d.write(name);
        // Rest of the class up to the class attributes: walk it.
        DataInputStream in = new DataInputStream(new java.io.ByteArrayInputStream(bytes, end, bytes.length - end));
        int fixed = 6;
        byte[] head = new byte[fixed];
        in.readFully(head);
        d.write(head);
        int interfaces = in.readUnsignedShort();
        d.writeShort(interfaces);
        for (int i = 0; i < interfaces; i++) {
            d.writeShort(in.readUnsignedShort());
        }
        for (int kind = 0; kind < 2; kind++) {
            int members = in.readUnsignedShort();
            d.writeShort(members);
            for (int i = 0; i < members; i++) {
                d.writeShort(in.readUnsignedShort());
                d.writeShort(in.readUnsignedShort());
                d.writeShort(in.readUnsignedShort());
                copyAttributes(in, d, in.readUnsignedShort());
            }
        }
        int attributes = in.readUnsignedShort();
        java.io.ByteArrayOutputStream rest = new java.io.ByteArrayOutputStream();
        copyAttributes(in, new java.io.DataOutputStream(rest), attributes);
        d.writeShort(attributes + 1);
        byte[] copied = rest.toByteArray();
        d.write(copied, 2, copied.length - 2);
        byte[] value = text.getBytes("UTF-8");
        d.writeShort(count);
        d.writeInt(value.length);
        d.write(value);
        return out.toByteArray();
    }

    static void copyAttributes(DataInputStream in, java.io.DataOutputStream d, int n) throws Exception {
        d.writeShort(n);
        for (int i = 0; i < n; i++) {
            d.writeShort(in.readUnsignedShort());
            int len = in.readInt();
            d.writeInt(len);
            byte[] body = new byte[len];
            in.readFully(body);
            d.write(body);
        }
    }

    public static class Debugged {
        public static String who() {
            return "debugged";
        }
    }

    static void attributeRows() throws Exception {
        System.out.println("sde plain: " + sourceDebugExtension(Plain.class));
        System.out.println("sde int[]: " + sourceDebugExtension(int[].class));
        System.out.println("sde int: " + sourceDebugExtension(int.class));
        byte[] patched = withSourceDebugExtension(bytesOf(Debugged.class), "SMAP\nDebugged.jsp\nJSP\n*E\n");
        ClassLoader isolated = new ClassLoader(null) {
            @Override
            protected Class<?> findClass(String name) throws ClassNotFoundException {
                if (name.equals(Debugged.class.getName())) {
                    return defineClass(name, patched, 0, patched.length);
                }
                throw new ClassNotFoundException(name);
            }
        };
        Class<?> debugged = isolated.loadClass(Debugged.class.getName());
        System.out.println("sde debugged: " + sourceDebugExtension(debugged).replace("\n", "|"));
        System.out.println("sde debugged runs: " + debugged.getMethod("who").invoke(null));

        Object[] cp = constantPoolOf(bytesOf(Plain.class));
        String got = constantPool(Plain.class);
        String[] parts = got.split(" ");
        System.out.println("cp plain: err=" + parts[0]
            + " count-same=" + (parts.length > 1 && Integer.parseInt(parts[1]) == (Integer) cp[0] ? 1 : 0)
            + " bytes-same=" + (parts.length > 2 && parts[2].equals(hex((byte[]) cp[1])) ? 1 : 0));
        System.out.println("cp int[]: " + constantPool(int[].class).split(" ")[0]);
        System.out.println("cp int: " + constantPool(int.class).split(" ")[0]);
    }

    // ---------------------------------------------------------------------
    // Monitors

    static void monitorRows() throws Exception {
        Object lock = new Object();
        System.out.println("usage free: " + monitorUsage(lock));
        CountDownLatch waiting = new CountDownLatch(1);
        Thread waiter = new Thread(() -> {
            synchronized (lock) {
                waiting.countDown();
                try {
                    lock.wait();
                } catch (InterruptedException e) {
                    // done
                }
            }
        }, "l1w63-waiter");
        waiter.start();
        waiting.await();
        park(waiter);
        Thread entrant;
        synchronized (lock) {
            synchronized (lock) {
                entrant = new Thread(() -> {
                    synchronized (lock) {
                        lock.notifyAll();
                    }
                }, "l1w63-entrant");
                entrant.start();
                blocked(entrant);
                System.out.println("usage owned: " + monitorUsage(lock));
                lock.notify();
                System.out.println("usage notified: " + monitorUsage(lock));
            }
        }
        entrant.join();
        waiter.join();
        System.out.println("usage after: " + monitorUsage(lock));
        System.out.println("usage null: " + monitorUsage(null));
    }

    // ---------------------------------------------------------------------
    // Interrupt and stop

    static void signalRows() throws Exception {
        String[] seen = new String[2];
        Thread sleeper = new Thread(() -> {
            try {
                Thread.sleep(20_000);
                seen[0] = "slept";
            } catch (InterruptedException e) {
                seen[0] = "interrupted flag=" + Thread.currentThread().isInterrupted();
            }
        }, "l1w63-sleeper");
        sleeper.start();
        park(sleeper);
        System.out.println("interrupt: err=" + interrupt(sleeper));
        sleeper.join();
        System.out.println("interrupt seen: " + seen[0]);
        Thread victim = new Thread(() -> {
            try {
                Thread.sleep(20_000);
                seen[1] = "slept";
            } catch (InterruptedException e) {
                seen[1] = "interrupted";
            } catch (IllegalStateException e) {
                seen[1] = "stopped by " + e.getMessage();
            }
        }, "l1w63-victim");
        victim.start();
        park(victim);
        System.out.println("stop not throwable: err=" + stop(victim, null));
        System.out.println("stop: err=" + stop(victim, new IllegalStateException("the agent")));
        victim.join();
        System.out.println("stop seen: " + seen[1]);
        Thread fresh = new Thread(() -> { }, "l1w63-unstarted");
        System.out.println("interrupt unstarted: err=" + interrupt(fresh));
        System.out.println("stop unstarted: err=" + stop(fresh, new IllegalStateException("x")));
        System.out.println("interrupt not a thread: err=" + interruptObject());
        Thread vt = Thread.ofVirtual().unstarted(() -> {
            try {
                Thread.sleep(20_000);
                seen[0] = "slept";
            } catch (InterruptedException e) {
                seen[0] = "virtual interrupted";
            }
        });
        vt.start();
        park(vt);
        System.out.println("interrupt virtual: err=" + interrupt(vt));
        vt.join();
        System.out.println("interrupt virtual seen: " + seen[0]);
    }

    /** `InterruptThread` of a `String` (through the `Thread` parameter's
     *  erasure: the shim does not check the type). */
    static native int interruptString(Object s);

    static int interruptObject() {
        return interruptString("not a thread");
    }

    // ---------------------------------------------------------------------
    // RunAgentThread

    static void agentThreadRows() throws Exception {
        Thread t = new Thread("l1w63-agent");
        System.out.print(runAgentThread(t, Thread.MAX_PRIORITY));
        t.join(5_000);
        System.out.println("agent thread after: alive=" + t.isAlive() + " state=" + t.getState());
        Thread started = new Thread(() -> { }, "l1w63-started");
        started.start();
        started.join();
        System.out.print(runAgentThreadErrors(started, new Thread("l1w63-fresh")));
    }

    // ---------------------------------------------------------------------
    // Modules

    public interface Service {
    }

    public static class ServiceImpl implements Service {
    }

    static void moduleRows() {
        Module base = Object.class.getModule();
        Module logging = java.util.logging.Logger.class.getModule();
        Module unnamed = L1W63JvmtiAgentFunctions.class.getModule();
        System.out.println("before: misc exported=" + base.isExported("jdk.internal.misc", unnamed)
            + " reflect open=" + base.isOpen("jdk.internal.reflect", unnamed)
            + " logging reads=" + logging.canRead(unnamed)
            + " logging uses=" + logging.canUse(Service.class));
        System.out.print(modules(base, logging, unnamed, Service.class, ServiceImpl.class, "not a module"));
        System.out.println("after: misc exported=" + base.isExported("jdk.internal.misc", unnamed)
            + " reflect open=" + base.isOpen("jdk.internal.reflect", unnamed)
            + " logging reads=" + logging.canRead(unnamed)
            + " logging uses=" + logging.canUse(Service.class));
    }

    // ---------------------------------------------------------------------
    // Virtual threads, sampling, prefixes

    static void virtualRows() throws Exception {
        Thread[] vts = new Thread[3];
        for (int i = 0; i < vts.length; i++) {
            vts[i] = Thread.ofVirtual().name("l1w63-vt" + i).start(() -> LockSupport.park());
        }
        for (Thread vt : vts) {
            park(vt);
        }
        System.out.print(suspendAllVirtual(vts, vts[1]));
        for (Thread vt : vts) {
            LockSupport.unpark(vt);
            vt.join();
        }
    }

    static void samplingRows() {
        System.out.println("sampling -1: err=" + sampling(-1));
        System.out.println("sampling 0: err=" + sampling(0));
        System.out.println("sampling 524288: err=" + sampling(524288));
    }

    static void prefixRows() {
        try {
            System.out.println("thrice before prefix: " + thrice(5));
        } catch (UnsatisfiedLinkError e) {
            System.out.println("thrice before prefix: UnsatisfiedLinkError");
        }
        System.out.println("prefix null: err=" + prefix(null));
        System.out.println("prefixes negative: err=" + prefixes(null));
        System.out.println("prefix: err=" + prefix("$$L1W63$$"));
        System.out.println("twice: " + twice(21));
        try {
            System.out.println("thrice after prefix: " + thrice(5));
        } catch (UnsatisfiedLinkError e) {
            System.out.println("thrice after prefix: UnsatisfiedLinkError");
        }
        System.out.println("prefixes two: err=" + prefixes(new String[] {"$$A$$", "$$B$$"}));
        System.out.println("prefixes none: err=" + prefixes(new String[0]));
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W63JvmtiAgentFunctions <path of the shim library>");
            return;
        }
        System.load(args[0]);
        System.out.print(onload());
        System.out.print(caps());
        extensionRows();
        verboseRows();
        jniRows();
        attributeRows();
        monitorRows();
        signalRows();
        agentThreadRows();
        moduleRows();
        virtualRows();
        samplingRows();
        prefixRows();
        System.out.println("done");
    }
}

// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with the Rust
// port of the C shim (same calls, same order, same output), three runs, the
// same each time:
//
//   onload potential: err=0 can_tag_objects can_generate_field_modification_events can_generate_field_access_events can_get_bytecodes can_get_synthetic_attribute can_get_owned_monitor_info can_get_current_contended_monitor can_get_monitor_info can_pop_frame can_redefine_classes can_signal_thread can_get_source_file_name can_get_line_numbers can_get_source_debug_extension can_access_local_variables can_maintain_original_method_order can_generate_single_step_events can_generate_exception_events can_generate_frame_pop_events can_generate_breakpoint_events can_suspend can_redefine_any_class can_get_current_thread_cpu_time can_get_thread_cpu_time can_generate_method_entry_events can_generate_method_exit_events can_generate_all_class_hook_events can_generate_compiled_method_load_events can_generate_monitor_events can_generate_vm_object_alloc_events can_generate_native_method_bind_events can_generate_garbage_collection_events can_generate_object_free_events can_force_early_return can_get_owned_monitor_stack_depth_info can_get_constant_pool can_set_native_method_prefix can_retransform_classes can_retransform_any_class can_generate_resource_exhaustion_heap_events can_generate_resource_exhaustion_threads_events can_generate_early_vmstart can_generate_early_class_hook_events can_generate_sampled_object_alloc_events can_support_virtual_threads
//   onload add all: err=0
//   onload SetHeapSamplingInterval 1024: err=0
//   onload GetExtensionFunctions: err=0 count=3
//   caps held: err=0 can_tag_objects can_generate_field_modification_events can_generate_field_access_events can_get_bytecodes can_get_synthetic_attribute can_get_owned_monitor_info can_get_current_contended_monitor can_get_monitor_info can_pop_frame can_redefine_classes can_signal_thread can_get_source_file_name can_get_line_numbers can_get_source_debug_extension can_access_local_variables can_maintain_original_method_order can_generate_single_step_events can_generate_exception_events can_generate_frame_pop_events can_generate_breakpoint_events can_suspend can_redefine_any_class can_get_current_thread_cpu_time can_get_thread_cpu_time can_generate_method_entry_events can_generate_method_exit_events can_generate_all_class_hook_events can_generate_compiled_method_load_events can_generate_monitor_events can_generate_vm_object_alloc_events can_generate_native_method_bind_events can_generate_garbage_collection_events can_generate_object_free_events can_force_early_return can_get_owned_monitor_stack_depth_info can_get_constant_pool can_set_native_method_prefix can_retransform_classes can_retransform_any_class can_generate_resource_exhaustion_heap_events can_generate_resource_exhaustion_threads_events can_generate_early_vmstart can_generate_early_class_hook_events can_generate_sampled_object_alloc_events can_support_virtual_threads
//   caps live: err=0 can_tag_objects can_get_bytecodes can_get_synthetic_attribute can_get_owned_monitor_info can_get_current_contended_monitor can_get_monitor_info can_pop_frame can_redefine_classes can_signal_thread can_get_source_file_name can_get_line_numbers can_get_source_debug_extension can_access_local_variables can_maintain_original_method_order can_generate_single_step_events can_generate_exception_events can_generate_frame_pop_events can_redefine_any_class can_get_current_thread_cpu_time can_get_thread_cpu_time can_generate_method_entry_events can_generate_method_exit_events can_generate_all_class_hook_events can_generate_compiled_method_load_events can_generate_monitor_events can_generate_vm_object_alloc_events can_generate_native_method_bind_events can_generate_garbage_collection_events can_generate_object_free_events can_force_early_return can_get_owned_monitor_stack_depth_info can_get_constant_pool can_set_native_method_prefix can_retransform_classes can_retransform_any_class can_generate_resource_exhaustion_heap_events can_generate_resource_exhaustion_threads_events can_generate_early_vmstart can_generate_early_class_hook_events can_support_virtual_threads
//   ext functions: err=0 count=3
//   ext fn com.sun.hotspot.functions.IsClassUnloadingEnabled [Tell if class unloading is enabled (-noclassgc)] params=IsClassUnloadingEnabled:96:108:0 errors=
//   ext fn com.sun.hotspot.functions.GetVirtualThread [Get virtual thread executed on carrier thread] params=GetVirtualThread:91:110:0,GetVirtualThread:96:110:0 errors=99,10
//   ext fn com.sun.hotspot.functions.GetCarrierThread [Get carrier thread executing virtual thread] params=GetCarrierThread:91:110:0,GetCarrierThread:96:110:0 errors=99,10
//   ext events: err=0 count=3
//   ext ev 49 com.sun.hotspot.events.ClassUnload [CLASS_UNLOAD event] params=JNI Environment:92:117:0,Class:92:115:0
//   ext ev 48 com.sun.hotspot.events.VirtualThreadMount [VIRTUAL_THREAD_MOUNT event] params=JNI Environment:92:117:0,Virtual Thread:91:110:0
//   ext ev 47 com.sun.hotspot.events.VirtualThreadUnmount [VIRTUAL_THREAD_UNMOUNT event] params=JNI Environment:92:117:0,Virtual Thread:91:110:0
//   IsClassUnloadingEnabled: err=0 value=1
//   GetVirtualThread platform: err=0 vthread=null
//   GetCarrierThread parked: err=0 carrier=null
//   GetCarrierThread platform: err=10
//   GetVirtualThread parked: err=10
//   GetVirtualThread null out: err=100
//   GetCarrierThread null thread: err=10
//   SetExtensionEventCallback 46: err=103
//   SetExtensionEventCallback 47: err=0
//   SetExtensionEventCallback 48: err=0
//   SetExtensionEventCallback 49: err=0
//   SetExtensionEventCallback 50: err=103
//   GetExtensionFunctions null count: err=100
//   GetExtensionEvents null array: err=100
//   mounted GetCarrierThread: err=0 carrier=thread carrier-virtual=0
//   mounted GetVirtualThread carrier: err=0 same=1
//   verbose other false: err=0
//   verbose jni false: err=0
//   verbose 3 false: err=103
//   verbose 8 false: err=103
//   verbose -1 false: err=103
//   jni get: err=0 same=1 copy=1
//   jni get null: err=100
//   jni set null: err=100
//   jni set: err=0
//   jni version: 0x180000 hooked-calls=1 restore err=0 after-restore=1
//   sde plain: !101
//   sde int[]: !101
//   sde int: !101
//   sde debugged: =SMAP|Debugged.jsp|JSP|*E|
//   sde debugged runs: debugged
//   cp plain: err=0 count-same=1 bytes-same=1
//   cp int[]: 101
//   cp int: 101
//   usage free: err=0 owner=null entry_count=0 waiters=0[] notify_waiters=0[]
//   usage owned: err=0 owner=main entry_count=2 waiters=1[l1w63-entrant] notify_waiters=1[l1w63-waiter]
//   usage notified: err=0 owner=main entry_count=2 waiters=2[l1w63-entrant,l1w63-waiter] notify_waiters=0[]
//   usage after: err=0 owner=null entry_count=0 waiters=0[] notify_waiters=0[]
//   usage null: err=20
//   interrupt: err=0
//   interrupt seen: interrupted flag=false
//   stop not throwable: err=100
//   stop: err=0
//   stop seen: stopped by the agent
//   interrupt unstarted: err=15
//   stop unstarted: err=15
//   interrupt not a thread: err=10
//   interrupt virtual: err=0
//   interrupt virtual seen: virtual interrupted
//   RunAgentThread: err=0
//   agent proc: name=l1w63-agent priority=10 daemon=1 arg=1 current=1 currentThread=1 alive=1 runnable=1
//   agent thread after: alive=false state=TERMINATED
//   RunAgentThread null proc: err=100
//   RunAgentThread priority 0: err=12
//   RunAgentThread priority 11: err=12
//   RunAgentThread null thread: err=10
//   RunAgentThread not a thread: err=10
//   RunAgentThread terminated thread: err=0
//   before: misc exported=false reflect open=false logging reads=false logging uses=false
//   GetAllModules: err=0 many=1 base=1 logging=1 app-unnamed=1
//   GetAllModules null: err=100
//   GetNamedModule boot java/lang: err=0 same=1
//   GetNamedModule boot java/util/logging: err=0 same=1
//   GetNamedModule boot no/such: err=0 null
//   GetNamedModule null package: err=100
//   GetNamedModule not a loader: err=103
//   AddModuleReads: err=0
//   AddModuleExports dotted: err=0
//   AddModuleExports slashed: err=103
//   AddModuleExports not in module: err=103
//   AddModuleOpens: err=0
//   AddModuleUses: err=0
//   AddModuleProvides: err=0
//   AddModuleReads not a module: err=26
//   AddModuleReads null to: err=100
//   AddModuleUses not a class: err=21
//   IsModifiableModule base: err=0 value=1
//   IsModifiableModule unnamed: err=0 value=1
//   IsModifiableModule not a module: err=26
//   after: misc exported=true reflect open=true logging reads=true logging uses=true
//   SuspendAll negative: err=103
//   SuspendAll null list: err=100
//   SuspendAllVirtualThreads except vt1: err=0
//   suspended: vt0=1 vt1=0 vt2=1
//   ResumeAllVirtualThreads: err=0
//   resumed: vt0=0 vt1=0 vt2=0
//   sampling -1: err=103
//   sampling 0: err=0
//   sampling 524288: err=0
//   thrice before prefix: UnsatisfiedLinkError
//   prefix null: err=0
//   prefixes negative: err=103
//   prefix: err=0
//   twice: 42
//   thrice after prefix: 15
//   prefixes two: err=0
//   prefixes none: err=0
//   done
