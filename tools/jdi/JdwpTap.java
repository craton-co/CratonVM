// JDWP logging proxy (interpreter round i1 wave 23, lane L1): sits between a
// debugger and a debuggee and writes every packet, both ways, to a log, so
// the commands a JDI session sends — and what HotSpot answers them — can be
// read off one run. It is how the wave-23 walk of a JDI line-breakpoint
// session found the commands CratonVM did not serve
// (docs/internal/fixed-bugs/interpreter-L1-jdi-cannot-set-a-line-breakpoint-jdwp-lacks-the-generic-commands-FIXED-20260926.md).
//
//   javac -d out tools/jdi/JdwpTap.java
//   # a debuggee listening on 5731 (HotSpot:
//   #   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5731 ...;
//   # CratonVM, experimental-debug build: cratonvm --jdwp-port 5731 ...)
//   java -cp out JdwpTap 5732 5731 session.log      # accepts one debugger on 5732
//   # then attach the debugger to 5732 instead of 5731
//
// Log lines: `>> cmd#<id> <set>/<cmd> [<len>] <hex>` for a command from the
// debugger, `<< reply#<id> (<set>/<cmd>) err=<code> [<len>] <hex>` for its
// reply, and `<< cmd#<id> 64/100 ...` for an event the debuggee sends
// (payloads are cut at 400 bytes). Summarise with
//   grep -o '>> cmd#[0-9]* [0-9]*/[0-9]*' session.log | awk '{print $3}' | sort | uniq -c
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.OutputStream;
import java.io.PrintStream;
import java.net.ServerSocket;
import java.net.Socket;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;

public class JdwpTap {
    /** Command packet id -> "set/cmd", to label its reply. */
    static final Map<Integer, String> PENDING = new ConcurrentHashMap<>();
    static PrintStream log;

    public static void main(String[] args) throws Exception {
        if (args.length != 3) {
            System.err.println("usage: JdwpTap <listen-port> <debuggee-port> <log-file>");
            System.exit(2);
        }
        log = new PrintStream(new FileOutputStream(args[2]), true);
        try (ServerSocket server = new ServerSocket(Integer.parseInt(args[0]))) {
            Socket debugger = server.accept();
            Socket debuggee = new Socket("localhost", Integer.parseInt(args[1]));
            byte[] handshake = new byte[14];
            new DataInputStream(debugger.getInputStream()).readFully(handshake);
            debuggee.getOutputStream().write(handshake);
            new DataInputStream(debuggee.getInputStream()).readFully(handshake);
            debugger.getOutputStream().write(handshake);
            Thread down = new Thread(() -> pump(debugger, debuggee, ">>"));
            Thread up = new Thread(() -> pump(debuggee, debugger, "<<"));
            down.start();
            up.start();
            down.join();
            up.join();
        }
    }

    static void pump(Socket from, Socket to, String dir) {
        try {
            DataInputStream in = new DataInputStream(from.getInputStream());
            OutputStream out = to.getOutputStream();
            while (true) {
                int length = in.readInt();
                byte[] rest = new byte[length - 4];
                in.readFully(rest);
                ByteArrayOutputStream packet = new ByteArrayOutputStream();
                new DataOutputStream(packet).writeInt(length);
                packet.write(rest);
                out.write(packet.toByteArray());
                out.flush();
                DataInputStream p = new DataInputStream(new ByteArrayInputStream(rest));
                int id = p.readInt();
                int flags = p.readUnsignedByte();
                synchronized (log) {
                    if ((flags & 0x80) != 0) {
                        int error = p.readUnsignedShort();
                        byte[] data = p.readAllBytes();
                        log.println(dir + " reply#" + id + " (" + PENDING.remove(id) + ") err="
                                + error + " " + hex(data));
                    } else {
                        int set = p.readUnsignedByte();
                        int cmd = p.readUnsignedByte();
                        byte[] data = p.readAllBytes();
                        PENDING.put(id, set + "/" + cmd);
                        log.println(dir + " cmd#" + id + " " + set + "/" + cmd + " " + hex(data));
                    }
                }
            }
        } catch (IOException closed) {
            synchronized (log) {
                log.println(dir + " closed: " + closed);
            }
            try {
                to.shutdownOutput();
            } catch (IOException ignored) {
                // Already closed.
            }
        }
    }

    static String hex(byte[] data) {
        StringBuilder sb = new StringBuilder("[" + data.length + "]");
        for (int i = 0; i < Math.min(data.length, 400); i++) {
            sb.append(String.format(" %02x", data[i]));
        }
        if (data.length > 400) {
            sb.append(" ...");
        }
        return sb.toString();
    }
}
