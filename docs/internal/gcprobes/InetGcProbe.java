import java.net.InetAddress;
import java.net.UnknownHostException;
import java.util.ArrayList;
import java.util.List;

public class InetGcProbe {
    static byte[] bytes16(String hex) {
        hex = hex.replace(":", "");
        byte[] out = new byte[16];
        for (int i = 0; i < 16; i++) {
            out[i] = (byte) Integer.parseInt(hex.substring(i * 2, i * 2 + 2), 16);
        }
        return out;
    }

    static void report(String label, InetAddress ia) {
        byte[] b = ia.getAddress();
        StringBuilder hex = new StringBuilder();
        for (byte x : b) hex.append(String.format("%02x", x));
        System.out.println(label + ": toString=" + ia + " hostAddress=" + ia.getHostAddress()
                + " getAddress.len=" + b.length + " getAddress.hex=" + hex);
    }

    public static void main(String[] args) throws UnknownHostException {
        // Two 16-byte IPv6 patterns matching the shape from the known-issue doc.
        byte[] b1 = bytes16("d147bc96ffffffffda222d9affffffff");
        byte[] b2 = bytes16("00000000000000000000000000000000".substring(0, 32)); // ::

        InetAddress a1 = InetAddress.getByAddress(b1);
        InetAddress a2 = InetAddress.getByAddress(b2);

        System.out.println("=== immediately after construction ===");
        report("a1", a1);
        report("a2", a2);

        // Hold references in a list so they survive (aren't collected), but
        // force a bunch of allocation + explicit GC to encourage a moving
        // young-gen collector to relocate them if it's going to.
        List<Object> garbage = new ArrayList<>();
        for (int round = 0; round < 20; round++) {
            for (int i = 0; i < 200_000; i++) {
                garbage.add(new byte[64]);
            }
            garbage.clear();
            System.gc();
        }

        System.out.println("=== after GC churn ===");
        report("a1", a1);
        report("a2", a2);

        // Also construct fresh addresses with the SAME byte patterns AFTER
        // the churn, to make sure the natives still work post-GC for brand
        // new objects (isolates "old object's mapping went stale" from "the
        // native is broken outright").
        InetAddress a1b = InetAddress.getByAddress(b1);
        InetAddress a2b = InetAddress.getByAddress(b2);
        System.out.println("=== fresh objects constructed AFTER GC churn ===");
        report("a1b", a1b);
        report("a2b", a2b);

        // Two-arg getByAddress(String, byte[]) overload -- check whether this
        // path (potentially NOT natively overridden, falling to real bytecode)
        // produces correct results even without any GC churn.
        InetAddress a3 = InetAddress.getByAddress("somehost", b1);
        System.out.println("=== two-arg getByAddress(String,byte[]) ===");
        report("a3", a3);
    }
}
