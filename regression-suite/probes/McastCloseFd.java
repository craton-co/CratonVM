// H8-1 §1.4 / N3: does java.net.MulticastSocket.close() release its fd under
// net_close? H8-A found that sun/nio/ch/UnixDispatcher.close0 has always been
// served by net.rs's net_close (the deleted lib.rs registration never won),
// but which real bytecode route MulticastSocket.close() actually takes was
// left OPEN -- a source read cannot answer it, only a run.
//
// This does not read the fd table directly (no such accessor from Java); it
// opens, uses and closes many multicast sockets in a loop and relies on fd
// exhaustion as the observable: if close() is not releasing the OS fd, the
// loop hits "Too many open files" (or an unexpected exception) well before
// COUNT iterations on any ordinary ulimit -n. A clean run to completion is
// the positive signal.
//
// H8-1-three-declines-that-were-not-declines-20260820-RETIRED-20260921.md
import java.net.*;

public class McastCloseFd {
  static final int COUNT = 2000;

  public static void main(String[] a) throws Exception {
    InetAddress group = InetAddress.getByName("230.0.0.1");
    int leaked = 0;
    int joinLeaveFailures = 0;
    for (int i = 0; i < COUNT; i++) {
      MulticastSocket s = new MulticastSocket();
      try {
        // setLoopbackMode is deliberately NOT called: it is a separate,
        // unrelated native (measured to throw `InternalError: Should not
        // get here` on this VM, same as joinGroup below) and this probe's
        // only question is whether close() releases the fd, not whether
        // every MulticastSocket method is implemented.
        s.joinGroup(group);
        s.leaveGroup(group);
      } catch (Throwable joinErr) {
        // Catches both a legitimate IOException (no multicast route in a
        // sandboxed CI network namespace) and the unrelated InternalError
        // measured on this VM (spawned as a separate task) -- neither is
        // this probe's question, which close() below answers regardless.
        joinLeaveFailures++;
      } finally {
        s.close();
      }
      if (!s.isClosed()) {
        leaked++;
      }
    }
    System.out.println("iterations=" + COUNT + " isClosedFalseCount=" + leaked
        + " joinLeaveFailures=" + joinLeaveFailures);
    System.out.println("completed=true");
  }
}
