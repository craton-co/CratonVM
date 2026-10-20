// The i54-L1 retirement: the C JVMTI table's class and method events for the
// classes the VM mints for lambdas, against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-does-not-know-the-vms-lambda-proxy-classes-FIXED-20261007.md).
//
// The shim records, between `start` and `stop`, every `ClassFileLoadHook`,
// `ClassLoad` and `ClassPrepare` of a lambda's class (its name or signature
// holds `$$Lambda`; enabled globally, as the first can only be), and, on the
// thread that called `start`, every `MethodEntry` / `MethodExit` of a method
// a lambda's class declares. `main` creates and calls a non-capturing method
// reference and a capturing lambda. The hidden-class suffix is masked.
//
// Needs the native shim tools/probes/interp/L1/L1K54JvmtiLambdaProxyEvents.c,
// loaded both as an agent and with `System.load`:
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1k54ev.so tools/probes/interp/L1/L1K54JvmtiLambdaProxyEvents.c
//   javac -d /tmp/l1k54ev tools/probes/interp/L1/L1K54JvmtiLambdaProxyEvents.java
//   java     -agentpath:/tmp/libl1k54ev.so -cp /tmp/l1k54ev L1K54JvmtiLambdaProxyEvents /tmp/libl1k54ev.so
//   cratonvm -agentpath:/tmp/libl1k54ev.so -cp /tmp/l1k54ev L1K54JvmtiLambdaProxyEvents /tmp/libl1k54ev.so
//
// The Rust port of the shim is tools/probes/interp/L1/rust-ports/L1K54JvmtiLambdaProxyEvents.
// Expected output (HotSpot 25.0.3): at the end of the file.
import java.util.function.Supplier;

public class L1K54JvmtiLambdaProxyEvents {
    static native void start();

    static native String stop();

    static int counter;

    static void hit() {
        counter++;
    }

    public static void main(String[] args) {
        if (args.length != 1) {
            System.out.println("usage: L1K54JvmtiLambdaProxyEvents <path of the shim library>");
            return;
        }
        System.load(args[0]);
        start();
        Runnable plain = L1K54JvmtiLambdaProxyEvents::hit;
        plain.run();
        String text = "x";
        Supplier<String> capturing = () -> text + counter;
        capturing.get();
        System.out.print(stop());
    }
}

// Expected output (HotSpot 25.0.3, MEASURED with the Rust port; ClassPrepare before ClassLoad, no ClassFileLoadHook):
//
//   prepare LL1K54JvmtiLambdaProxyEvents$$Lambda.0x?;
//   load LL1K54JvmtiLambdaProxyEvents$$Lambda.0x?;
//   entry LL1K54JvmtiLambdaProxyEvents$$Lambda.0x?;.<init>
//   exit LL1K54JvmtiLambdaProxyEvents$$Lambda.0x?;.<init>
//   entry LL1K54JvmtiLambdaProxyEvents$$Lambda.0x?;.run
//   exit LL1K54JvmtiLambdaProxyEvents$$Lambda.0x?;.run
//   prepare LL1K54JvmtiLambdaProxyEvents$$Lambda.0x?;
//   load LL1K54JvmtiLambdaProxyEvents$$Lambda.0x?;
//   entry LL1K54JvmtiLambdaProxyEvents$$Lambda.0x?;.<init>
//   exit LL1K54JvmtiLambdaProxyEvents$$Lambda.0x?;.<init>
//   entry LL1K54JvmtiLambdaProxyEvents$$Lambda.0x?;.get
//   exit LL1K54JvmtiLambdaProxyEvents$$Lambda.0x?;.get
