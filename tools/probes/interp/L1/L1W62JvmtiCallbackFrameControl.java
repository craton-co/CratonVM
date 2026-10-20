// Interpreter round i2 wave 62, lane L1: the C JVMTI table's frame control
// from an event callback, against HotSpot -- stage 1 of
// docs/internal/proposals/interpreter/i61-L1-proposal-frame-control-from-an-event-callback-20261025.md:
// `ForceEarlyReturn*` (81-86) and `PopFrame` (80) of the calling thread made
// inside its own `Breakpoint`, `SingleStep` or `MethodEntry` callback, where
// the thread's top frame is the Java frame the event is about (the
// fault-injection pattern: make a method return a chosen value, without a
// second thread).
//
// `arm(event, method, sig, location, actions)` makes the agent act once, in
// the first callback of `event` (1 `Breakpoint` at `location`, 2
// `SingleStep` at `location`, 3 `MethodEntry`) for `method`: each
// space-separated action calls one function on the event's thread and
// records its `jvmtiError` (`int:42` is `ForceEarlyReturnInt(thread, 42)`,
// `intnull:7` the same with a NULL thread, `long`, `obj` (a string
// "forced"), `void`, `pop`). The row prints those numbers, how many callbacks
// matched (`hits`), how many times the target's body began (`calls`) and
// what the call returned. Each row runs once; the first runs again at the
// end (the cold-VM first-call rows).
//
// Needs the native shim tools/probes/interp/L1/L1W62JvmtiCallbackFrameControl.c,
// loaded both as an agent and with `System.load` (for the natives):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w62cbframes.so tools/probes/interp/L1/L1W62JvmtiCallbackFrameControl.c
//   javac -d /tmp/l1w62cbframes tools/probes/interp/L1/L1W62JvmtiCallbackFrameControl.java
//   java     -agentpath:/tmp/libl1w62cbframes.so -cp /tmp/l1w62cbframes L1W62JvmtiCallbackFrameControl /tmp/libl1w62cbframes.so
//   cratonvm -agentpath:/tmp/libl1w62cbframes.so -cp /tmp/l1w62cbframes L1W62JvmtiCallbackFrameControl /tmp/libl1w62cbframes.so
//
// Without the argument it prints only the usage line. The Rust port of the
// shim is tools/probes/interp/L1/rust-ports/L1W62JvmtiCallbackFrameControl
// (README in tools/probes/interp/L1/rust-ports/).
//
// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with the Rust
// port of the C shim (same calls, same order, same output), three runs, the
// same each time:
//
//   bp int: arm=0 long=34 void=34 int:42=0 int:43=113 hits=1 calls=0 returned 42
//   bp int null thread: arm=0 intnull:7=0 hits=1 calls=0 returned 7
//   bp int mid: arm=0 int:42=0 hits=1 calls=1 returned 42
//   bp long: arm=0 int:1=34 long=0 hits=1 calls=0 returned 77
//   bp obj: arm=0 int:1=34 obj=0 hits=1 calls=0 returned forced
//   bp void: arm=0 int:1=34 void=0 hits=1 calls=0 returned reached=false
//   bp pop: arm=0 pop=0 pop=113 hits=2 calls=1 returned 41
//   bp pop then int: arm=0 pop=0 int:9=0 hits=1 calls=0 returned 9
//   step int: arm=0 int:44=0 hits=1 calls=0 returned 44
//   step int mid: arm=0 int:46=0 hits=1 calls=1 returned 46
//   step pop: arm=0 pop=0 hits=2 calls=1 returned 41
//   entry int: arm=0 int:45=0 hits=1 calls=0 returned 45
//   entry pop: arm=0 pop=0 hits=2 calls=1 returned 41
//   bp int: arm=0 long=34 void=34 int:42=0 int:43=113 hits=1 calls=0 returned 42
//
// (`bp pop then int`: HotSpot pops `popTarget`, then returns 9 from its
// caller `popCaller`, the frame on top once the pop is done.)
//
// CratonVM before wave 62, and with `CRATONVM_JVMTI_C_CALLBACK_FRAME_CONTROL=0`
// (the negative control): every request of the calling thread is 32
// (`OPAQUE_FRAME`) and the calls run on. With the switch on (a debug
// `cratonvm` with `experimental-debug`): HotSpot's lines except the two
// `entry` rows, stage 2 of the proposal (32, the calls run on:
// `entry int: arm=0 int:45=32 hits=1 calls=1 returned 2`,
// `entry pop: arm=0 pop=32 hits=1 calls=1 returned 41`). Positive
// control: `CRATONVM_FRAME_TRACE=1` prints `[CALLBACK_FRAME_CONTROL]
// requested tid=<n> depth=<d>` per served request, with
// `[JVMTI_EARLY_RETURN] recorded ...` or `[POP_FRAMES] jvmti callback
// recorded ...` before it.
public class L1W62JvmtiCallbackFrameControl {
    static native int arm(int event, String method, String sig, long location, String actions);

    /** Disarms; `<action>=<err> ... hits=<n>`. */
    static native String disarm();

    static final int BREAKPOINT = 1;
    static final int SINGLE_STEP = 2;
    static final int METHOD_ENTRY = 3;

    static int calls;
    static boolean reached;

    static int intTarget(int x) {
        calls++;
        return x + 1;
    }

    static long longTarget() {
        calls++;
        return 5L;
    }

    static String objTarget() {
        calls++;
        return "plain";
    }

    static void voidTarget() {
        calls++;
        reached = true;
    }

    static int popTarget(int n) {
        calls++;
        return n * 2;
    }

    static int popCaller() {
        return popTarget(20) + 1;
    }

    static void row(String label, int event, String method, String sig, long location, String actions) {
        calls = 0;
        reached = false;
        int err = arm(event, method, sig, location, actions);
        Object returned;
        switch (method) {
            case "intTarget":
                returned = intTarget(1);
                break;
            case "longTarget":
                returned = longTarget();
                break;
            case "objTarget":
                returned = objTarget();
                break;
            case "voidTarget":
                voidTarget();
                returned = "reached=" + reached;
                break;
            default:
                returned = popCaller();
                break;
        }
        String done = disarm();
        System.out.println(label + ": arm=" + err + " " + done + " calls=" + calls + " returned " + returned);
    }

    public static void main(String[] args) {
        if (args.length != 1) {
            System.out.println("usage: L1W62JvmtiCallbackFrameControl <path of the shim library>");
            return;
        }
        System.load(args[0]);
        row("bp int", BREAKPOINT, "intTarget", "(I)I", 0, "long void int:42 int:43");
        row("bp int null thread", BREAKPOINT, "intTarget", "(I)I", 0, "intnull:7");
        // Location 8 is `iload_0`, after `calls++` (getstatic, iconst_1, iadd, putstatic).
        row("bp int mid", BREAKPOINT, "intTarget", "(I)I", 8, "int:42");
        row("bp long", BREAKPOINT, "longTarget", "()J", 0, "int:1 long");
        row("bp obj", BREAKPOINT, "objTarget", "()Ljava/lang/String;", 0, "int:1 obj");
        row("bp void", BREAKPOINT, "voidTarget", "()V", 0, "int:1 void");
        row("bp pop", BREAKPOINT, "popTarget", "(I)I", 0, "pop pop");
        row("bp pop then int", BREAKPOINT, "popTarget", "(I)I", 0, "pop int:9");
        row("step int", SINGLE_STEP, "intTarget", "(I)I", 0, "int:44");
        row("step int mid", SINGLE_STEP, "intTarget", "(I)I", 8, "int:46");
        row("step pop", SINGLE_STEP, "popTarget", "(I)I", 0, "pop");
        row("entry int", METHOD_ENTRY, "intTarget", "(I)I", -1, "int:45");
        row("entry pop", METHOD_ENTRY, "popTarget", "(I)I", -1, "pop");
        row("bp int", BREAKPOINT, "intTarget", "(I)I", 0, "long void int:42 int:43");
    }
}
