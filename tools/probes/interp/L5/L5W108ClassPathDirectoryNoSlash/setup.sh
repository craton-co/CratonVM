#!/bin/sh
# Interpreter round i5 wave 108, lane L5: JDK 25's `URLClassPath.getLoader(URL)`
# makes a `FileLoader` (a directory) only for a URL whose file ends with `/`;
# every other URL is a `JarLoader`, whose constructor opens it as a jar
# (`ensureOpen`), fails on a directory, and `getLoader` skips the URL. So a
# manifest `Class-Path: classes` (no slash) adds nothing, `extra/` adds the
# directory, and `dot/.` too (`URLStreamHandler.parseURL` folds a trailing
# `/.` to `/`). CratonVM made every existing directory a class path root
# (`classloading/src/class_path.rs` `process_classpath_token`); fixed by
# `ManifestInfo::token_is_slashless_directory` (not built or run).
#
# Builds out/L5W108DNS/main.jar and the directories classes/, extra/, dot/.
# Run from this directory with a JDK 25 javac, jar and java on PATH:
#
#   sh setup.sh
#   java -cp out/L5W108DNS/main.jar L5W108ClassPathDirectoryNoSlash
#   java -jar out/L5W108DNS/main.jar
#   cratonvm --jdk-only   [--nojit] -cp out/L5W108DNS/main.jar L5W108ClassPathDirectoryNoSlash
#   cratonvm --compatible [--nojit] -cp out/L5W108DNS/main.jar L5W108ClassPathDirectoryNoSlash
#   cratonvm --jdk-only   [--nojit] -jar out/L5W108DNS/main.jar
#   cratonvm --compatible [--nojit] -jar out/L5W108DNS/main.jar
#
# HOTSPOT_EXPECTED derived from the JDK 25 sources, not run (every command,
# Windows and Linux):
# HOTSPOT_EXPECTED_BEGIN
# c.C=java.lang.ClassNotFoundException: c.C
# e.E=found
# d.D=found
# c/c.txt=null
# e/e.txt=from-extra
# d/d.txt=from-dot
# HOTSPOT_EXPECTED_END
set -e
OUT=out/L5W108DNS
rm -rf "$OUT"
mkdir -p "$OUT/src/c" "$OUT/src/e" "$OUT/src/d" "$OUT/classes" "$OUT/extra" "$OUT/dot" "$OUT/main"
printf 'package c;\npublic class C { }\n' > "$OUT/src/c/C.java"
printf 'package e;\npublic class E { }\n' > "$OUT/src/e/E.java"
printf 'package d;\npublic class D { }\n' > "$OUT/src/d/D.java"
javac --release 8 -d "$OUT/classes" "$OUT/src/c/C.java"
javac --release 8 -d "$OUT/extra" "$OUT/src/e/E.java"
javac --release 8 -d "$OUT/dot" "$OUT/src/d/D.java"
printf 'from-classes' > "$OUT/classes/c/c.txt"
printf 'from-extra' > "$OUT/extra/e/e.txt"
printf 'from-dot' > "$OUT/dot/d/d.txt"
javac --release 11 -d "$OUT/main" L5W108ClassPathDirectoryNoSlash.java
printf 'Main-Class: L5W108ClassPathDirectoryNoSlash\nClass-Path: classes extra/ dot/.\n' > "$OUT/manifest.txt"
jar --create --file "$OUT/main.jar" --manifest "$OUT/manifest.txt" -C "$OUT/main" .
