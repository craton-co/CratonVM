// The i54-L1 retirement: the residuals of
// docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-does-not-know-the-vms-lambda-proxy-classes-FIXED-20261007.md
// that wave 60 left, against HotSpot.
//
//   fields   `GetClassFields` of a lambda's class (its `arg$N` captures),
//            each field's name, signature, modifiers, declaring class and
//            `IsFieldSynthetic`, and the captured value read through JNI
//            with the JVMTI field id (`Get<Type>Field`)
//   locals   from the body, the local-variable functions at the depth of
//            the lambda class's frame (depth 2, 3 under a lambda body): `GetLocalInstance`,
//            `GetLocalObject` of slot 0 (the same object as the lambda?),
//            the SAM arguments by type and with the wrong type, and
//            `GetLocalVariableTable` of that frame's method
//   hot      the frame count and depth-2 method from a body a hot loop calls
//            (the compiled caller's lambda fast path), against the cold call
//
// Needs the native shim tools/probes/interp/L1/L1K54JvmtiLambdaProxyLocalsAndFields.c,
// loaded both as an agent and with `System.load` (for the natives):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1k54.so tools/probes/interp/L1/L1K54JvmtiLambdaProxyLocalsAndFields.c
//   javac -d /tmp/l1k54 tools/probes/interp/L1/L1K54JvmtiLambdaProxyLocalsAndFields.java
//   java     -agentpath:/tmp/libl1k54.so -cp /tmp/l1k54 L1K54JvmtiLambdaProxyLocalsAndFields /tmp/libl1k54.so
//   cratonvm -agentpath:/tmp/libl1k54.so -cp /tmp/l1k54 L1K54JvmtiLambdaProxyLocalsAndFields /tmp/libl1k54.so
//
// The Rust port of the shim is tools/probes/interp/L1/rust-ports/L1K54JvmtiLambdaProxyLocalsAndFields.
// Expected output (HotSpot 25.0.3): at the end of the file.
import java.util.function.Function;
import java.util.function.IntBinaryOperator;
import java.util.function.Supplier;

public class L1K54JvmtiLambdaProxyLocalsAndFields {
    static native String fields(Object lambda);

    static native String locals(Object lambda, Object arg, int depth);

    static native String frames();

    static Object current;
    static Object currentArg;
    static volatile boolean report;
    static String hotLine;
    static int sink;

    static void plainBody() {
        System.out.println("  " + locals(current, null, 2).replace("\n", "\n  "));
    }

    static String capturingBody(String s, long n) {
        System.out.println("  " + locals(current, null, 3).replace("\n", "\n  "));
        return s + n;
    }

    static String boxedBody(Integer i) {
        System.out.println("  " + locals(current, currentArg, 2).replace("\n", "\n  "));
        return "v" + i;
    }

    static int intsBody(int a, int b) {
        System.out.println("  " + locals(current, null, 2).replace("\n", "\n  "));
        return a + b;
    }

    static void hotBody() {
        sink++;
        if (report) {
            hotLine = frames();
        }
    }

    static void loop(Runnable r, int n) {
        for (int i = 0; i < n; i++) {
            if (i == n - 1) {
                report = true;
            }
            r.run();
        }
        report = false;
    }

    public static void main(String[] args) {
        if (args.length < 1) {
            System.out.println("usage: L1K54JvmtiLambdaProxyLocalsAndFields <path of the shim>");
            return;
        }
        System.load(args[0]);
        String text = "cap";
        long number = 42L;

        Runnable plain = L1K54JvmtiLambdaProxyLocalsAndFields::plainBody;
        System.out.println("plain fields: " + fields(plain).replace("\n", "\n  "));
        current = plain;
        System.out.println("plain locals:");
        plain.run();

        Supplier<String> capturing = () -> capturingBody(text, number);
        System.out.println("capturing fields: " + fields(capturing).replace("\n", "\n  "));
        current = capturing;
        System.out.println("capturing locals:");
        capturing.get();

        Function<Integer, String> boxed = L1K54JvmtiLambdaProxyLocalsAndFields::boxedBody;
        Integer argument = 1234;
        current = boxed;
        currentArg = argument;
        System.out.println("boxed locals:");
        boxed.apply(argument);

        IntBinaryOperator ints = L1K54JvmtiLambdaProxyLocalsAndFields::intsBody;
        current = ints;
        System.out.println("ints locals:");
        sink += ints.applyAsInt(7, 35);

        StringBuilder bound = new StringBuilder("b");
        Runnable boundRef = bound::reverse;
        System.out.println("bound fields: " + fields(boundRef).replace("\n", "\n  "));

        Runnable hot = L1K54JvmtiLambdaProxyLocalsAndFields::hotBody;
        loop(hot, 1);
        System.out.println("cold: " + hotLine);
        loop(hot, 200_000);
        System.out.println("hot: " + hotLine);
        loop(hot, 200_000);
        System.out.println("hot again: " + hotLine);
    }
}

// Expected output (HotSpot 25.0.3, MEASURED with the Rust port, default flags and -Xint, four runs, the same each time):
//
//   plain fields: err=0 count=0
//   plain locals:
//     depth 2: err=0 run()V at 0
//     instance: err=0 same=1
//     slot 0 object: err=0 same=1
//     slot 1 object: err=35 arg=0
//     slot 1 int: err=35 value=0
//     slot 2 int: err=35 value=0
//     slot 9 object: err=35
//     variable table: err=101 count=0
//   capturing fields: err=0 count=2
//     [0] name err=0 arg$1 Ljava/lang/String; generic=null modifiers err=0 0x12 declaring err=0 own=1 synthetic err=0 0 value="cap"
//     [1] name err=0 arg$2 J generic=null modifiers err=0 0x12 declaring err=0 own=1 synthetic err=0 0 value=42
//   capturing locals:
//     depth 3: err=0 get()Ljava/lang/Object; at 8
//     instance: err=0 same=1
//     slot 0 object: err=0 same=1
//     slot 1 object: err=35 arg=0
//     slot 1 int: err=35 value=0
//     slot 2 int: err=35 value=0
//     slot 9 object: err=35
//     variable table: err=101 count=0
//   boxed locals:
//     depth 2: err=0 apply(Ljava/lang/Object;)Ljava/lang/Object; at 4
//     instance: err=0 same=1
//     slot 0 object: err=0 same=1
//     slot 1 object: err=0 arg=1
//     slot 1 int: err=34 value=0
//     slot 2 int: err=35 value=0
//     slot 9 object: err=35
//     variable table: err=101 count=0
//   ints locals:
//     depth 2: err=0 applyAsInt(II)I at 2
//     instance: err=0 same=1
//     slot 0 object: err=0 same=1
//     slot 1 object: err=34 arg=0
//     slot 1 int: err=0 value=7
//     slot 2 int: err=0 value=35
//     slot 9 object: err=35
//     variable table: err=101 count=0
//   bound fields: err=0 count=1
//     [0] name err=0 arg$1 Ljava/lang/StringBuilder; generic=null modifiers err=0 0x12 declaring err=0 own=1 synthetic err=0 0 value=a Ljava/lang/StringBuilder;
//   cold: count err=0 5; depth 2 err=0 run()V at 0; depth 3 err=0 loop(Ljava/lang/Runnable;I)V
//   hot: count err=0 5; depth 2 err=0 run()V at 0; depth 3 err=0 loop(Ljava/lang/Runnable;I)V
//   hot again: count err=0 5; depth 2 err=0 run()V at 0; depth 3 err=0 loop(Ljava/lang/Runnable;I)V
