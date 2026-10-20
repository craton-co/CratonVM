#!/bin/sh
# Interpreter round i5 wave 109, lane L5 (page
# interpreter-i108-L5-compatible-class-path-element-spelled-twice-is-two-entries-FIXED-20261016):
# one jar on `-cp` as `a.jar`, `./a.jar` and `sub/../a.jar`, one directory as
# `d` and `./d`. JDK 25's `URLClassPath(String cp, ...)` maps every element
# through `new File(s).getCanonicalFile()` and keeps one loader per URL, so the
# jar and the directory are one element each and `getResources` answers one
# URL for each resource. `--compatible` kept every spelling as an entry (three
# URLs for the jar's resource, two for the directory's). Fixed by
# `classloading/src/class_path.rs` `same_classpath_file` (not built or run).
# `sub/` exists so the Linux kernel opens `sub/../a.jar` too.
#
# Builds out/L5W109CP/{main/,a.jar,d/,sub/}. Run from this directory with a
# JDK 25 javac, jar and java on PATH (`;` on Windows, `:` on Linux):
#
#   sh setup.sh
#   CP="out/L5W109CP/main;out/L5W109CP/a.jar;out/L5W109CP/./a.jar;out/L5W109CP/sub/../a.jar;out/L5W109CP/d;out/L5W109CP/./d"
#   java -cp "$CP" L5W109ClassPathSpelledTwice
#   cratonvm --jdk-only   [--nojit] -cp "$CP" L5W109ClassPathSpelledTwice
#   cratonvm --compatible [--nojit] -cp "$CP" L5W109ClassPathSpelledTwice
#
# HOTSPOT_EXPECTED derived from the JDK 25 sources, not run (Windows and Linux):
# HOTSPOT_EXPECTED_BEGIN
# a/a.txt getResources count=1
# d/d.txt getResources count=1
# a.A found=true
# HOTSPOT_EXPECTED_END
set -e
OUT=out/L5W109CP
rm -rf "$OUT"
mkdir -p "$OUT/src/a" "$OUT/acls" "$OUT/main" "$OUT/d/d" "$OUT/sub"
printf 'package a;\npublic class A { }\n' > "$OUT/src/a/A.java"
javac --release 8 -d "$OUT/acls" "$OUT/src/a/A.java"
printf 'from-a' > "$OUT/acls/a/a.txt"
jar --create --file "$OUT/a.jar" -C "$OUT/acls" .
printf 'from-d' > "$OUT/d/d/d.txt"
javac --release 11 -d "$OUT/main" L5W109ClassPathSpelledTwice.java
