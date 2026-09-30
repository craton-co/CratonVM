// Interpreter round i1 wave 11, lane L3.
//
// The call shape of Tomcat BCEL's `ClassParser.parse() -> readInterfaces()`,
// which `jit_direct_call_requires_dispatch` kept off direct JIT-to-JIT calls
// BY NAME from 2026-07-16 (`TOMCAT-SILENT-HANG.5`: "a direct call into the
// readInterfaces scan edge corrupts a later allocation header") until wave 11
// removed the name rule. The general fix for that mechanism (a GC inside a raw
// direct call lost the caller's roots: `1ee92e3fd`, 2026-07-19) landed three
// days after the rule; this probe exercises the same shape under allocation
// pressure so a regression shows up as a wrong checksum or a crash:
//
//   * a private instance callee with only `this`, reached by a statically
//     bound call from a compiled caller;
//   * the callee allocates a reference array, publishes it into a field of
//     the receiver, then fills it in a loop whose every iteration calls an
//     allocating static helper;
//   * the caller allocates again straight after the call returns and reads
//     the array the callee published.
//
// Expected output (deterministic; computed from the arithmetic below, the
// orchestrator's HotSpot 25 run is the reference):
//   parsed=200000 interfaces=1630261 checksum=-5910653170072031882
// Run under --compatible with and without --nojit; both must match HotSpot.
// Timing goes to stderr.
public class L7DirectCallFieldArrayFill {
    static final String[] EMPTY = new String[0];

    static final class Parsed {
        final String name;
        final String[] interfaces;

        Parsed(String name, String[] interfaces) {
            this.name = name;
            this.interfaces = interfaces;
        }

        long checksum() {
            long h = name.hashCode();
            for (String s : interfaces) {
                h = h * 31 + s.hashCode();
            }
            return h;
        }
    }

    static final class Parser {
        private final int[] data;
        private int pos;
        private String className;
        private String[] interfaceNames;

        Parser(int[] data, int start) {
            this.data = data;
            this.pos = start;
        }

        private int readUnsignedShort() {
            int v = data[pos % data.length];
            pos++;
            return v & 0xffff;
        }

        private static String nameOf(int index) {
            // Allocates: a StringBuilder, its buffer and the result.
            return new StringBuilder(24).append("pkg.Iface").append(index % 977).toString();
        }

        private void readClassInfo() {
            className = nameOf(readUnsignedShort() + 100000);
        }

        private void readInterfaces() {
            final int count = readUnsignedShort() % 17;
            if (count > 0) {
                interfaceNames = new String[count];
                for (int i = 0; i < count; i++) {
                    final int index = readUnsignedShort();
                    interfaceNames[i] = nameOf(index);
                }
            } else {
                interfaceNames = EMPTY;
            }
        }

        Parsed parse() {
            readClassInfo();
            readInterfaces();
            // Allocate right after the callee returns, then read what it
            // published.
            return new Parsed(className, interfaceNames);
        }
    }

    public static void main(String[] args) {
        int[] data = new int[4099];
        int seed = 12345;
        for (int i = 0; i < data.length; i++) {
            seed = seed * 1103515245 + 12345;
            data[i] = (seed >>> 8) & 0xffff;
        }
        long t0 = System.nanoTime();
        long checksum = 0;
        long interfaces = 0;
        int parsed = 0;
        Object[] churn = new Object[64];
        for (int round = 0; round < 200000; round++) {
            Parsed p = new Parser(data, round * 7).parse();
            interfaces += p.interfaces.length;
            checksum = checksum * 1000003 + p.checksum();
            // Garbage so that collections land inside the calls above.
            churn[round & 63] = new byte[256 + (round & 1023)];
            parsed++;
        }
        System.out.println("parsed=" + parsed + " interfaces=" + interfaces + " checksum=" + checksum);
        System.err.println("elapsed_ms=" + (System.nanoTime() - t0) / 1_000_000);
    }
}
