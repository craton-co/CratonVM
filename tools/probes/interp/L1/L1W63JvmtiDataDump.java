// Interpreter round i2 wave 63, lane L1: JVMTI `DataDumpRequest` (71), posted
// when the VM gets `SIGQUIT` (Linux; Windows' `Ctrl-Break` is item 4 of
// docs/internal/fixed-bugs/interpreter-i59-L1-sigquit-terminates-the-vm-FIXED-20261115.md).
//
// The startup agent tools/probes/interp/L1/L1W63JvmtiDataDump.c enables the
// event in `Agent_OnLoad` and counts it; this program sends itself `SIGQUIT`
// with `kill -QUIT` and waits up to five seconds for the count. Both VMs
// also print a thread dump on `SIGQUIT`, so only the `row:` lines are
// compared:
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w63dump.so tools/probes/interp/L1/L1W63JvmtiDataDump.c
//   javac -d /tmp/l1w63dump tools/probes/interp/L1/L1W63JvmtiDataDump.java
//   java     -agentpath:/tmp/libl1w63dump.so -cp /tmp/l1w63dump L1W63JvmtiDataDump /tmp/libl1w63dump.so | grep '^row:'
//   cratonvm -agentpath:/tmp/libl1w63dump.so -cp /tmp/l1w63dump L1W63JvmtiDataDump /tmp/libl1w63dump.so | grep '^row:'
//
// Expected `row:` lines (HotSpot 25): see the block at the end of this file.
public class L1W63JvmtiDataDump {
    static native int dumps();

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W63JvmtiDataDump <path of the shim library>");
            return;
        }
        System.load(args[0]);
        System.out.println("row: before QUIT: " + dumps());
        long pid = ProcessHandle.current().pid();
        new ProcessBuilder("kill", "-QUIT", Long.toString(pid)).inheritIO().start().waitFor();
        long deadline = System.nanoTime() + 5_000_000_000L;
        while (dumps() == 0 && System.nanoTime() < deadline) {
            Thread.sleep(20);
        }
        System.out.println("row: after QUIT: " + dumps());
        System.out.println("row: alive: true");
    }
}

// Expected `row:` lines (HotSpot 25.0.4): MEASURED on the Linux host, three
// runs, the same each time; CratonVM (wave 63, both modes) the same:
//
//   row: before QUIT: 0
//   row: after QUIT: 1
//   row: alive: true
