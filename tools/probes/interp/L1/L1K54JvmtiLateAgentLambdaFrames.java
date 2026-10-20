// The i54-L1 retirement: the frames of lambdas' classes listed by a C agent
// loaded into the running VM (`VirtualMachine.loadAgentPath`), against
// HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-does-not-know-the-vms-lambda-proxy-classes-FIXED-20261007.md).
//
// Two calls were made before the agent existed: a thread started with a
// lambda waits inside it, and a method reference ran 200000 times from a
// loop (hot, compiled). Once the agent is loaded, the waiting lambda and one
// more call through the hot loop list their stacks with `GetStackTrace`
// (each frame's class signature, masked, method name and location).
//
// Needs the native shim tools/probes/interp/L1/L1K54JvmtiLateAgentLambdaFrames.c
// (loaded with `System.load` for the natives, then as an agent) and
// `-Djdk.attach.allowAttachSelf=true`; the argument must be an absolute path:
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1k54late.so tools/probes/interp/L1/L1K54JvmtiLateAgentLambdaFrames.c
//   javac -d /tmp/l1k54late tools/probes/interp/L1/L1K54JvmtiLateAgentLambdaFrames.java
//   java     -Djdk.attach.allowAttachSelf=true -cp /tmp/l1k54late L1K54JvmtiLateAgentLambdaFrames /tmp/libl1k54late.so
//   cratonvm -Djdk.attach.allowAttachSelf=true -cp /tmp/l1k54late L1K54JvmtiLateAgentLambdaFrames /tmp/libl1k54late.so
//
// The Rust port of the shim is tools/probes/interp/L1/rust-ports/L1K54JvmtiLateAgentLambdaFrames.
// Expected output (HotSpot 25.0.3): at the end of the file.
import com.sun.tools.attach.VirtualMachine;

public class L1K54JvmtiLateAgentLambdaFrames {
    static native String stack();

    static volatile boolean attached;
    static volatile boolean report;
    static String waitingLine;
    static String hotLine;
    static int sink;

    static void waitThenReport() {
        while (!attached) {
            Thread.onSpinWait();
        }
        waitingLine = stack();
    }

    static void hotBody() {
        sink++;
        if (report) {
            hotLine = stack();
        }
    }

    static void loop(Runnable r, int n) {
        for (int i = 0; i < n; i++) {
            r.run();
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1K54JvmtiLateAgentLambdaFrames <absolute path of the shim>");
            return;
        }
        System.load(args[0]);
        Thread waiting = new Thread(() -> waitThenReport(), "waiting");
        waiting.start();
        Runnable hot = L1K54JvmtiLateAgentLambdaFrames::hotBody;
        loop(hot, 200_000);
        VirtualMachine vm = VirtualMachine.attach(String.valueOf(ProcessHandle.current().pid()));
        vm.loadAgentPath(args[0]);
        vm.detach();
        attached = true;
        waiting.join();
        System.out.println("waiting:\n" + waitingLine);
        report = true;
        loop(hot, 1);
        System.out.println("hot:\n" + hotLine);
    }
}

// Expected output (HotSpot 25.0.3, MEASURED with the Rust port, default flags):
//
//   waiting:
//     err=0 count=5
//     LL1K54JvmtiLateAgentLambdaFrames;.waitThenReport at 12
//     LL1K54JvmtiLateAgentLambdaFrames;.lambda$main$0 at 0
//     LL1K54JvmtiLateAgentLambdaFrames$$Lambda.0x?;.run at 0
//     Ljava/lang/Thread;.runWith at 5
//     Ljava/lang/Thread;.run at 19
//   hot:
//     err=0 count=4
//     LL1K54JvmtiLateAgentLambdaFrames;.hotBody at 14
//     LL1K54JvmtiLateAgentLambdaFrames$$Lambda.0x?;.run at 0
//     LL1K54JvmtiLateAgentLambdaFrames;.loop at 8
//     LL1K54JvmtiLateAgentLambdaFrames;.main at 106
