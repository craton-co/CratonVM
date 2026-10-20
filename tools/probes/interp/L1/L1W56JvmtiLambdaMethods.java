// Interpreter round i2 wave 56, lane L1: what the C JVMTI table answers for
// the methods of a lambda's class, against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-does-not-know-the-vms-lambda-proxy-classes-FIXED-20261007.md).
//
// The shim's `Agent_OnLoad` adds `can_get_bytecodes`,
// `can_get_synthetic_attribute` and `can_get_line_numbers`; the native
// `methods` lists `GetClassMethods` of one class and, for each method, its
// name and signature, declaring class, modifiers, `IsMethodNative`,
// `IsMethodSynthetic`, max locals, arguments size, `GetMethodLocation`,
// `GetLineNumberTable` and `GetBytecodes` (size and first opcode). `main`
// asks it of a non-capturing `Runnable` lambda's class, a capturing
// `Function` lambda's, a serializable lambda's, then of a second plain
// lambda's (the first-call row again). The native `stack`, called from a
// lambda body, names the frames under it.
//
// Needs the native shim tools/probes/interp/L1/L1W56JvmtiLambdaMethods.c, loaded
// both as an agent and with `System.load` (for the natives):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w56methods.so tools/probes/interp/L1/L1W56JvmtiLambdaMethods.c
//   javac -d /tmp/l1w56methods tools/probes/interp/L1/L1W56JvmtiLambdaMethods.java
//   java     -agentpath:/tmp/libl1w56methods.so -cp /tmp/l1w56methods L1W56JvmtiLambdaMethods /tmp/libl1w56methods.so
//   cratonvm -agentpath:/tmp/libl1w56methods.so -cp /tmp/l1w56methods L1W56JvmtiLambdaMethods /tmp/libl1w56methods.so
//
// Without the argument it prints only the usage line. The Rust port of the
// shim is tools/probes/interp/L1/rust-ports/L1W56JvmtiLambdaMethods (README in
// tools/probes/interp/L1/rust-ports/).
//
// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with the Rust
// port of the C shim (same calls, same order, same output), three runs, the
// same each time:
//
//   plain ran
//   plain:
//   onload: add=0
//   methods: err=0 count=2
//     [0] name err=0 <init>()V generic=null
//     [0] declaring err=0 same=1 modifiers err=0 0x2 native err=0 0 synthetic err=0 0
//     [0] locals err=0 1 args err=0 1 location err=0 0..4 lines err=101 0 bytecodes err=0 5 first=42
//     [1] name err=0 run()V generic=null
//     [1] declaring err=0 same=1 modifiers err=0 0x1 native err=0 0 synthetic err=0 0
//     [1] locals err=0 1 args err=0 1 location err=0 0..3 lines err=101 0 bytecodes err=0 4 first=184
//   capturing ran p71
//   capturing:
//   methods: err=0 count=2
//     [0] name err=0 <init>(Ljava/lang/String;J)V generic=null
//     [0] declaring err=0 same=1 modifiers err=0 0x2 native err=0 0 synthetic err=0 0
//     [0] locals err=0 4 args err=0 4 location err=0 0..14 lines err=101 0 bytecodes err=0 15 first=42
//     [1] name err=0 apply(Ljava/lang/Object;)Ljava/lang/Object; generic=null
//     [1] declaring err=0 same=1 modifiers err=0 0x1 native err=0 0 synthetic err=0 0
//     [1] locals err=0 2 args err=0 2 location err=0 0..15 lines err=101 0 bytecodes err=0 16 first=42
//   serializable ran
//   serializable:
//   methods: err=0 count=3
//     [0] name err=0 <init>()V generic=null
//     [0] declaring err=0 same=1 modifiers err=0 0x2 native err=0 0 synthetic err=0 0
//     [0] locals err=0 1 args err=0 1 location err=0 0..4 lines err=101 0 bytecodes err=0 5 first=42
//     [1] name err=0 run()V generic=null
//     [1] declaring err=0 same=1 modifiers err=0 0x1 native err=0 0 synthetic err=0 0
//     [1] locals err=0 1 args err=0 1 location err=0 0..3 lines err=101 0 bytecodes err=0 4 first=184
//     [2] name err=0 writeReplace()Ljava/lang/Object; generic=null
//     [2] declaring err=0 same=1 modifiers err=0 0x12 native err=0 0 synthetic err=0 0
//     [2] locals err=0 1 args err=0 1 location err=0 0..29 lines err=101 0 bytecodes err=0 30 first=187
//   second ran
//   second:
//   methods: err=0 count=2
//     [0] name err=0 <init>()V generic=null
//     [0] declaring err=0 same=1 modifiers err=0 0x2 native err=0 0 synthetic err=0 0
//     [0] locals err=0 1 args err=0 1 location err=0 0..4 lines err=101 0 bytecodes err=0 5 first=42
//     [1] name err=0 run()V generic=null
//     [1] declaring err=0 same=1 modifiers err=0 0x1 native err=0 0 synthetic err=0 0
//     [1] locals err=0 1 args err=0 1 location err=0 0..3 lines err=101 0 bytecodes err=0 4 first=184
//   stack: err=0 count=4
//     frame 1: LL1W56JvmtiLambdaMethods; lambda$main$3
//     frame 2: LL1W56JvmtiLambdaMethods$$Lambda.0xhhhhhhhhhhhhhhhh; run
//     frame 3: LL1W56JvmtiLambdaMethods; main
//   stack: err=0 count=4
//     frame 1: LL1W56JvmtiLambdaMethods; lambda$main$3
//     frame 2: LL1W56JvmtiLambdaMethods$$Lambda.0xhhhhhhhhhhhhhhhh; run
//     frame 3: LL1W56JvmtiLambdaMethods; main
//
// CratonVM before wave 56 (MEASURED, debug build, both modes): `methods:
// err=21 count=0` for every lambda class (no `jmethodID`s for a proxy), and
// each `stack:` block `count=3` without the proxy's frame (frame 2 `main`).
// Since wave 56 (`CRATONVM_JVMTI_C_LAMBDA_METHODS` on, the default from the
// lane's last commit; MEASURED the same way): every row of HotSpot's except
// `location` and `bytecodes`, which answer `err=101 0..0` / `err=101 0
// first=-1` (the spun code is not modelled), and the `stack:` blocks, as
// before; both open on the lambda page. Since wave 57
// (`CRATONVM_JVMTI_C_LAMBDA_CODE`, on from the lane's last commit; MEASURED
// the same way, both modes): `location` and `bytecodes` too, from the code
// `InnerClassLambdaMetafactory` spins (`lambda_proxy::lambda_proxy_method_code`);
// only the `stack:` blocks still differ.
public class L1W56JvmtiLambdaMethods {
    static native String methods(Class<?> klass);
    static native String stack();

    public static void main(String[] args) {
        if (args.length != 1) {
            System.out.println("usage: L1W56JvmtiLambdaMethods <path of the shim library>");
            return;
        }
        System.load(args[0]);
        Runnable plain = () -> System.out.println("plain ran");
        plain.run();
        System.out.println("plain:");
        System.out.print(methods(plain.getClass()));
        String prefix = args[0].isEmpty() ? "?" : "p";
        long count = args.length;
        java.util.function.Function<Integer, String> capturing = x -> prefix + x + count;
        System.out.println("capturing ran " + capturing.apply(7));
        System.out.println("capturing:");
        System.out.print(methods(capturing.getClass()));
        Runnable serializable = (Runnable & java.io.Serializable) () -> System.out.println("serializable ran");
        serializable.run();
        System.out.println("serializable:");
        System.out.print(methods(serializable.getClass()));
        Runnable second = () -> System.out.println("second ran");
        second.run();
        System.out.println("second:");
        System.out.print(methods(second.getClass()));
        Runnable walker = () -> System.out.print(stack());
        walker.run();
        walker.run();
    }
}
