#!/bin/sh
# Interpreter round i5 wave 104, lane L5
# (docs/internal/fixed-bugs/interpreter-i100-L5-multi-release-jars-read-at-version-25-whatever-jdk-boots-FIXED-20261016.md,
# module-path residue). Builds a multi-release automatic-module jar
# out/L5W104MRMP/mp/mrv-1.0.jar and the class-path main class. Run from this
# directory with JDK 25's javac and java on PATH:
#
#   sh setup.sh
#   M=out/L5W104MRMP; C="-p $M/mp --add-modules mrv,vmi -cp $M/main L5W104MultiReleaseModulePath"
#   java $C                                   cratonvm --java-home <jdk25> [--compatible] $C
#   <jdk21>/bin/java $C                       cratonvm --java-home <jdk21> [--compatible] $C
#   java -Djdk.util.jar.version=21 $C         cratonvm --java-home <jdk25> [--compatible] -Djdk.util.jar.version=21 $C
#
# The jar holds p/A.class (base) and versioned-only classes in e (versions/8),
# f (versions/5), g (versions/09, zero-padded), t (versions/22).
# `ModulePath` reads module-path jars at `JarFile.runtimeVersion()`, and an
# automatic module's packages are those of `versionedStream()`'s class names,
# each kept only when `getJarEntry(base)` finds it: versions 8..=runtime,
# canonical decimals only.
#
# Expected (HotSpot):
#   JDK 25:                     feature=25 / mrv packages=[e, p, t]
#   JDK 21:                     feature=21 / mrv packages=[e, p]
#   JDK 25 -Djdk.util.jar.version=21: feature=25 / mrv packages=[e, p]
#   and on every row: vmi automatic=false exports=[w]
# vmi-1.0.jar is multi-release with its module-info.class ONLY under
# META-INF/versions/9/: `ModulePath.readJar` finds it with the versioned
# `getJarEntry`, so vmi is an explicit module. Before wave 104 CratonVM read
# only the base module-info.class and made vmi automatic
# (vmi automatic=true exports=[]).
# Before wave 104 CratonVM printed mrv packages=[e, f, g, p, t] on every row
# (any parseable version <= 25 counted, and the module path was read at 25
# whatever JDK booted).
set -e
OUT=out/L5W104MRMP
rm -rf "$OUT"
mkdir -p "$OUT/main" "$OUT/tool" "$OUT/mp"
javac --release 17 -d "$OUT/main" L5W104MultiReleaseModulePath.java
for pkg in p e f g t; do
  mkdir -p "$OUT/src/$pkg" "$OUT/cls/$pkg"
  printf 'package %s;\npublic class A {}\n' "$pkg" > "$OUT/src/$pkg/A.java"
  javac --release 17 -d "$OUT/cls/$pkg" "$OUT/src/$pkg/A.java"
done
cat > "$OUT/tool/MakeJar.java" <<'JAVA'
import java.io.*;
import java.nio.file.*;
import java.util.jar.*;

public class MakeJar {
    // args: <jar> <manifest attrs "K: V;K: V" or -> then pairs <entry name> <file>
    public static void main(String[] args) throws Exception {
        Manifest mf = new Manifest();
        mf.getMainAttributes().put(Attributes.Name.MANIFEST_VERSION, "1.0");
        if (!args[1].equals("-")) {
            for (String kv : args[1].split(";")) {
                String[] p = kv.split(": ", 2);
                mf.getMainAttributes().put(new Attributes.Name(p[0]), p[1]);
            }
        }
        try (JarOutputStream out = new JarOutputStream(new FileOutputStream(args[0]), mf)) {
            for (int i = 2; i + 1 < args.length; i += 2) {
                out.putNextEntry(new JarEntry(args[i]));
                out.write(Files.readAllBytes(Paths.get(args[i + 1])));
                out.closeEntry();
            }
        }
    }
}
JAVA
java "$OUT/tool/MakeJar.java" "$OUT/mp/mrv-1.0.jar" "Multi-Release: true" \
  p/A.class "$OUT/cls/p/p/A.class" \
  META-INF/versions/8/e/A.class "$OUT/cls/e/e/A.class" \
  META-INF/versions/5/f/A.class "$OUT/cls/f/f/A.class" \
  META-INF/versions/09/g/A.class "$OUT/cls/g/g/A.class" \
  META-INF/versions/22/t/A.class "$OUT/cls/t/t/A.class"
mkdir -p "$OUT/src/vmi/w" "$OUT/cls/vmi"
printf 'module vmi { exports w; }\n' > "$OUT/src/vmi/module-info.java"
printf 'package w;\npublic class A {}\n' > "$OUT/src/vmi/w/A.java"
javac --release 17 -d "$OUT/cls/vmi" "$OUT/src/vmi/module-info.java" "$OUT/src/vmi/w/A.java"
java "$OUT/tool/MakeJar.java" "$OUT/mp/vmi-1.0.jar" "Multi-Release: true" \
  w/A.class "$OUT/cls/vmi/w/A.class" \
  META-INF/versions/9/module-info.class "$OUT/cls/vmi/module-info.class"
echo "built $OUT"
