/**
 * gci i33/ms (2026-10-16): HotSpot 25's two metaspace spaces (page
 * {@code gci-i33ms-metaspace-spaces-and-limit-20261016}). stdout is
 * deterministic; the figures are on stderr.
 *
 * <ul>
 *   <li>{@code pools} (default): the {@code Metaspace} and
 *       {@code Compressed Class Space} pools' usage. stderr
 *       {@code [ms33] pools classes=<n> meta used=<u> committed=<c> max=<m> ccs used=<u> committed=<c> max=<m>};
 *       stdout {@code ms33 pools consistent} when the class space is
 *       populated and both committed figures are whole 64 KiB granules at
 *       or above used (HotSpot's shape), else {@code ms33 pools INCONSISTENT}.</li>
 *   <li>{@code jfr}: one {@code System.gc()} under a recording of
 *       {@code jdk.MetaspaceSummary}. stderr one {@code [ms33] jfr ...} line
 *       per row; stdout {@code ms33 jfr rows=true consistent} when every row's
 *       {@code dataSpace} + {@code classSpace} is its {@code metaspace}
 *       (used, committed, reserved), the class space is populated and its
 *       reservation is {@code CompressedClassSpaceSize} (1 GiB).</li>
 * </ul>
 */
import java.lang.management.ClassLoadingMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryUsage;
import java.nio.file.Files;
import java.nio.file.Path;
import jdk.jfr.Recording;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordedObject;
import jdk.jfr.consumer.RecordingFile;

public class GciMs33MetaspaceSpacesProbe {
    static final long GRANULE = 64L * 1024L;
    static final long CCS_SIZE = 1024L * 1024L * 1024L;

    static MemoryPoolMXBean pool(String name) {
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getName().equals(name)) {
                return p;
            }
        }
        return null;
    }

    static String describe(String label, MemoryUsage u) {
        if (u == null) {
            return label + " absent";
        }
        return label + " used=" + u.getUsed() + " committed=" + u.getCommitted() + " max=" + u.getMax();
    }

    static void pools() {
        ClassLoadingMXBean cl = ManagementFactory.getClassLoadingMXBean();
        MemoryPoolMXBean meta = pool("Metaspace");
        MemoryPoolMXBean ccs = pool("Compressed Class Space");
        // Warm the sampling path once so its own classes are in the figures.
        if (meta != null) {
            meta.getUsage();
        }
        if (ccs != null) {
            ccs.getUsage();
        }
        long classes = cl.getLoadedClassCount();
        MemoryUsage m = meta == null ? null : meta.getUsage();
        MemoryUsage c = ccs == null ? null : ccs.getUsage();
        System.err.println("[ms33] pools classes=" + classes + " " + describe("meta", m) + " "
                + describe("ccs", c));
        boolean ok = m != null && c != null
                && m.getCommitted() >= m.getUsed()
                && m.getCommitted() % GRANULE == 0
                && c.getUsed() > 0
                && c.getUsed() <= m.getUsed()
                && c.getCommitted() >= c.getUsed()
                && c.getCommitted() % GRANULE == 0
                && c.getMax() > 0;
        System.out.println("ms33 pools " + (ok ? "consistent" : "INCONSISTENT"));
    }

    static void jfr() throws Exception {
        Path file = Files.createTempFile("ms33-", ".jfr");
        try (Recording r = new Recording()) {
            r.enable("jdk.MetaspaceSummary");
            r.start();
            System.gc();
            r.stop();
            r.dump(file);
        }
        int rows = 0;
        boolean ok = true;
        for (RecordedEvent e : RecordingFile.readAllEvents(file)) {
            if (!e.getEventType().getName().equals("jdk.MetaspaceSummary")) {
                continue;
            }
            rows++;
            RecordedObject m = e.getValue("metaspace");
            RecordedObject d = e.getValue("dataSpace");
            RecordedObject c = e.getValue("classSpace");
            long mu = m.getLong("used");
            long mc = m.getLong("committed");
            long mr = m.getLong("reserved");
            long du = d.getLong("used");
            long dc = d.getLong("committed");
            long dr = d.getLong("reserved");
            long cu = c.getLong("used");
            long cc = c.getLong("committed");
            long cr = c.getLong("reserved");
            System.err.println("[ms33] jfr row=" + rows + " metaspace=" + mu + "/" + mc + "/" + mr
                    + " data=" + du + "/" + dc + "/" + dr + " class=" + cu + "/" + cc + "/" + cr
                    + " gcThreshold=" + e.getLong("gcThreshold"));
            ok = ok && cu > 0 && du + cu == mu && dc + cc == mc && dr + cr == mr && mc >= mu
                    && cc >= cu && cr == CCS_SIZE;
        }
        Files.deleteIfExists(file);
        System.out.println("ms33 jfr rows=" + (rows > 0) + " " + (ok ? "consistent" : "INCONSISTENT"));
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "pools";
        if (mode.equals("jfr")) {
            jfr();
        } else {
            pools();
        }
    }
}
