#!/bin/sh
# Interpreter round i5 wave 100, lane L5
# (docs/internal/fixed-bugs/interpreter-i100-L5-multi-release-jars-read-at-version-25-whatever-jdk-boots-FIXED-20261016.md).
# Builds out/L5W100MR/mr.jar: a Multi-Release jar whose class mr.V answers
# the release directory it was served from: base, 9, 21, 22, 25 and 26.
# Each variant is compiled with --release 9 so any JDK >= 9 can load it.
# Run from this directory with a JDK 25 javac and java on PATH:
#
#   sh setup.sh
#   <java> -cp out/L5W100MR/mr.jar:out/L5W100MR/main L5W100MultiReleaseFeature
#   cratonvm --java-home <jdk> [--nojit] [--compatible] -cp out/L5W100MR/mr.jar:out/L5W100MR/main L5W100MultiReleaseFeature
#
# Expected (the launch class path's versioned view is the BOOTED library's
# feature version, `Runtime.version().feature()`):
#   HotSpot 25 (--java-home a JDK 25):   feature=25 class=25 resource=25
#   HotSpot 21 (--java-home a JDK 21):   feature=21 class=21 resource=21
# CratonVM over a JDK 21 is expected to answer class=25 resource=25 before a fix.
#
# Wave 103 (lane L5): `JarFile.<clinit>` caps the version with
# `-Djdk.util.jar.version` and `-Djdk.util.jar.enableMultiRelease=false` turns
# versioned entries off. Add the -D before the class path, same jar, JDK 25
# --java-home, both modes (HotSpot 25 derived from JarFile's static init):
#   -Djdk.util.jar.version=21                  feature=25 class=21 resource=21
#   -Djdk.util.jar.version=22                  feature=25 class=22 resource=22
#   -Djdk.util.jar.version=30                  feature=25 class=25 resource=25
#   -Djdk.util.jar.version=8                   feature=25 class=base resource=base
#   -Djdk.util.jar.enableMultiRelease=false    feature=25 class=base resource=base
# Before wave 103 CratonVM answered class=25 on every row (resource: the JDK's
# own JarFile under --jdk-only).
#
# Wave 101 (lane L5): the jar is written by a few lines of Java
# (java.util.jar.JarOutputStream) instead of `jar --create --release ...` plus
# a `jar --update` for version 26. That pair produced a jar HotSpot read as
# NOT multi-release (both JDKs printed class=base resource=base): `jar` checks
# and rewrites what it is given, and refuses a release above its own. The
# writer below puts exactly the manifest (Manifest-Version, Multi-Release:
# true) and the entries in, nothing else. The main class needs --release 10
# (`Runtime.version().feature()`).
set -e
OUT=out/L5W100MR
rm -rf "$OUT"
mkdir -p "$OUT/main" "$OUT/tool"
for v in base 9 21 22 25 26; do
  mkdir -p "$OUT/src/$v/mr" "$OUT/cls/$v"
  printf 'package mr;\npublic class V { public static String v() { return "%s"; } }\n' "$v" > "$OUT/src/$v/mr/V.java"
  javac --release 9 -d "$OUT/cls/$v" "$OUT/src/$v/mr/V.java"
  printf '%s' "$v" > "$OUT/cls/$v/mr/v.txt"
done
cat > "$OUT/tool/MakeMrJar.java" <<'JAVA'
import java.io.*;
import java.nio.file.*;
import java.util.jar.*;

public class MakeMrJar {
    // args: <jar> <cls dir>; versions base, 9, 21, 22, 25, 26 under <cls dir>/<v>/mr/.
    public static void main(String[] args) throws Exception {
        Manifest mf = new Manifest();
        mf.getMainAttributes().put(Attributes.Name.MANIFEST_VERSION, "1.0");
        mf.getMainAttributes().put(new Attributes.Name("Multi-Release"), "true");
        try (JarOutputStream out = new JarOutputStream(new FileOutputStream(args[0]), mf)) {
            for (String v : new String[] {"base", "9", "21", "22", "25", "26"}) {
                String prefix = v.equals("base") ? "" : "META-INF/versions/" + v + "/";
                for (String f : new String[] {"mr/V.class", "mr/v.txt"}) {
                    out.putNextEntry(new JarEntry(prefix + f));
                    out.write(Files.readAllBytes(Paths.get(args[1], v, f)));
                    out.closeEntry();
                }
            }
        }
    }
}
JAVA
java "$OUT/tool/MakeMrJar.java" "$OUT/mr.jar" "$OUT/cls"
javac --release 10 -cp "$OUT/cls/base" -d "$OUT/main" L5W100MultiReleaseFeature.java
