// Interpreter round i2 wave 57, lane L1: the C JVMTI table's `GetErrorName`
// (128) of every code from 0 to 120, against HotSpot.
//
// The native `names` prints, per code, the name `GetErrorName` answers, or
// the error it refuses the code with (`JVMTI_ERROR_ILLEGAL_ARGUMENT`, 103,
// for a code that names no error), then `GetErrorName` with a NULL
// out-parameter. Twice (the first-call rows again).
//
// Needs the native shim tools/probes/interp/L1/L1W57JvmtiErrorNames.c, loaded
// both as an agent and with `System.load` (for the native):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w57errors.so tools/probes/interp/L1/L1W57JvmtiErrorNames.c
//   javac -d /tmp/l1w57errors tools/probes/interp/L1/L1W57JvmtiErrorNames.java
//   java     -agentpath:/tmp/libl1w57errors.so -cp /tmp/l1w57errors L1W57JvmtiErrorNames /tmp/libl1w57errors.so
//   cratonvm -agentpath:/tmp/libl1w57errors.so -cp /tmp/l1w57errors L1W57JvmtiErrorNames /tmp/libl1w57errors.so
//
// Without the argument it prints only the usage line. The Rust port of the
// shim is tools/probes/interp/L1/rust-ports/L1W57JvmtiErrorNames (README in
// tools/probes/interp/L1/rust-ports/).
//
// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with the Rust
// port of the C shim (same calls, same order, same output), two runs, the
// same each time:
//
//   0..120, one row each, in order: every code below names its error;
//   every other code is `<code> err=103`:
//   0 JVMTI_ERROR_NONE
//   10 JVMTI_ERROR_INVALID_THREAD
//   11 JVMTI_ERROR_INVALID_THREAD_GROUP
//   12 JVMTI_ERROR_INVALID_PRIORITY
//   13 JVMTI_ERROR_THREAD_NOT_SUSPENDED
//   14 JVMTI_ERROR_THREAD_SUSPENDED
//   15 JVMTI_ERROR_THREAD_NOT_ALIVE
//   20 JVMTI_ERROR_INVALID_OBJECT
//   21 JVMTI_ERROR_INVALID_CLASS
//   22 JVMTI_ERROR_CLASS_NOT_PREPARED
//   23 JVMTI_ERROR_INVALID_METHODID
//   24 JVMTI_ERROR_INVALID_LOCATION
//   25 JVMTI_ERROR_INVALID_FIELDID
//   26 JVMTI_ERROR_INVALID_MODULE
//   31 JVMTI_ERROR_NO_MORE_FRAMES
//   32 JVMTI_ERROR_OPAQUE_FRAME
//   34 JVMTI_ERROR_TYPE_MISMATCH
//   35 JVMTI_ERROR_INVALID_SLOT
//   40 JVMTI_ERROR_DUPLICATE
//   41 JVMTI_ERROR_NOT_FOUND
//   50 JVMTI_ERROR_INVALID_MONITOR
//   51 JVMTI_ERROR_NOT_MONITOR_OWNER
//   52 JVMTI_ERROR_INTERRUPT
//   60 JVMTI_ERROR_INVALID_CLASS_FORMAT
//   61 JVMTI_ERROR_CIRCULAR_CLASS_DEFINITION
//   62 JVMTI_ERROR_FAILS_VERIFICATION
//   63 JVMTI_ERROR_UNSUPPORTED_REDEFINITION_METHOD_ADDED
//   64 JVMTI_ERROR_UNSUPPORTED_REDEFINITION_SCHEMA_CHANGED
//   65 JVMTI_ERROR_INVALID_TYPESTATE
//   66 JVMTI_ERROR_UNSUPPORTED_REDEFINITION_HIERARCHY_CHANGED
//   67 JVMTI_ERROR_UNSUPPORTED_REDEFINITION_METHOD_DELETED
//   68 JVMTI_ERROR_UNSUPPORTED_VERSION
//   69 JVMTI_ERROR_NAMES_DONT_MATCH
//   70 JVMTI_ERROR_UNSUPPORTED_REDEFINITION_CLASS_MODIFIERS_CHANGED
//   71 JVMTI_ERROR_UNSUPPORTED_REDEFINITION_METHOD_MODIFIERS_CHANGED
//   72 JVMTI_ERROR_UNSUPPORTED_REDEFINITION_CLASS_ATTRIBUTE_CHANGED
//   73 JVMTI_ERROR_UNSUPPORTED_OPERATION
//   79 JVMTI_ERROR_UNMODIFIABLE_CLASS
//   80 JVMTI_ERROR_UNMODIFIABLE_MODULE
//   98 JVMTI_ERROR_NOT_AVAILABLE
//   99 JVMTI_ERROR_MUST_POSSESS_CAPABILITY
//   100 JVMTI_ERROR_NULL_POINTER
//   101 JVMTI_ERROR_ABSENT_INFORMATION
//   102 JVMTI_ERROR_INVALID_EVENT_TYPE
//   103 JVMTI_ERROR_ILLEGAL_ARGUMENT
//   104 JVMTI_ERROR_NATIVE_METHOD
//   106 JVMTI_ERROR_CLASS_LOADER_UNSUPPORTED
//   110 JVMTI_ERROR_OUT_OF_MEMORY
//   111 JVMTI_ERROR_ACCESS_DENIED
//   112 JVMTI_ERROR_WRONG_PHASE
//   113 JVMTI_ERROR_INTERNAL
//   115 JVMTI_ERROR_UNATTACHED_THREAD
//   116 JVMTI_ERROR_INVALID_ENVIRONMENT
//   null out: err=100
//   again:
//   (the same 122 rows again)
//
// CratonVM before wave 57 (MEASURED, debug build): `err=103` for 26, 72,
// 73, 79, 80, 102, 104 and 106, which `runtime::jvmti::JvmtiError` does not
// list.
public class L1W57JvmtiErrorNames {
    static native String names();

    public static void main(String[] args) {
        if (args.length != 1) {
            System.out.println("usage: L1W57JvmtiErrorNames <path of the shim library>");
            return;
        }
        System.load(args[0]);
        System.out.print(names());
        System.out.println("again:");
        System.out.print(names());
    }
}
