// Interpreter round i2 wave 60, lane L1: the frame of a lambda's class in the
// C JVMTI table's stack functions, against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-does-not-know-the-vms-lambda-proxy-classes-FIXED-20261007.md).
//
// The native `stack`, called from the body a lambda's class forwards to,
// lists `GetStackTrace` (each frame's class signature, masked, its method's
// name and signature, and its location), then `GetFrameCount` and
// `GetFrameLocation` of depth 2 (the frame under the body). Five shapes: a
// plain `Runnable` (first, and again last: the cold-VM first-call row), a
// `Supplier` capturing a `String`, a static method reference that unboxes
// its argument, a bound instance method reference, and a method reference
// to a lambda's own `run` (two lambda classes' frames, one above the other).
// The native `ids` (stage 2 of the proposal
// docs/internal/fixed-bugs/interpreter-i55-L1-proposal-lambda-proxies-answer-the-tool-interfaces-from-one-record-DONE-20261224.md)
// asks JNI's `GetMethodID` of a lambda's class for its `run`, its
// constructor and the inherited `toString`, names each answer's declaring
// class and modifiers through JVMTI, then calls `run` through its id.
//
// Needs the native shim tools/probes/interp/L1/L1W60JvmtiLambdaFrames.c,
// loaded both as an agent and with `System.load` (for the native):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w60frames.so tools/probes/interp/L1/L1W60JvmtiLambdaFrames.c
//   javac -d /tmp/l1w60frames tools/probes/interp/L1/L1W60JvmtiLambdaFrames.java
//   java     -agentpath:/tmp/libl1w60frames.so -cp /tmp/l1w60frames L1W60JvmtiLambdaFrames /tmp/libl1w60frames.so
//   cratonvm -agentpath:/tmp/libl1w60frames.so -cp /tmp/l1w60frames L1W60JvmtiLambdaFrames /tmp/libl1w60frames.so
//
// Without the argument it prints only the usage line. The Rust port of the
// shim is tools/probes/interp/L1/rust-ports/L1W60JvmtiLambdaFrames (README in
// tools/probes/interp/L1/rust-ports/).
//
// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with the Rust
// port of the C shim (same calls, same order, same output), default flags,
// three runs, the same each time:
//
//   plain:
//     stack: err=0 count=4
//       frame 1: LL1W60JvmtiLambdaFrames; lambda$main$0()V at 3
//       frame 2: LL1W60JvmtiLambdaFrames$$Lambda.0xhhhhhhhhhhhhhhhh; run()V at 0
//       frame 3: LL1W60JvmtiLambdaFrames; main([Ljava/lang/String;)V at 28
//     frame count: err=0 count=4
//     depth 2: err=0 LL1W60JvmtiLambdaFrames$$Lambda.0xhhhhhhhhhhhhhhhh; run()V at 0
//   jni call ran
//   ids:
//     run()V: found=1 declaring err=0 LL1W60JvmtiLambdaFrames$$Lambda.0xhhhhhhhhhhhhhhhh; own=1 modifiers err=0 0x1
//     <init>()V: found=1 declaring err=0 LL1W60JvmtiLambdaFrames$$Lambda.0xhhhhhhhhhhhhhhhh; own=1 modifiers err=0 0x2
//     toString()Ljava/lang/String;: found=1 declaring err=0 Ljava/lang/Object; own=0 modifiers err=0 0x1
//     call: pending=0
//   capturing:
//     stack: err=0 count=4
//       frame 1: LL1W60JvmtiLambdaFrames; lambda$main$2(Ljava/lang/String;)Ljava/lang/String; at 1
//       frame 2: LL1W60JvmtiLambdaFrames$$Lambda.0xhhhhhhhhhhhhhhhh; get()Ljava/lang/Object; at 4
//       frame 3: LL1W60JvmtiLambdaFrames; main([Ljava/lang/String;)V at 84
//     frame count: err=0 count=4
//     depth 2: err=0 LL1W60JvmtiLambdaFrames$$Lambda.0xhhhhhhhhhhhhhhhh; get()Ljava/lang/Object; at 4
//   unboxing:
//     stack: err=0 count=4
//       frame 1: LL1W60JvmtiLambdaFrames; twice(I)Ljava/lang/String; at 0
//       frame 2: LL1W60JvmtiLambdaFrames$$Lambda.0xhhhhhhhhhhhhhhhh; apply(Ljava/lang/Object;)Ljava/lang/Object; at 7
//       frame 3: LL1W60JvmtiLambdaFrames; main([Ljava/lang/String;)V at 112
//     frame count: err=0 count=4
//     depth 2: err=0 LL1W60JvmtiLambdaFrames$$Lambda.0xhhhhhhhhhhhhhhhh; apply(Ljava/lang/Object;)Ljava/lang/Object; at 7
//   bound:
//     stack: err=0 count=4
//       frame 1: LL1W60JvmtiLambdaFrames; instance()V at 7
//       frame 2: LL1W60JvmtiLambdaFrames$$Lambda.0xhhhhhhhhhhhhhhhh; run()V at 4
//       frame 3: LL1W60JvmtiLambdaFrames; main([Ljava/lang/String;)V at 146
//     frame count: err=0 count=4
//     depth 2: err=0 LL1W60JvmtiLambdaFrames$$Lambda.0xhhhhhhhhhhhhhhhh; run()V at 4
//   nested:
//     stack: err=0 count=5
//       frame 1: LL1W60JvmtiLambdaFrames; lambda$main$3()V at 3
//       frame 2: LL1W60JvmtiLambdaFrames$$Lambda.0xhhhhhhhhhhhhhhhh; run()V at 0
//       frame 3: LL1W60JvmtiLambdaFrames$$Lambda.0xhhhhhhhhhhhhhhhh; run()V at 4
//       frame 4: LL1W60JvmtiLambdaFrames; main([Ljava/lang/String;)V at 174
//     frame count: err=0 count=5
//     depth 2: err=0 LL1W60JvmtiLambdaFrames$$Lambda.0xhhhhhhhhhhhhhhhh; run()V at 0
//   jni call ran
//   ids again:
//     run()V: found=1 declaring err=0 LL1W60JvmtiLambdaFrames$$Lambda.0xhhhhhhhhhhhhhhhh; own=1 modifiers err=0 0x1
//     <init>()V: found=1 declaring err=0 LL1W60JvmtiLambdaFrames$$Lambda.0xhhhhhhhhhhhhhhhh; own=1 modifiers err=0 0x2
//     toString()Ljava/lang/String;: found=1 declaring err=0 Ljava/lang/Object; own=0 modifiers err=0 0x1
//     call: pending=0
//   plain:
//     stack: err=0 count=4
//       frame 1: LL1W60JvmtiLambdaFrames; lambda$main$0()V at 3
//       frame 2: LL1W60JvmtiLambdaFrames$$Lambda.0xhhhhhhhhhhhhhhhh; run()V at 0
//       frame 3: LL1W60JvmtiLambdaFrames; main([Ljava/lang/String;)V at 195
//     frame count: err=0 count=4
//     depth 2: err=0 LL1W60JvmtiLambdaFrames$$Lambda.0xhhhhhhhhhhhhhhhh; run()V at 0
//
// CratonVM before wave 60 (code read; wave 56 MEASURED the same gap with
// `L1W56JvmtiLambdaMethods`): no lambda class's frame in any block, so every
// count is one lower (two for `nested`) and depth 2 is the caller.
public class L1W60JvmtiLambdaFrames {
    static native String stack();
    static native String ids(Object lambda);

    static String twice(int x) {
        return stack();
    }

    private final String label;

    L1W60JvmtiLambdaFrames(String label) {
        this.label = label;
    }

    void instance() {
        System.out.print(label + ":\n" + stack());
    }

    public static void main(String[] args) {
        if (args.length != 1) {
            System.out.println("usage: L1W60JvmtiLambdaFrames <path of the shim library>");
            return;
        }
        System.load(args[0]);
        Runnable plain = () -> System.out.print("plain:\n" + stack());
        plain.run();
        Runnable viaJni = () -> System.out.println("jni call ran");
        System.out.print("ids:\n" + ids(viaJni));
        String prefix = args[0].isEmpty() ? "?" : "capturing:\n";
        java.util.function.Supplier<String> capturing = () -> prefix + stack();
        System.out.print(capturing.get());
        java.util.function.Function<Integer, String> unboxing = L1W60JvmtiLambdaFrames::twice;
        System.out.print("unboxing:\n" + unboxing.apply(21));
        Runnable bound = new L1W60JvmtiLambdaFrames("bound")::instance;
        bound.run();
        Runnable inner = () -> System.out.print("nested:\n" + stack());
        Runnable outer = inner::run;
        outer.run();
        System.out.print("ids again:\n" + ids(viaJni));
        plain.run();
    }
}
