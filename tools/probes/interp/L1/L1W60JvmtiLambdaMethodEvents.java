// Interpreter round i2 wave 60, lane L1: the C JVMTI table's `MethodEntry`
// and `MethodExit` events for the methods of a lambda's class, against
// HotSpot (docs/internal/fixed-bugs/interpreter-L1-no-method-events-for-a-lambda-proxys-methods-FIXED-20261024.md).
//
// The shim's `Agent_OnLoad` adds `can_generate_method_entry_events` and
// `can_generate_method_exit_events` and sets both callbacks, which count,
// on the thread that called `start`, the events of a method declared by a
// lambda's class (its signature holds `$$Lambda`) and of a lambda body
// (`lambda$main$...`). `start` enables both events for that thread,
// `stop` disables them and prints the counts and the proxy methods' names.
// `main` runs a plain `Runnable` three times, then once more.
//
// Needs the native shim tools/probes/interp/L1/L1W60JvmtiLambdaMethodEvents.c,
// loaded both as an agent and with `System.load` (for the natives):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w60events.so tools/probes/interp/L1/L1W60JvmtiLambdaMethodEvents.c
//   javac -d /tmp/l1w60events tools/probes/interp/L1/L1W60JvmtiLambdaMethodEvents.java
//   java     -agentpath:/tmp/libl1w60events.so -cp /tmp/l1w60events L1W60JvmtiLambdaMethodEvents /tmp/libl1w60events.so
//   cratonvm -agentpath:/tmp/libl1w60events.so -cp /tmp/l1w60events L1W60JvmtiLambdaMethodEvents /tmp/libl1w60events.so
//
// Without the argument it prints only the usage line. The Rust port of the
// shim is tools/probes/interp/L1/rust-ports/L1W60JvmtiLambdaMethodEvents.
//
// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with the Rust
// port of the C shim, default flags, three runs, the same each time:
//
//   events: proxy entry=3 exit=3 body entry=3 exit=3 names=[ run run run]
//   events: proxy entry=1 exit=1 body entry=1 exit=1 names=[ run]
//
// CratonVM before wave 60 (MEASURED, debug build, both modes): every
// `proxy` count 0 and `names=[]` (the body rows as HotSpot's). Since wave 60
// (`CRATONVM_JVMTI_C_LAMBDA_FRAMES` with `CRATONVM_JVMTI_C_LAMBDA_EVENTS`):
// HotSpot's lines.
public class L1W60JvmtiLambdaMethodEvents {
    static native void start();
    static native String stop();

    public static void main(String[] args) {
        if (args.length != 1) {
            System.out.println("usage: L1W60JvmtiLambdaMethodEvents <path of the shim library>");
            return;
        }
        System.load(args[0]);
        Runnable plain = () -> { };
        start();
        plain.run();
        plain.run();
        plain.run();
        System.out.print(stop());
        start();
        plain.run();
        System.out.print(stop());
    }
}
