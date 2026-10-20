#!/bin/sh
# Interpreter round i5 wave 108, lane L5: a manifest `Class-Path` naming one
# jar as `dep.jar`, `sub/../dep.jar` and `./dep.jar`. JDK 25's
# `JarLoader.parseClassPath` makes each token a URL with `new URL(base,
# token)`, whose `URLStreamHandler.parseURL` drops `.` segments and folds
# `name/..` pairs lexically; `URLClassPath.getLoader` skips a URL that already
# has a loader (`lmap`), so the jar is ONE loader and every resource URL is
# spelled without dot segments. CratonVM joined the tokens to the jar's
# directory as spelled and compared entries by path, so the jar was three
# entries (`getResources` answered three URLs, two spelled with `..` / `.`).
# Fixed by `classloading/src/class_path.rs` `url_path_normalized` (not built
# or run). `sub/` exists so the Linux kernel opens `sub/../dep.jar` too
# (Windows folds `..` itself).
#
# Builds out/L5W108DS/{main,dep}.jar. Run from this directory with a JDK 25
# javac, jar and java on PATH:
#
#   sh setup.sh
#   java -cp out/L5W108DS/main.jar L5W108ClassPathDotSegments
#   java -jar out/L5W108DS/main.jar
#   cratonvm --jdk-only   [--nojit] -cp out/L5W108DS/main.jar L5W108ClassPathDotSegments
#   cratonvm --compatible [--nojit] -cp out/L5W108DS/main.jar L5W108ClassPathDotSegments
#   cratonvm --jdk-only   [--nojit] -jar out/L5W108DS/main.jar
#   cratonvm --compatible [--nojit] -jar out/L5W108DS/main.jar
#
# HOTSPOT_EXPECTED derived from the JDK 25 sources, not run (every command,
# Windows and Linux):
# HOTSPOT_EXPECTED_BEGIN
# getResources count=1
# url0 dot-segments=false jar=dep.jar
# getResource dot-segments=false
# dep.D source dot-segments=false jar=dep.jar
# HOTSPOT_EXPECTED_END
set -e
OUT=out/L5W108DS
rm -rf "$OUT"
mkdir -p "$OUT/src/dep" "$OUT/depcls" "$OUT/main" "$OUT/sub"
printf 'package dep;\npublic class D { }\n' > "$OUT/src/dep/D.java"
javac --release 8 -d "$OUT/depcls" "$OUT/src/dep/D.java"
printf 'from-dep' > "$OUT/depcls/dep/d.txt"
jar --create --file "$OUT/dep.jar" -C "$OUT/depcls" .
javac --release 11 -d "$OUT/main" L5W108ClassPathDotSegments.java
printf 'Main-Class: L5W108ClassPathDotSegments\nClass-Path: dep.jar sub/../dep.jar ./dep.jar\n' > "$OUT/manifest.txt"
jar --create --file "$OUT/main.jar" --manifest "$OUT/manifest.txt" -C "$OUT/main" .
