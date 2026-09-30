// Interpreter round i1 wave 44, lane L1: the C JVMTI table's local-variable,
// frame and monitor functions against HotSpot, called on the current thread
// from a JNI native: GetLocal* / SetLocal* / GetLocalInstance (their error
// codes: no capability, a type mismatch, an invalid slot, a native frame,
// depths out of range, a NULL out-parameter, another thread),
// GetFrameLocation, GetStackTrace with negative start depths,
// GetCurrentContendedMonitor and GetOwnedMonitorStackDepthInfo.
//
// `caller` (a static method, compiled with -g so it has a
// LocalVariableTable) calls the native `rows` from inside a
// `synchronized (LOCK)` block. `rows` prints one line per call it makes,
// `<row>: <answer>`, with `err=<jvmtiError number>` for an error, and writes
// two of `caller`'s locals (`after = 42`, `i = 9`), which `caller` prints
// when the native returns. The rows that need a capability are asked once
// before `AddCapabilities` and once after.
//
// Needs the native shim tools/probes/interp/L1/L1W44JvmtiLocalsAndStack.c,
// loaded both as an agent (its `Agent_OnLoad` adds the capabilities, which
// HotSpot grants only then, or later once a startup agent acquired them)
// and with `System.load` (for `rows`):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w44locals.so tools/probes/interp/L1/L1W44JvmtiLocalsAndStack.c
//   javac -g -d /tmp/l1w44locals tools/probes/interp/L1/L1W44JvmtiLocalsAndStack.java
//   java     -agentpath:/tmp/libl1w44locals.so -cp /tmp/l1w44locals L1W44JvmtiLocalsAndStack /tmp/libl1w44locals.so
//   cratonvm -agentpath:/tmp/libl1w44locals.so -cp /tmp/l1w44locals L1W44JvmtiLocalsAndStack /tmp/libl1w44locals.so
//
// (`-g` matters: the type and slot checks read the LocalVariableTable.)
// Without the argument it prints only the usage line (HotSpot and CratonVM
// alike). HotSpot also prints its restricted-method WARNING lines for
// `System.load` on stderr.
//
// Expected stdout (HotSpot 25.0.3): measured on the Windows box with a Rust
// port of the C shim (the box has no C compiler; same calls, same order,
// same output format), three runs, the same each time. The orchestrator
// should run the HotSpot line above once with the C shim and correct this
// block if anything differs.
//
//   live-phase potential: err=0 access_local_variables=1 current_contended_monitor=1 owned_monitor_stack_depth_info=1
//   no capability GetLocalInt: err=99
//   no capability SetLocalInt: err=99
//   no capability GetLocalInstance: err=99
//   no capability GetCurrentContendedMonitor: err=99
//   no capability GetOwnedMonitorStackDepthInfo: err=99
//   AddCapabilities in the live phase: err=0
//   Agent_OnLoad potential: access_local_variables=1 current_contended_monitor=1 owned_monitor_stack_depth_info=1
//   Agent_OnLoad AddCapabilities: err=0
//   GetFrameCount: err=0 count=3
//   GetFrameLocation 0: err=0 rows@-1
//   GetFrameLocation 1: err=0 caller@16
//   GetFrameLocation -1: err=103
//   GetFrameLocation count: err=31
//   GetFrameLocation NULL method_ptr: err=100
//   GetStackTrace -1 5: n=1 main
//   GetStackTrace -2 1: n=1 caller
//   GetStackTrace -count 32: n=3 rows caller main
//   GetStackTrace -(count+1) 32: err=103
//   GetStackTrace count 32: err=103
//   GetStackTrace count-1 32: n=1 main
//   GetStackTrace 0 0: n=0
//   GetStackTrace 1 2: n=2 caller main
//   GetStackTrace 0 -1: err=103
//   GetLocalInt i: err=0 3
//   GetLocalLong l: err=0 4
//   GetLocalFloat f: err=0 5.5
//   GetLocalDouble d: err=0 6.25
//   GetLocalObject o: err=0 obj
//   GetLocalInt after: err=0 7
//   GetLocalObject other: err=0 same=1
//   GetLocalInt i of the current thread's jthread: err=0 3
//   GetLocalInt l: err=34
//   GetLocalInt second half of l: err=35
//   GetLocalInt f: err=34
//   GetLocalInt o: err=34
//   GetLocalLong i: err=34
//   GetLocalLong d: err=34
//   GetLocalFloat d: err=34
//   GetLocalDouble f: err=34
//   GetLocalObject i: err=34
//   GetLocalObject r (not assigned yet): err=34
//   GetLocalInt r (not assigned yet): err=35
//   GetLocalObject slot 10 (no table entry): err=35
//   GetLocalInt slot 11 (no table entry, never written): err=35
//   GetLocalDouble slot 11 (past max_locals): err=35
//   GetLocalInt slot 99: err=35
//   GetLocalInt slot -1: err=35
//   GetLocalInt depth 0 (the native): err=32
//   GetLocalInt depth -1: err=103
//   GetLocalInt depth count: err=31
//   GetLocalInt NULL value_ptr: err=100
//   GetLocalInstance depth 1 (static): err=35
//   GetLocalInstance depth 0 (the native): err=35
//   GetLocalInt of a running other thread: err=13
//   GetFrameCount of a running other thread: err=0
//   SetLocalInt after=42: err=0
//   SetLocalInt i=9: err=0
//   SetLocalLong i: err=34
//   SetLocalFloat d: err=34
//   SetLocalObject i: err=34
//   SetLocalInt slot 99: err=35
//   SetLocalInt depth 0 (the native): err=32
//   SetLocalObject o=lock: err=0 reads back the lock=1
//   GetLocalInt after, after the write: 42
//   GetCurrentContendedMonitor: err=0 null=1
//   GetCurrentContendedMonitor NULL monitor_ptr: err=100
//   GetOwnedMonitorStackDepthInfo: err=0 count=1 depth=1 lock=1
//   GetOwnedMonitorStackDepthInfo NULL count_ptr: err=100
//   GetCurrentContendedMonitor of a running other thread: err=0
//   GetOwnedMonitorStackDepthInfo of a running other thread: err=0
//   caller after the native: after=42 i=9
//
// CratonVM before wave 44 (from reading `jvmti::native_env`): every
// local-variable function, `GetCurrentContendedMonitor` and
// `GetOwnedMonitorStackDepthInfo` is an unimplemented slot answering
// `JVMTI_ERROR_NOT_AVAILABLE` (98) whatever it is asked, the three
// capabilities are not potential (both potential rows print `0 0 0`, and
// both `AddCapabilities` rows `err=98`), so every such row prints `err=98`
// and `caller` prints `after=7 i=3`; the frame and stack-trace rows already
// matched.
//
// CratonVM since wave 44, predicted from the code: every row matches but
// two, which read ANOTHER thread's stack: `GetFrameCount of a running other
// thread` and `GetOwnedMonitorStackDepthInfo of a running other thread`
// print `err=98`. This table cannot suspend a thread, and HotSpot reads a
// running one's stack; filed as
// docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-reads-no-other-threads-stack-FIXED-20261010.md.
// Since wave 45 (lane L1) `GetFrameCount of a running other thread` prints
// `err=0` (the sleeping `other` is read through its blocking region's
// inspection window, `interpreter::blocked_frame_rows`); the
// `GetOwnedMonitorStackDepthInfo` row still prints `err=98`.
// Wave 46 (lane L1): it prints `err=0` (`native_env::other_monitors`), and
// every row matches, predicted from the code. Between wave 44's merge and
// wave 46 the local rows did not: `native_env::listed_rows` looked its
// interpreter frames up among the JNI natives' anchor positions, so every
// `GetLocal*` / `SetLocal*` row that names `caller` printed `err=32`, the
// `GetOwnedMonitorStackDepthInfo` row `depth=-1`, and `caller` printed
// `after=7 i=3` (read from the code; fixed in wave 46).
// `--compatible` must print the same. No debug line: the rows are the
// positive control (every `err=98` row of the base changes).
public class L1W44JvmtiLocalsAndStack {
    static final Object LOCK = new Object();

    /** The JVMTI rows, run on this thread (see the header). */
    static native String rows(Object lock, Thread other);

    static String caller(int i, long l, float f, double d, Object o, Thread other) {
        int after = 7;
        String r;
        synchronized (LOCK) {
            r = rows(LOCK, other);
        }
        return r + "caller after the native: after=" + after + " i=" + i;
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W44JvmtiLocalsAndStack <absolute path of the native shim>");
            return;
        }
        System.load(args[0]);
        Thread other = new Thread(() -> {
            try {
                Thread.sleep(60_000);
            } catch (InterruptedException ignored) {
                // Ends the thread.
            }
        }, "other");
        other.setDaemon(true);
        other.start();
        System.out.println(caller(3, 4L, 5.5f, 6.25, "obj", other));
    }
}
