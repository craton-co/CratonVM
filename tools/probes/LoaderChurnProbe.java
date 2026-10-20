import java.lang.reflect.Method;

/**
 * G1ChurnPauseProbe's allocation shape, run against a heap in which the three
 * class-loader registries are NON-EMPTY.
 *
 * <h2>Why this probe exists</h2>
 *
 * <p>`CRATONVM_G1_MARK_SIDE_TABLES` replaces three process-global registry
 * lookups per scanned object with one per-batch snapshot. Each of those three
 * registries ({@code loader_pin}, {@code mirror_pin}, {@code metadata_pin})
 * carries a {@code NON_EMPTY} latch, and the ONLY writer of
 * {@code loader_pin::set_loader_pin} in the tree is
 * {@code native-builtins/src/classloader.rs}'s {@code defineClass} path. A
 * program that never defines a class through a user {@code ClassLoader}
 * therefore leaves all three latches clear, every lookup is one relaxed atomic
 * load, and the flag has nothing to remove.
 *
 * <p>`w2c-what-landed-and-what-to-measure.md` says so itself: an A/B that
 * comes out identical "would mean the registries are empty in the workload
 * being measured and the probe is not exercising the path at all". This probe
 * is the arm that flips the latch, so a null result means something.
 *
 * <p>It defines {@code loaders} classes through a custom loader, keeps every
 * one of them reachable (an unreferenced loader could be unloaded and clear
 * the latch again), and then runs the churn. The class bytes are a minimal
 * valid class file assembled here rather than read from disk, so the probe has
 * no external inputs.
 *
 * <p>Usage: {@code LoaderChurnProbe <loaders> <liveMiB> <churnRounds>}
 */
public final class LoaderChurnProbe {

    static final class Node {
        Node next;
        final byte[] payload;
        int tag;
        Node(Node next, int payloadBytes, int tag) {
            this.next = next;
            this.payload = new byte[payloadBytes];
            this.tag = tag;
        }
    }

    /** A loader that defines one class from bytes we hand it. */
    static final class Defining extends ClassLoader {
        Defining() { super(LoaderChurnProbe.class.getClassLoader()); }
        Class<?> define(String name, byte[] b) { return defineClass(name, b, 0, b.length); }
    }

    /**
     * A minimal class file: `public class <name> { public static int v() { return <k>; } }`
     * assembled by hand so the probe needs no compiler and no resource files.
     */
    static byte[] classBytes(String name, int k) {
        // Constant pool entries (1-based):
        //  1 Utf8 name        2 Class #1
        //  3 Utf8 java/lang/Object  4 Class #3
        //  5 Utf8 "v"         6 Utf8 "()I"
        //  7 Utf8 "Code"
        java.io.ByteArrayOutputStream bo = new java.io.ByteArrayOutputStream();
        java.io.DataOutputStream d = new java.io.DataOutputStream(bo);
        try {
            d.writeInt(0xCAFEBABE);
            d.writeShort(0);          // minor
            d.writeShort(52);         // major (Java 8 — no StackMapTable needed for this body)
            d.writeShort(8);          // constant_pool_count = entries + 1
            d.writeByte(1); d.writeUTF(name.replace('.', '/'));  // #1
            d.writeByte(7); d.writeShort(1);                     // #2 Class
            d.writeByte(1); d.writeUTF("java/lang/Object");      // #3
            d.writeByte(7); d.writeShort(3);                     // #4 Class
            d.writeByte(1); d.writeUTF("v");                     // #5
            d.writeByte(1); d.writeUTF("()I");                   // #6
            d.writeByte(1); d.writeUTF("Code");                  // #7
            d.writeShort(0x0021);     // ACC_PUBLIC | ACC_SUPER
            d.writeShort(2);          // this_class
            d.writeShort(4);          // super_class
            d.writeShort(0);          // interfaces
            d.writeShort(0);          // fields
            d.writeShort(1);          // methods
            d.writeShort(0x0009);     // ACC_PUBLIC | ACC_STATIC
            d.writeShort(5);          // name "v"
            d.writeShort(6);          // descriptor "()I"
            d.writeShort(1);          // attributes
            d.writeShort(7);          // "Code"
            byte[] code = (k >= -1 && k <= 5)
                    ? new byte[] { (byte) (0x03 + k), (byte) 0xAC }        // iconst_<k>; ireturn
                    : new byte[] { 0x10, (byte) k, (byte) 0xAC };          // bipush k; ireturn
            d.writeInt(12 + code.length);
            d.writeShort(2);          // max_stack
            d.writeShort(0);          // max_locals
            d.writeInt(code.length);
            d.write(code);
            d.writeShort(0);          // exception_table
            d.writeShort(0);          // code attributes
            d.writeShort(0);          // class attributes
            d.flush();
        } catch (java.io.IOException e) {
            throw new AssertionError(e);
        }
        return bo.toByteArray();
    }

    public static void main(String[] args) throws Exception {
        int loaders = args.length > 0 ? Integer.parseInt(args[0]) : 8;
        int liveMiB = args.length > 1 ? Integer.parseInt(args[1]) : 32;
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 150;

        // RETAINED, deliberately: an unreachable loader may be unloaded, which
        // clears the NON_EMPTY latch and silently returns the run to the arm
        // this probe exists not to be.
        Object[] pinned = new Object[loaders * 2];
        long defined = 0;
        for (int i = 0; i < loaders; i++) {
            Defining dl = new Defining();
            String n = "w3c.Gen" + i;
            Class<?> c = dl.define(n, classBytes(n, i % 6));
            Method m = c.getMethod("v");
            defined += ((Integer) m.invoke(null)).intValue() + 1;
            pinned[i * 2] = dl;
            pinned[i * 2 + 1] = c;
        }

        final int nodeBytes = 256;
        final int liveNodes = (liveMiB * 1024 * 1024) / nodeBytes;
        Node head = null;
        for (int i = 0; i < liveNodes; i++) head = new Node(head, nodeBytes - 32, i);

        long checksum = 0;
        long start = System.nanoTime();
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < (4 * 1024 * 1024) / nodeBytes; i++) {
                Node dead = new Node(null, nodeBytes - 32, i);
                checksum += dead.payload.length + dead.tag;
            }
            Node cur = head; int walked = 0;
            while (cur != null && walked < 4096) {
                cur.tag = cur.tag + r; checksum += cur.tag; cur = cur.next; walked++;
            }
        }
        long wallMs = (System.nanoTime() - start) / 1_000_000L;
        // `pinned` is read here so nothing may collect it early.
        int pin = 0;
        for (Object o : pinned) if (o != null) pin++;
        System.out.println("LOADERCHURN_OK loaders=" + loaders + " defined=" + defined
                + " pinned=" + pin + " rounds=" + rounds
                + " wallMs=" + wallMs + " checksum=" + checksum);
    }
}
