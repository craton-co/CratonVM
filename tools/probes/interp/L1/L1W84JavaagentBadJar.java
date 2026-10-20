// Interpreter round i4 wave 84, lane L1: a startup -javaagent whose JAR is
// unusable stops the VM during initialization, as HotSpot 25 does; the
// application's main never runs. Both modes.
//
// Run (from a scratch directory holding no file named l1w84-missing.jar):
//   javac -d . L1W84JavaagentBadJar.java
//   java -javaagent:l1w84-missing.jar -cp . L1W84JavaagentBadJar > out.txt 2> err.txt; echo "rc=$?"
//   cratonvm --jdk-only   -javaagent:l1w84-missing.jar -cp . L1W84JavaagentBadJar > out.txt 2> err.txt; echo "rc=$?"
//   cratonvm --compatible -javaagent:l1w84-missing.jar -cp . L1W84JavaagentBadJar > out.txt 2> err.txt; echo "rc=$?"
//
// Expected (derived from the JDK sources: InvocationAdapter.c Agent_OnLoad
// prints its line on stderr and returns JNI_ERR; jvmtiAgent.cpp
// invoke_Agent_OnLoad calls vm_exit_during_initialization("agent library
// failed Agent_OnLoad", "instrument"), whose lines go to tty = stdout):
//   out.txt:
//     Error occurred during initialization of VM
//     agent library failed Agent_OnLoad: instrument
//   err.txt:
//     Error opening zip file or JAR manifest missing : l1w84-missing.jar
//   rc=1
//
// Second case, a JAR without Premain-Class (build it with
//   printf 'Manifest-Version: 1.0\n' > m.txt; jar cfm l1w84-nopremain.jar m.txt L1W84JavaagentBadJar.class):
//   java -javaagent:l1w84-nopremain.jar -cp . L1W84JavaagentBadJar
//   out.txt: the same two lines; err.txt:
//     Failed to find Premain-Class manifest attribute in l1w84-nopremain.jar
//   rc=1
//
// Before the fix CratonVM printed "Warning: ignoring -javaagent:...: cannot
// read agent JAR ..." on stderr, ran main ("main ran" on stdout) and exited 0.
public class L1W84JavaagentBadJar {
    public static void main(String[] args) {
        System.out.println("main ran");
    }
}
