import java.io.*;
import java.nio.file.*;
import java.nio.file.attribute.FileTime;
import java.util.*;
import java.util.jar.*;
import java.util.zip.*;

/**
 * HotSpot-vs-CratonVM parity vector for ZipEntry timestamp accessors.
 *
 * The JDK's ZipOutputStream writes the 0x5455 "extended timestamp" extra
 * field twice: the LOCAL header copy carries every time it was given
 * (modified/access/creation), while the CENTRAL directory copy carries only
 * the modified time. ZipFile/JarFile read the central directory; only
 * ZipInputStream reads local headers. So the two read paths legitimately
 * disagree, and any bridge must reproduce that disagreement exactly.
 */
public class ZipTimeParity {
    static final FileTime MOD = FileTime.fromMillis(1609459200000L); // 2021-01-01
    static final FileTime ACC = FileTime.fromMillis(1640995200000L); // 2022-01-01
    static final FileTime CRE = FileTime.fromMillis(1672531200000L); // 2023-01-01

    static void ck(String k, Object v) {
        System.out.println("CK " + k + " " + v);
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("ziptimeparity");
        File jar = dir.resolve("vector.jar").toFile();

        Manifest mf = new Manifest();
        mf.getMainAttributes().put(Attributes.Name.MANIFEST_VERSION, "1.0");
        try (JarOutputStream out = new JarOutputStream(new FileOutputStream(jar), mf)) {
            JarEntry all = new JarEntry("all-three.txt");
            all.setLastModifiedTime(MOD);
            all.setLastAccessTime(ACC);
            all.setCreationTime(CRE);
            out.putNextEntry(all);
            out.write("all-three".getBytes("UTF-8"));
            out.closeEntry();

            JarEntry modOnly = new JarEntry("mod-only.txt");
            modOnly.setLastModifiedTime(MOD);
            out.putNextEntry(modOnly);
            out.write("mod-only".getBytes("UTF-8"));
            out.closeEntry();

            JarEntry none = new JarEntry("no-times.txt");
            out.putNextEntry(none);
            out.write("no-times".getBytes("UTF-8"));
            out.closeEntry();
        }

        // ---- stage 1: what ZipFile (central directory) reports ----
        try (ZipFile zf = new ZipFile(jar)) {
            for (String n : new String[] {"all-three.txt", "mod-only.txt", "no-times.txt"}) {
                ZipEntry e = zf.getEntry(n);
                ck("zipfile.getEntry." + n + ".mod", e.getLastModifiedTime());
                ck("zipfile.getEntry." + n + ".acc", e.getLastAccessTime());
                ck("zipfile.getEntry." + n + ".cre", e.getCreationTime());
            }
        }

        // ---- stage 2: JarFile.entries() ----
        try (JarFile jf = new JarFile(jar)) {
            List<String> names = new ArrayList<>();
            Map<String, JarEntry> byName = new HashMap<>();
            for (Enumeration<JarEntry> en = jf.entries(); en.hasMoreElements(); ) {
                JarEntry e = en.nextElement();
                names.add(e.getName());
                byName.put(e.getName(), e);
            }
            Collections.sort(names);
            ck("jarfile.entries.names", names);
            for (String n : new String[] {"all-three.txt", "mod-only.txt", "no-times.txt"}) {
                JarEntry e = byName.get(n);
                ck("jarfile.entries." + n + ".mod", e.getLastModifiedTime());
                ck("jarfile.entries." + n + ".acc", e.getLastAccessTime());
                ck("jarfile.entries." + n + ".cre", e.getCreationTime());
            }
            JarEntry ge = jf.getJarEntry("all-three.txt");
            ck("jarfile.getJarEntry.all-three.mod", ge.getLastModifiedTime());
            ck("jarfile.getJarEntry.all-three.acc", ge.getLastAccessTime());
            ck("jarfile.getJarEntry.all-three.cre", ge.getCreationTime());
        }

        // ---- stage 3: ZipInputStream (local headers) ----
        try (ZipInputStream zis = new ZipInputStream(new FileInputStream(jar))) {
            ZipEntry e;
            while ((e = zis.getNextEntry()) != null) {
                if (!e.getName().equals("all-three.txt") && !e.getName().equals("mod-only.txt")) {
                    continue;
                }
                ck("zipstream." + e.getName() + ".mod", e.getLastModifiedTime());
                ck("zipstream." + e.getName() + ".acc", e.getLastAccessTime());
                ck("zipstream." + e.getName() + ".cre", e.getCreationTime());
            }
        }

        // ---- stage 4: raw extra-field bytes, local vs central ----
        byte[] raw = Files.readAllBytes(jar.toPath());
        ck("raw.local.all-three", hexExtraAtLocal(raw, "all-three.txt"));
        ck("raw.central.all-three", hexExtraAtCentral(raw, "all-three.txt"));

        // ---- stage 5: getTime()/setTime round trip ----
        try (ZipFile zf = new ZipFile(jar)) {
            ZipEntry e = zf.getEntry("all-three.txt");
            ck("zipfile.all-three.getTime", e.getTime());
            ck("zipfile.no-times.hasExtra", zf.getEntry("no-times.txt").getExtra() != null);
        }

        System.out.println("PASS ZipTimeParity");
    }

    static String hex(byte[] b, int off, int len) {
        StringBuilder sb = new StringBuilder();
        for (int i = off; i < off + len && i < b.length; i++) {
            sb.append(String.format("%02x", b[i] & 0xff));
        }
        return sb.toString();
    }

    /** Scan for the local file header whose name matches, return its extra bytes hex. */
    static String hexExtraAtLocal(byte[] b, String name) {
        byte[] nb = name.getBytes(java.nio.charset.StandardCharsets.UTF_8);
        for (int i = 0; i + 30 < b.length; i++) {
            if (b[i] == 0x50 && b[i + 1] == 0x4b && b[i + 2] == 0x03 && b[i + 3] == 0x04) {
                int nlen = u16(b, i + 26);
                int elen = u16(b, i + 28);
                if (nlen == nb.length && regionMatches(b, i + 30, nb)) {
                    return hex(b, i + 30 + nlen, elen);
                }
            }
        }
        return "<not found>";
    }

    /** Scan for the central directory record whose name matches. */
    static String hexExtraAtCentral(byte[] b, String name) {
        byte[] nb = name.getBytes(java.nio.charset.StandardCharsets.UTF_8);
        for (int i = 0; i + 46 < b.length; i++) {
            if (b[i] == 0x50 && b[i + 1] == 0x4b && b[i + 2] == 0x01 && b[i + 3] == 0x02) {
                int nlen = u16(b, i + 28);
                int elen = u16(b, i + 30);
                if (nlen == nb.length && regionMatches(b, i + 46, nb)) {
                    return hex(b, i + 46 + nlen, elen);
                }
            }
        }
        return "<not found>";
    }

    static boolean regionMatches(byte[] b, int off, byte[] want) {
        if (off + want.length > b.length) {
            return false;
        }
        for (int i = 0; i < want.length; i++) {
            if (b[off + i] != want[i]) {
                return false;
            }
        }
        return true;
    }

    static int u16(byte[] b, int off) {
        return (b[off] & 0xff) | ((b[off + 1] & 0xff) << 8);
    }
}
