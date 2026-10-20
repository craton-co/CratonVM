// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 52, lane L5 -- `java.io.File`'s constructors
// under `--compatible`, a GUARD: the JDK normalises the path through the
// platform `FileSystem` (`normalize`, `prefixLength`). Written while chasing
// why a real `URLClassLoader` over a directory found nothing under
// `--compatible` on Windows (the `jdk-url-class-loaders` HotSpot-alignment
// row): the host-independent rows already matched in both modes; the
// Windows-only `drive-N` row did not until the wave's orchestrator follow-up
// (`docs/internal/fixed-bugs/interpreter-L5-compatible-file-keeps-a-slash-before-a-windows-drive-FIXED-20261016.md`).
// Separators are printed as `/` so the lines are the same on every host.
// Rows, first and again last:
//
//   path-N    `getPath()` of `a//b/`, `x/`, `./y//z`, and `File(File, String)`
//             of (`p/`, `/q/`).
//   uri-N     `new File(URI)` of a directory's `file:` URI with a trailing
//             slash: does it exist, and does a child of it exist.
//   drive-N   Windows only (`not windows` elsewhere). For a temp directory
//             `T` spelled `\T\`, `\\T`, `//T` (with `/`) and `\/T`: `P` when
//             `getPath()` is `T` (`normalizePrefix` drops every separator
//             before a drive), `E` when it exists. `root=`: `\C:\`'s path is
//             `C:\`, and `C:\`'s absolute path is `C:\`. `unc=`: a UNC share
//             root's canonical path ends without a separator. `bad=`:
//             `getCanonicalPath` of `C:\a*b` and `C:\a|b` throws
//             `IOException` (a wildcard; an invalid name in an existing
//             directory), of `C:\<missing>\a:b` does not.
//
// Run (no setup):
//   javac -d out L5W52CompatFileConstructors.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W52CompatFileConstructors
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim;
// on Linux the `drive-N` rows read `drive-N=not windows`):
//   path-1=[a/b] [x] [./y/z] [p/q]
//   uri-1=dir=true child=true
//   drive-1=PEPEPEPE root=truetrue unc=ok bad=IOE,IOE,ok
//   path-2=[a/b] [x] [./y/z] [p/q]
//   uri-2=dir=true child=true
//   drive-2=PEPEPEPE root=truetrue unc=ok bad=IOE,IOE,ok
//
// Before the follow-up, CratonVM on Windows printed
// `drive-N=pepepepe root=falsefalse unc=slash bad=ok,ok,ok` under
// `--compatible` and `drive-N=PEPEPEPE root=truetrue unc=slash bad=ok,ok,ok`
// under `--jdk-only` (the canonical-path rows are `canonicalize0`'s, shared
// by both modes).

import java.io.File;
import java.nio.file.Files;
import java.nio.file.Path;

public class L5W52CompatFileConstructors {
    static String p(File f) {
        return "[" + f.getPath().replace(File.separatorChar, '/') + "]";
    }

    static void round(int r) throws Exception {
        System.out.println("path-" + r + "=" + p(new File("a//b/")) + " " + p(new File("x/")) + " "
                + p(new File("./y//z")) + " " + p(new File(new File("p/"), "/q/")));
        Path dir = Files.createTempDirectory("l5w52f");
        Files.writeString(dir.resolve("c.txt"), "c");
        File d = new File(dir.toUri());
        File c = new File(new File(dir.toUri().getPath()), "c.txt");
        System.out.println("uri-" + r + "=dir=" + d.isDirectory() + " child=" + c.exists());
        System.out.println("drive-" + r + "=" + drive(dir));
    }

    /**
     * Windows only (one fixed line elsewhere): the drive-prefix rows of
     * `WinNTFileSystem.normalizePrefix` and `canonicalize0`, as booleans so
     * the line is the same on every Windows host.
     */
    static String drive(Path dir) {
        if (File.separatorChar != '\\') {
            return "not windows";
        }
        String t = dir.toAbsolutePath().toString();
        String bs = String.valueOf(File.separatorChar);
        StringBuilder sb = new StringBuilder();
        for (String s : new String[] {bs + t + bs, bs + bs + t, "//" + t.replace('\\', '/'), bs + "/" + t}) {
            File f = new File(s);
            sb.append(f.getPath().equals(t) ? 'P' : 'p').append(f.exists() ? 'E' : 'e');
        }
        String root = t.substring(0, 3);
        sb.append(" root=").append(new File(bs + root).getPath().equals(root))
                .append(new File(root).getAbsolutePath().equals(root));
        sb.append(" unc=").append(canon("//host/share/"));
        sb.append(" bad=").append(canon(root + "a*b")).append(',').append(canon(root + "a|b"))
                .append(',').append(canon(root + "nope-l5w52" + bs + "a:b").equals("ok") ? "ok" : "?");
        return sb.toString();
    }

    static String canon(String s) {
        try {
            String c = new File(s).getCanonicalPath();
            return c.endsWith("\\") ? "slash" : "ok";
        } catch (java.io.IOException e) {
            return "IOE";
        }
    }

    public static void main(String[] args) throws Exception {
        round(1);
        round(2);
    }
}
