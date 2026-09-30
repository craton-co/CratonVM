// Interpreter round i1 wave 45, lane L1: the C JVMTI table's thread
// suspension (SuspendThread, ResumeThread, SuspendThreadList,
// ResumeThreadList), GetThreadState, and the stack functions (GetFrameCount,
// GetFrameLocation, GetStackTrace) asked about ANOTHER thread: suspended,
// running, and blocked; stage 1 of
// docs/known-issues/interpreter/i44-L1-proposal-c-jvmti-suspension-and-frame-control-20261008.md
// and the stack half of
// docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-reads-no-other-threads-stack-FIXED-20261010.md.
//
// `main` starts three daemon threads and calls the native `rows` once all
// three run: `sleeper` loops in `Thread.sleep(50)`, `spinner` loops in a
// Java method (`spin`), `waiter` waits on a monitor with no timeout. A
// finished thread and a thread never started are passed too. `rows` prints
// one line per call, `<row>: <answer>`, with `err=<jvmtiError number>` for
// an error. Thread states are printed masked to the bits this table
// answers: the `java.lang.Thread.State` bits, SUSPENDED and INTERRUPTED
// (`st=0x...`). The `after 300 ms` rows run once the suspended `sleeper`'s
// 50 ms native has returned: HotSpot still lists it in `sleepNanos0`.
//
// Needs the native shim tools/probes/interp/L1/L1W45JvmtiSuspendAndOtherStacks.c,
// loaded both as an agent (its `Agent_OnLoad` adds `can_suspend`) and with
// `System.load` (for `rows`):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w45susp.so tools/probes/interp/L1/L1W45JvmtiSuspendAndOtherStacks.c
//   javac -d /tmp/l1w45susp tools/probes/interp/L1/L1W45JvmtiSuspendAndOtherStacks.java
//   java     -agentpath:/tmp/libl1w45susp.so -cp /tmp/l1w45susp L1W45JvmtiSuspendAndOtherStacks /tmp/libl1w45susp.so
//   cratonvm -agentpath:/tmp/libl1w45susp.so -cp /tmp/l1w45susp L1W45JvmtiSuspendAndOtherStacks /tmp/libl1w45susp.so
//
// Without the argument it prints only the usage line. HotSpot also prints
// its restricted-method WARNING lines for `System.load` on stderr.
//
// Expected stdout (HotSpot 25.0.3): measured on the Windows box with a Rust
// port of the C shim (the box has no C compiler; same calls, same order,
// same output format), three runs, the same each time. The orchestrator
// should run the HotSpot line above once with the C shim and correct this
// block if anything differs.
//
//   live-phase potential: err=0 can_suspend=0
//   Agent_OnLoad potential: can_suspend=1
//   Agent_OnLoad AddCapabilities: err=0
//   no capability SuspendThread: err=99
//   no capability ResumeThread: err=99
//   no capability SuspendThreadList: err=99
//   no capability ResumeThreadList: err=99
//   AddCapabilities in the live phase: err=98
//   GetThreadState NULL (current): st=0x5
//   GetThreadState sleeper: st=0xa1
//   GetThreadState spinner: st=0x5
//   GetThreadState waiter: st=0x91
//   GetThreadState finished: st=0x2
//   GetThreadState unstarted: st=0x0
//   GetThreadState NULL state_ptr: err=100
//   GetThreadState of a class: err=10
//   SuspendThread sleeper: err=0
//   SuspendThread sleeper again: err=14
//   GetThreadState sleeper (suspended): st=0x1000a1
//   after 300 ms GetFrameCount sleeper: err=0 count=4
//   after 300 ms GetStackTrace sleeper 0 3: n=3 sleepNanos0 sleepNanos sleep
//   after 300 ms GetStackTrace sleeper -1 1: n=1 run
//   after 300 ms GetFrameLocation sleeper 0: err=0 sleepNanos0@-1
//   after 300 ms GetFrameLocation sleeper 1: err=0 sleepNanos@33
//   after 300 ms GetThreadState sleeper: st=0x100005
//   ResumeThread sleeper: err=0
//   ResumeThread sleeper again: err=13
//   GetFrameCount spinner (running): err=0 count=2
//   GetStackTrace spinner (running) 0 2: n=2 spin run
//   GetThreadState spinner (after the reads): st=0x5
//   GetFrameCount waiter (waiting): err=0 count=4
//   GetStackTrace waiter (waiting) 0 3: n=3 wait0 wait wait
//   GetFrameLocation waiter 0: err=0 wait0@-1
//   SuspendThreadList sleeper waiter sleeper: err=0 results=0 0 14
//   GetThreadState waiter (suspended): st=0x100091
//   GetStackTrace waiter (suspended) 0 3: n=3 wait0 wait wait
//   ResumeThreadList sleeper waiter sleeper: err=0 results=0 0 13
//   GetThreadState waiter (resumed): st=0x91
//   SuspendThreadList count 0: err=0
//   SuspendThreadList count -1: err=103
//   SuspendThreadList NULL list: err=100
//   SuspendThreadList NULL results: err=100
//   SuspendThread finished: err=15
//   ResumeThread finished: err=15
//   SuspendThread unstarted: err=15
//   SuspendThread of a class: err=10
//   ResumeThread spinner (running): err=13
//   GetFrameCount finished: err=15
//   GetFrameCount unstarted: err=15
//
// CratonVM before wave 45 (from reading `jvmti::native_env`): slots 5, 6,
// 17, 92 and 93 are unimplemented and answer `JVMTI_ERROR_NOT_AVAILABLE`
// (98) whatever they are asked, so every SuspendThread, ResumeThread,
// *ThreadList and GetThreadState row prints `err=98`; `can_suspend` is not
// potential (`Agent_OnLoad potential: can_suspend=0`, `Agent_OnLoad
// AddCapabilities: err=98`); every stack function asked about another
// thread answers 98 (`frames_of`), and `GetFrameCount unstarted` answers
// 10 (`INVALID_THREAD`). Since wave 45 the rows match, predicted from the
// code (the `after 300 ms` rows need the native-exit park,
// `interpreter::park_if_suspended_at_native_exit`, switched on). Positive control:
// `CRATONVM_FRAME_TRACE=1` prints `[JVMTI_SUSPEND] suspend tid=<n>
// stopped=true` for each `SuspendThread` of another thread, `[JVMTI_SUSPEND]
// handshake tid=<n> stopped=true` for each stack read of a running one, and
// `[JVMTI_OTHER_STACK] tid=<n> rows=<k>` for each stack read of another
// thread. `--compatible` may differ in the `sleeper` and `waiter` frame
// names, where a registered native stands in for `Thread.sleep` or
// `Object.wait`.
public class L1W45JvmtiSuspendAndOtherStacks {
    static final Object W = new Object();
    static volatile int started;
    static volatile long sink;

    static native String rows(Thread sleeper, Thread spinner, Thread waiter, Thread finished,
            Thread unstarted);

    static final class Sleeper extends Thread {
        Sleeper() {
            super("sleeper");
            setDaemon(true);
        }

        @Override
        public void run() {
            synchronized (L1W45JvmtiSuspendAndOtherStacks.class) {
                started++;
            }
            try {
                while (true) {
                    Thread.sleep(50);
                }
            } catch (InterruptedException stop) {
                // Ends with the program.
            }
        }
    }

    static final class Spinner extends Thread {
        Spinner() {
            super("spinner");
            setDaemon(true);
        }

        static void spin() {
            long x = 0;
            while (true) {
                x = x * 31 + 7;
                sink = x;
            }
        }

        @Override
        public void run() {
            synchronized (L1W45JvmtiSuspendAndOtherStacks.class) {
                started++;
            }
            spin();
        }
    }

    static final class Waiter extends Thread {
        Waiter() {
            super("waiter");
            setDaemon(true);
        }

        @Override
        public void run() {
            try {
                synchronized (W) {
                    synchronized (L1W45JvmtiSuspendAndOtherStacks.class) {
                        started++;
                    }
                    W.wait();
                }
            } catch (InterruptedException stop) {
                // Ends with the program.
            }
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W45JvmtiSuspendAndOtherStacks <path of the shim library>");
            return;
        }
        System.load(args[0]);
        Thread finished = new Thread(() -> { }, "finished");
        finished.start();
        finished.join();
        Thread unstarted = new Thread(() -> { }, "unstarted");
        Thread sleeper = new Sleeper();
        Thread spinner = new Spinner();
        Thread waiter = new Waiter();
        sleeper.start();
        spinner.start();
        waiter.start();
        while (started < 3) {
            Thread.sleep(5);
        }
        // The waiter has released W inside wait() once main can take it.
        synchronized (W) {
            // Nothing: only to know the waiter waits.
        }
        Thread.sleep(200);
        System.out.print(rows(sleeper, spinner, waiter, finished, unstarted));
    }
}
