import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;

import jdk.jfr.Recording;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordingFile;

/**
 * zs s2/sl2: what {@code jdk.ZRelocationSet} reports on a cycle whose slide
 * the collector declined.
 *
 * <p>The shape is Zs1SlSelectiveShapes' {@code stream}: a dense retained block
 * plus a garbage stream that keeps only its newest nodes. Its slides move
 * little and the relocation cost gate (and, with
 * {@code CRATONVM_ZGC_SELECTIVE_RELOCATE=2}, the cycle gate) declines many of
 * them. Before zs s2/sl2 a declined cycle still booked the selector's set
 * ({@code relocate > 0}); with {@code CRATONVM_ZGC_RELOCATION_SET_BOOKS_FINAL}
 * (default ON) it books what the slide relocated ({@code relocate == 0}).
 *
 * <p>stdout is deterministic (the retained-graph checksum and {@code ZS2SL2
 * ok}) and must equal HotSpot's. The event counts go to stderr only:
 * {@code ZS2SL2 events=N relocate_zero=Z relocate_nonzero=P}. HotSpot ZGC
 * books every cycle's set; the counts are for the A/B against the kill arm.
 *
 * <p>Usage: {@code Zs2Sl2RelocationSetJfr [rounds]}
 */
public final class Zs2Sl2RelocationSetJfr {

    static final class Node {
        Node next;
        final byte[] payload;
        final int tag;

        Node(Node next, int payloadBytes, int tag) {
            this.next = next;
            this.payload = new byte[payloadBytes];
            this.payload[0] = (byte) tag;
            this.tag = tag;
        }
    }

    static long walk(Node head) {
        long sum = 0;
        long n = 0;
        for (Node x = head; x != null; x = x.next) {
            sum = sum * 31 + x.tag + x.payload[0];
            n++;
        }
        return sum ^ (n << 40);
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 20;
        Path out = Files.createTempFile("zs2sl2", ".jfr");
        long check;
        try (Recording rec = new Recording()) {
            rec.enable("jdk.ZRelocationSet");
            rec.start();
            Node dense = null;
            for (int i = 0; i < 200_000; i++) {
                dense = new Node(dense, 48, i);
            }
            final int keep = 4096;
            Node[] ring = new Node[keep];
            long sum = 0;
            for (int r = 0; r < rounds; r++) {
                for (int i = 0; i < 400_000; i++) {
                    Node n = new Node(null, 96, i ^ r);
                    ring[i & (keep - 1)] = n;
                    sum += n.tag;
                }
                if ((r & 3) == 3) {
                    System.gc();
                }
            }
            Node head = null;
            for (int i = keep - 1; i >= 0; i--) {
                ring[i].next = head;
                head = ring[i];
            }
            check = walk(dense) * 3 + walk(head) + sum;
            rec.stop();
            rec.dump(out);
        }
        List<RecordedEvent> events = RecordingFile.readAllEvents(out);
        long zero = 0;
        long nonzero = 0;
        for (RecordedEvent e : events) {
            if (!"jdk.ZRelocationSet".equals(e.getEventType().getName())) {
                continue;
            }
            long relocate = e.hasField("relocate") ? e.getLong("relocate") : -1;
            if (relocate == 0) {
                zero++;
            } else {
                nonzero++;
            }
        }
        Files.deleteIfExists(out);
        System.out.println("ZS2SL2 stream rounds=" + rounds + " check=" + check);
        System.err.println("ZS2SL2 events=" + (zero + nonzero) + " relocate_zero=" + zero
                + " relocate_nonzero=" + nonzero);
        System.out.println("ZS2SL2 ok");
    }
}
