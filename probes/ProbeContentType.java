import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;

/**
 * `Files.probeContentType` across the extensions whose answers come from
 * different places, so a CratonVM run diffs cleanly against a HotSpot run on
 * the same machine.
 *
 * The split matters and is the reason this probe exists. On Windows the answer
 * comes from `HKEY_CLASSES_ROOT\<ext>` "Content Type" for most of these, but
 * `.js` has no registry entry at all and reaches `text/javascript` only through
 * `AbstractFileTypeDetector`'s fallback to `URLConnection.getFileNameMap()`.
 * The two sources also disagree where both have an opinion — `.zip` is
 * `application/x-zip-compressed` in the registry and `application/zip` in the
 * properties map, `.xml` is `text/xml` vs `application/xml` — so a run that
 * answers the properties value everywhere is NOT equivalent to HotSpot, even
 * though every line looks plausible.
 *
 * The last three lines pin the JDK's own edge cases: an unknown extension, a
 * name with no dot, a directory, and a file that does not exist (the probe is a
 * name lookup, so a missing file still answers).
 */
public final class ProbeContentType {

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("probect");
        String[] names = {"a.txt", "b.html", "c.png", "d.json", "e.pdf", "f.xml", "g.zip",
                          "h.js", "i.css", "j.unknownext", "noextension"};
        for (String n : names) {
            Path f = dir.resolve(n);
            Files.write(f, "hello".getBytes(StandardCharsets.UTF_8));
            System.out.println(pad(n) + probe(f));
            Files.deleteIfExists(f);
        }
        System.out.println(pad("<dir>") + probe(dir));
        System.out.println(pad("missing.txt") + probe(dir.resolve("missing.txt")));
        Files.deleteIfExists(dir);
        System.out.println("DONE");
    }

    private static String probe(Path f) {
        try {
            return "= " + Files.probeContentType(f);
        } catch (Throwable t) {
            if (System.getProperty("ct.stack") != null) {
                t.printStackTrace(System.out);
            }
            return "! " + t;
        }
    }

    private static String pad(String s) {
        StringBuilder b = new StringBuilder(s);
        while (b.length() < 16) {
            b.append(' ');
        }
        return b.append("  ").toString();
    }

    private ProbeContentType() {
    }
}
