// Interpreter round i1 wave 45, lane L1: the C JVMTI table's
// NotifyFramePop and the FramePop event against HotSpot, stage 3 of
// docs/known-issues/interpreter/i44-L1-proposal-c-jvmti-suspension-and-frame-control-20261008.md.
//
// `caller` calls the native `rows(other, 1)`, which asks NotifyFramePop for
// its own frame (depth 0, a native method), for `caller` (depth 1, twice),
// for a negative depth, for a depth past the bottom, and for the sleeping
// `other` thread (not suspended). `thrower` calls `rows(other, 2)`, which
// asks it for `thrower`, then throws out of `thrower`. The agent's FramePop
// callback records `FramePop <method> popped=<was_popped_by_exception>`;
// `events()` hands the records back. `rows` prints one line per call,
// `<row>: <answer>`, with `err=<jvmtiError number>` for an error.
//
// Needs the native shim tools/probes/interp/L1/L1W45JvmtiNotifyFramePop.c,
// loaded both as an agent (its `Agent_OnLoad` adds
// `can_generate_frame_pop_events` and sets the callback) and with
// `System.load` (for `rows` and `events`):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w45pop.so tools/probes/interp/L1/L1W45JvmtiNotifyFramePop.c
//   javac -d /tmp/l1w45pop tools/probes/interp/L1/L1W45JvmtiNotifyFramePop.java
//   java     -agentpath:/tmp/libl1w45pop.so -cp /tmp/l1w45pop L1W45JvmtiNotifyFramePop /tmp/libl1w45pop.so
//   cratonvm -agentpath:/tmp/libl1w45pop.so -cp /tmp/l1w45pop L1W45JvmtiNotifyFramePop /tmp/libl1w45pop.so
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
//   live-phase potential: err=0 can_generate_frame_pop_events=1
//   Agent_OnLoad potential: can_generate_frame_pop_events=1
//   Agent_OnLoad AddCapabilities: err=0 SetEventCallbacks: err=0
//   no capability NotifyFramePop: err=99
//   no capability enable FramePop: err=99
//   AddCapabilities in the live phase: err=0
//   enable FramePop: err=0
//   NotifyFramePop depth 0 (the native): err=32
//   NotifyFramePop depth 1 (caller): err=0
//   NotifyFramePop depth 1 again: err=40
//   NotifyFramePop depth -1: err=103
//   NotifyFramePop depth 99: err=31
//   NotifyFramePop of a running other thread: err=13
//   NotifyFramePop of a class: err=10
//   NotifyFramePop thrower: err=0
//   FramePop caller popped=0
//   FramePop thrower popped=1
//   r=8
//
// CratonVM before wave 45 (from reading `jvmti::native_env`): slot 20 is
// unimplemented (`JVMTI_ERROR_NOT_AVAILABLE`, 98, for every NotifyFramePop
// row), `can_generate_frame_pop_events` is not potential and `FramePop`
// (61) is not an event the table delivers (`SetEventNotificationMode`
// answers 98 or 99), and no `FramePop` line prints. Since wave 45 the rows
// match, predicted from the code (`jvmti::native_env::notify_frame_pop`;
// the event rides `thread.frame_pop_requests` and
// `jvmti_events::fire_jvmti_frame_pop_if_requested`). Positive control: the
// two `FramePop` lines, and `CRATONVM_FRAME_TRACE=1` prints
// `[JVMTI_FRAME_POP] requested tid=<n> depth=1 frame=<index>` twice.
// `--compatible` must print the same. Wave 46 (lane L1), read from the code:
// on the wave-45 base every NotifyFramePop of an interpreter frame answered
// `OPAQUE_FRAME` (32) and no `FramePop` line printed, because
// `native_env::listed_rows` looked its interpreter frames up among the JNI
// natives' anchor positions (wave 44's merge); fixed in wave 46.
public class L1W45JvmtiNotifyFramePop {
    static native String rows(Thread other, int which);

    static native String events();

    static int caller(Thread other) {
        int r = 7;
        System.out.print(rows(other, 1));
        return r;
    }

    static void thrower(Thread other) {
        System.out.print(rows(other, 2));
        throw new IllegalStateException("out of thrower");
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W45JvmtiNotifyFramePop <path of the shim library>");
            return;
        }
        System.load(args[0]);
        Thread other = new Thread(() -> {
            try {
                Thread.sleep(600_000);
            } catch (InterruptedException stop) {
                // Ends with the program.
            }
        }, "other");
        other.setDaemon(true);
        other.start();
        int r = caller(other);
        try {
            thrower(other);
        } catch (IllegalStateException expected) {
            r++;
        }
        System.out.print(events());
        System.out.println("r=" + r);
    }
}
