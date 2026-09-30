// Interpreter round i1 wave 46, lane L1: the C JVMTI table's GetAllThreads,
// GetThreadListStackTraces and GetAllStackTraces against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-proposal-c-jvmti-all-threads-stack-traces-FIXED-20261010.md).
//
// `main` starts three daemon threads and calls the native `rows` once all
// three run: `sleeper` sleeps in one long `Thread.sleep`, `spinner` loops in
// a Java method (`spin`), `waiter` waits on a monitor with no timeout. A
// finished thread and a thread never started are passed too. `rows` prints
// one line per call, `<row>: <answer>`, with `err=<jvmtiError number>` for
// an error. A `jvmtiStackInfo` is printed as `<name> st=0x<state> n=<frame
// count> <method names>`, its state masked to the `java.lang.Thread.State`
// bits, SUSPENDED and INTERRUPTED. `GetAllThreads` and `GetAllStackTraces`
// list the VM's own threads too, which differ between VMs: only whether
// the known threads are among them is printed.
//
// Needs the native shim tools/probes/interp/L1/L1W46JvmtiAllStackTraces.c,
// loaded both as an agent and with `System.load` (for `rows`):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w46all.so tools/probes/interp/L1/L1W46JvmtiAllStackTraces.c
//   javac -d /tmp/l1w46all tools/probes/interp/L1/L1W46JvmtiAllStackTraces.java
//   java     -agentpath:/tmp/libl1w46all.so -cp /tmp/l1w46all L1W46JvmtiAllStackTraces /tmp/libl1w46all.so
//   cratonvm -agentpath:/tmp/libl1w46all.so -cp /tmp/l1w46all L1W46JvmtiAllStackTraces /tmp/libl1w46all.so
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
//   GetAllThreads: err=0 sleeper=1 spinner=1 waiter=1 finished=0 unstarted=0 main=1
//   GetAllThreads NULL count_ptr: err=100
//   GetAllThreads NULL threads_ptr: err=100
//   GetThreadListStackTraces sleeper waiter spinner max 3: err=0
//     sleeper st=0xa1 n=3 sleepNanos0 sleepNanos sleep
//     waiter st=0x91 n=3 wait0 wait wait
//     spinner st=0x5 n=2 spin run
//   GetThreadListStackTraces main max 2: err=0
//     main st=0x5 n=2 rows main
//   GetThreadListStackTraces finished unstarted max 3: err=0
//     finished st=0x2 n=0
//     unstarted st=0x0 n=0
//   GetThreadListStackTraces waiter max 0: err=0
//     waiter st=0x91 n=0
//   GetThreadListStackTraces count 0: err=0
//   GetThreadListStackTraces count -1: err=103
//   GetThreadListStackTraces max -1: err=103
//   GetThreadListStackTraces NULL list: err=100
//   GetThreadListStackTraces NULL stack_info_ptr: err=100
//   GetThreadListStackTraces of a class: err=10
//   GetAllStackTraces max 3: err=0 at least four=1
//     sleeper st=0xa1 n=3 sleepNanos0 sleepNanos sleep
//     spinner st=0x5 n=2 spin run
//     waiter st=0x91 n=3 wait0 wait wait
//     main st=0x5 n=2 rows main
//     known threads listed: 4
//   GetAllStackTraces max -1: err=103
//   GetAllStackTraces NULL stack_info_ptr: err=100
//   GetAllStackTraces NULL count_ptr: err=100
//
// CratonVM before wave 46 (from reading `jvmti::native_env`): slots 4, 100
// and 101 are unimplemented and answer `JVMTI_ERROR_NOT_AVAILABLE` (98)
// whatever they are asked: `GetAllThreads: err=98 sleeper=0 spinner=0
// waiter=0 finished=0 unstarted=0 main=0`, and every other row, the argument
// checks included, prints `err=98` with no stack lines.
//
// CratonVM since wave 46, predicted from the code: every row matches. Each
// stack is read as `GetStackTrace` reads it (`native_env::frames_of`): the
// sleeper and the waiter through their windows, the spinner after a
// handshake, `main` from its own listing. Positive control:
// `CRATONVM_FRAME_TRACE=1` prints `[JVMTI_ALL_STACKS] threads=<n>` for the
// `GetAllStackTraces max 3` row, one `[JVMTI_OTHER_STACK] tid=<n> rows=<k>
// via=window|handshake` per other thread read, and `[JVMTI_SUSPEND]
// handshake tid=<n> stopped=true` for the spinner; none exists on the base
// for these calls. HotSpot reads every stack at one safepoint; this VM
// reads them one after another (the rows do not depend on it).
// `--compatible` may differ in the sleeper's and the waiter's frame names,
// where a registered native stands in for `Thread.sleep` or `Object.wait`.
public class L1W46JvmtiAllStackTraces {
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
            synchronized (L1W46JvmtiAllStackTraces.class) {
                started++;
            }
            try {
                Thread.sleep(600_000);
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
            synchronized (L1W46JvmtiAllStackTraces.class) {
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
                    synchronized (L1W46JvmtiAllStackTraces.class) {
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
            System.out.println("usage: L1W46JvmtiAllStackTraces <path of the shim library>");
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
