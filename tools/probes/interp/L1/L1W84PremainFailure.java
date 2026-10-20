// Interpreter round i4 wave 84, lane L1: a startup -javaagent starts as
// JPLIS starts it (InstrumentationImpl.loadClassAndCallPremain): agentArgs is
// null when the command line gives no options, and a premain that throws ends
// the VM with JPLIS's lines and an abort. Both modes.
//
// The class is its own agent (Premain-Class) and its own main class:
//   mkdir -p /tmp/l1w84pm && cd /tmp/l1w84pm
//   javac -d . $REPO/tools/probes/interp/L1/L1W84PremainFailure.java
//   printf 'Premain-Class: L1W84PremainFailure\n' > m.txt
//   jar cfm l1w84pm.jar m.txt L1W84PremainFailure.class
//
// Case A, no options:
//   java -javaagent:l1w84pm.jar -cp l1w84pm.jar L1W84PremainFailure; echo "rc=$?"
// Expected (HotSpot 25; derived from InvocationAdapter.c
// commandStringIntoJavaStrings, which passes NULL for absent options):
//   premain args=null
//   main ran
//   rc=0
// (CratonVM printed "premain args=" before: an empty String.)
//
// Case B, a premain that throws:
//   java -javaagent:l1w84pm.jar=throw -cp l1w84pm.jar L1W84PremainFailure >out.txt 2>err.txt; echo "rc=$?"
// Expected stdout (out.txt):
//   premain args=throw
//   FATAL ERROR in native method: processing of -javaagent failed, processJavaStart failed
//   ...then, on HotSpot only, the thread's native frames (addresses; not compared)
// Expected stderr (err.txt), compared line by line where marked =:
//   = Exception in thread "main" java.lang.reflect.InvocationTargetException
//     \tat java.base/... reflection and InstrumentationImpl frames (not compared)
//   = Caused by: java.lang.RuntimeException: boom
//     \tat L1W84PremainFailure.premain(L1W84PremainFailure.java:53) and more frames (not compared)
//   = *** java.lang.instrument ASSERTION FAILED ***: "!errorOutstanding" with message Outstanding error when calling method in invokeJavaAgentMainMethod at src/java.instrument/share/native/libinstrument/JPLISAgent.c line: 627
//   = *** java.lang.instrument ASSERTION FAILED ***: "success" with message invokeJavaAgentMainMethod failed at src/java.instrument/share/native/libinstrument/JPLISAgent.c line: 466
//   = *** java.lang.instrument ASSERTION FAILED ***: "result" with message agent load/premain call failed at src/java.instrument/share/native/libinstrument/JPLISAgent.c line: 422
// Lines 627 and 466 were MEASURED on HotSpot 25.0.4 (the attach path, same C
// functions); 422 is DERIVED -- refresh it from $J/java and fix
// JPLIS_PREMAIN_FAILURE_LINES in vm/src/runtime/instrument.rs if it differs.
// Compare with:
//   grep -E '^(Exception in thread|Caused by:|\*\*\* java.lang.instrument)' err.txt
// Exit status: Linux/macOS rc=134 (os::abort -> ::abort(), SIGABRT; the shell
// may add "Aborted (core dumped)"), Windows rc=1 (os::abort ->
// ExitProcess(1)). No hs_err file on either.
//
// Before the fix CratonVM printed one line, "Error: javaagent premain aborted
// VM: premain of `L1W84PremainFailure` threw: java/lang/RuntimeException",
// with no trace, and exited 1.
//
// CRATONVM_ATTACH_AGENTMAIN_JDK=0 restores the old dispatch for this path too.
public class L1W84PremainFailure {
    public static void premain(String args) {
        System.out.println("premain args=" + args);
        if ("throw".equals(args)) {
            throw new RuntimeException("boom");
        }
    }

    public static void main(String[] args) {
        System.out.println("main ran");
    }
}
