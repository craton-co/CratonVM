// Interpreter round i2 wave 57, lane L1: the C JVMTI table's
// `GetLocalVariableTable` (72), against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-lacks-the-functions-and-events-agents-call-first-FIXED-20261007.md).
//
// The shim's `Agent_OnLoad` adds `can_access_local_variables` (an
// OnLoad-only capability); the native `table` prints the local variable
// table of one method (`FromReflectedMethod`): each entry's start, length,
// slot, name, signature and generic signature. The methods are the JDK's
// own (its classes carry a `LocalVariableTable`): `Math.max(int, int)`, the
// generic `Objects.requireNonNull(T, String)`, the instance method
// `String.isEmpty()`; then a native method, an abstract one, and this
// class's own `main` (javac without `-g` writes no table). The native
// `errors` prints the refusals: a NULL method, the two NULL out-parameters,
// and an env obtained in the live phase, which cannot hold the capability.
// Twice (the first-call rows again).
//
// Needs the native shim tools/probes/interp/L1/L1W57JvmtiLocalVariableTable.c,
// loaded both as an agent and with `System.load` (for the natives):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w57lvt.so tools/probes/interp/L1/L1W57JvmtiLocalVariableTable.c
//   javac -d /tmp/l1w57lvt tools/probes/interp/L1/L1W57JvmtiLocalVariableTable.java
//   java     -agentpath:/tmp/libl1w57lvt.so -cp /tmp/l1w57lvt L1W57JvmtiLocalVariableTable /tmp/libl1w57lvt.so
//   cratonvm -agentpath:/tmp/libl1w57lvt.so -cp /tmp/l1w57lvt L1W57JvmtiLocalVariableTable /tmp/libl1w57lvt.so
//
// Without the argument it prints only the usage line. The Rust port of the
// shim is tools/probes/interp/L1/rust-ports/L1W57JvmtiLocalVariableTable
// (README in tools/probes/interp/L1/rust-ports/).
//
// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with the Rust
// port of the C shim (same calls, same order, same output), three runs, the
// same each time:
//
//   round 1
//   Math.max: onload=0 err=0 count=2
//     [0] start=0 length=11 slot=0 a I generic=null
//     [1] start=0 length=11 slot=1 b I generic=null
//   Objects.requireNonNull: onload=0 err=0 count=2
//     [0] start=0 length=15 slot=0 obj Ljava/lang/Object; generic=TT;
//     [1] start=0 length=15 slot=1 message Ljava/lang/String; generic=null
//   String.isEmpty: onload=0 err=0 count=1
//     [0] start=0 length=14 slot=0 this Ljava/lang/String; generic=null
//   Thread.currentThread: onload=0 err=104 count=0
//   Runnable.run: onload=0 err=101 count=0
//   main: onload=0 err=101 count=0
//   errors: null-method=23 null-count=100 null-table=100 live-env=0/99
//   round 2
//   (the same rows again)
//
// (The JDK rows read the JDK's own class files: a JDK update that rewrites
// one of those three methods moves its lengths.)
//
// CratonVM before wave 57 (code read): every row `err=98`
// (`JVMTI_ERROR_NOT_AVAILABLE`, slot 72 unimplemented), and the table
// answered `JVMTI_ERROR_NATIVE_METHOD` as 111 (`ACCESS_DENIED`) wherever it
// refused a native method. With slot 72 built (MEASURED, debug build, both
// modes), every row HotSpot's but `errors: null-method=0`: a NULL
// `jmethodID` named method 0 of class id 0, a stored class, in every
// method function of the table (fixed the same wave).
import java.lang.reflect.Executable;

public class L1W57JvmtiLocalVariableTable {
    static native String table(String label, Executable m);
    static native String errors(Executable m);

    static void round(long n) throws Exception {
        System.out.println("round " + n);
        System.out.print(table("Math.max", Math.class.getMethod("max", int.class, int.class)));
        System.out.print(table("Objects.requireNonNull",
                java.util.Objects.class.getMethod("requireNonNull", Object.class, String.class)));
        System.out.print(table("String.isEmpty", String.class.getMethod("isEmpty")));
        System.out.print(table("Thread.currentThread", Thread.class.getMethod("currentThread")));
        System.out.print(table("Runnable.run", Runnable.class.getMethod("run")));
        System.out.print(table("main", L1W57JvmtiLocalVariableTable.class.getMethod("main", String[].class)));
        System.out.print(errors(Math.class.getMethod("max", int.class, int.class)));
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W57JvmtiLocalVariableTable <path of the shim library>");
            return;
        }
        System.load(args[0]);
        round(1);
        round(2);
    }
}
