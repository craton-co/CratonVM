// Interpreter round i2 wave 55, lane L1: what the C JVMTI table answers for
// a lambda's class, against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-does-not-know-the-vms-lambda-proxy-classes-FIXED-20261007.md).
//
// The shim's `Agent_OnLoad` adds `can_get_source_file_name`; the native
// `facts` asks eleven functions about one class: whether `GetLoadedClasses`
// lists it, `GetClassSignature` (every hex digit after `0x` printed as `h`:
// the suffix is an address on HotSpot, an id on CratonVM), `GetClassModifiers`,
// `GetClassLoader` (null / app / other), `GetImplementedInterfaces` (count and
// the first one's signature), `GetClassStatus`, `IsInterface` /
// `IsArrayClass`, `GetClassMethods` (count), `GetSourceFileName`,
// `GetClassVersionNumbers` (major) and whether `GetClassLoaderClasses` of the
// application loader lists it. `main` asks them of one `Runnable` lambda's
// class, then of a second lambda's (the first-call row again).
//
// Needs the native shim tools/probes/interp/L1/L1W55JvmtiLambdaClass.c, loaded
// both as an agent and with `System.load` (for the natives):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w55lambda.so tools/probes/interp/L1/L1W55JvmtiLambdaClass.c
//   javac -d /tmp/l1w55lambda tools/probes/interp/L1/L1W55JvmtiLambdaClass.java
//   java     -agentpath:/tmp/libl1w55lambda.so -cp /tmp/l1w55lambda L1W55JvmtiLambdaClass /tmp/libl1w55lambda.so
//   cratonvm -agentpath:/tmp/libl1w55lambda.so -cp /tmp/l1w55lambda L1W55JvmtiLambdaClass /tmp/libl1w55lambda.so
//
// Without the argument it prints only the usage line. The Rust port of the
// shim is tools/probes/interp/L1/rust-ports/L1W55JvmtiLambdaClass (README in
// tools/probes/interp/L1/rust-ports/).
//
// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with the Rust
// port of the C shim (same calls, same order, same output), three runs, the
// same each time:
//
//   first ran
//   onload: add=0
//   loaded classes: err=0 listed=1
//   signature: err=0 sig=LL1W55JvmtiLambdaClass$$Lambda.0xhhhhhhhhhhhhhhhh; generic=null
//   modifiers: err=0 value=0x1030
//   loader: err=0 kind=app
//   interfaces: err=0 count=1 first=Ljava/lang/Runnable;
//   status: err=0 value=7
//   kind: interface err=0 value=0 array err=0 value=0
//   methods: err=0 count=2
//   source file: err=101
//   version: err=0 major=69
//   app loader classes: err=0 listed=0
//   second ran
//   again:
//   (the eleven rows from `loaded classes:` again, the same)
//
// CratonVM before wave 55 (code read): `listed=0`, and every other row but
// the last `err=21` (JVMTI_ERROR_INVALID_CLASS: the VM mints a lambda's class
// outside the class store). Since wave 55 (`CRATONVM_JVMTI_C_LAMBDA_CLASSES`
// on, the default from the lane's last commit): HotSpot's lines (MEASURED on
// the Windows box with a debug `cratonvm` and the Rust port, both modes)
// except `methods: err=21 count=0` (the proxy has no
// `jmethodID`s; still open on the page). The `listed=1` row is the positive
// control (`jvmti::native_env::get_loaded_classes`); unit test
// `jvmti::native_env::tests::a_lambda_proxy_class_answers_the_class_functions`.
public class L1W55JvmtiLambdaClass {
    static native String facts(Class<?> klass, ClassLoader app);

    public static void main(String[] args) {
        if (args.length != 1) {
            System.out.println("usage: L1W55JvmtiLambdaClass <path of the shim library>");
            return;
        }
        System.load(args[0]);
        ClassLoader app = L1W55JvmtiLambdaClass.class.getClassLoader();
        Runnable first = () -> System.out.println("first ran");
        first.run();
        System.out.print(facts(first.getClass(), app));
        Runnable second = () -> System.out.println("second ran");
        second.run();
        System.out.println("again:");
        System.out.print(facts(second.getClass(), app));
    }
}
