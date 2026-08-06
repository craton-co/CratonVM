import java.util.Optional;

/**
 * java.lang.ProcessHandle, from both directions: the static current() and the
 * one a live child hands back through Process.toHandle().
 *
 * ProcessHandle is an INTERFACE, so a VM that fabricates an instance of it by
 * name gets whatever field count the interface declares -- zero -- and any
 * state it tries to store on the object is dropped. The visible symptom is a
 * pid of 0.
 */
public class ProcHandleProbe {
    public static void main(String[] args) throws Exception {
        ProcessHandle self = ProcessHandle.current();
        long selfPid = self.pid();
        System.out.println("CURRENT_PID_POSITIVE=" + (selfPid > 0));
        System.out.println("CURRENT_PID_MATCHES_PID_API=" + (selfPid == ProcessHandle.current().pid()));
        System.out.println("CURRENT_IS_ALIVE=" + self.isAlive());

        Process p = Runtime.getRuntime().exec(new String[] { "/bin/sleep", "1" });
        ProcessHandle childHandle = p.toHandle();
        long childPid = childHandle.pid();
        System.out.println("CHILD_PID_POSITIVE=" + (childPid > 0));
        System.out.println("CHILD_PID_MATCHES_PROCESS_PID=" + (childPid == p.pid()));
        System.out.println("CHILD_PID_DIFFERS_FROM_SELF=" + (childPid != selfPid));
        System.out.println("CHILD_ALIVE_BEFORE_WAIT=" + childHandle.isAlive());
        System.out.println("WAIT=" + p.waitFor());
        System.out.println("CHILD_ALIVE_AFTER_WAIT=" + childHandle.isAlive());

        Optional<ProcessHandle> byPid = ProcessHandle.of(selfPid);
        System.out.println("OF_SELF_PRESENT=" + byPid.isPresent());
        System.out.println("OF_SELF_PID_ROUNDTRIPS=" + (byPid.isPresent() && byPid.get().pid() == selfPid));

        System.out.println("PROBE_DONE");
    }
}
