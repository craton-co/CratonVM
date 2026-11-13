#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright 2024-2026 Craton Software Company

"""
Builds the per-module classpath argfiles for scripts/run_spring_interleaved.py.

Each argfile is what gradle resolves for that module's test runtime
(scripts/spring-test-classpath.init.gradle), not a glob of ~/.gradle/caches, which holds
every version ever downloaded (JUnit 5.12 + 6.0 + 6.1, three jboss-logging, two SLF4J
bindings, ...) and made AbstractMethodError / NoSuchMethodError / IllegalAccessError
rows that were never CratonVM defects.  The modules disagree with each other on
unmanaged libraries (kotlin, slf4j, okio), so there is one classpath per module, never a
union.  Two versions of one artifact inside a module are refused.

Output dir: <module>.txt argfiles plus classes.tsv (fqcn<TAB>module) for the runner.
"""
import argparse
import os
import re
import subprocess
import sys

CACHE = re.compile(r".*/files-2\.1/([^/]+)/([^/]+)/([^/]+)/[0-9a-f]{40}/[^/]+\.jar$")
INIT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "spring-test-classpath.init.gradle")


def version_conflicts(paths):
    """{group:artifact: versions} for every cached artifact present in more than one version."""
    seen = {}
    for p in paths:
        m = CACHE.match(p)
        if m:
            seen.setdefault(f"{m[1]}:{m[2]}", set()).add(m[3])
    return {k: sorted(v) for k, v in seen.items() if len(v) > 1}


def materialize(entry):
    """A project jar nobody built (build/libs/<m>-*.jar) stands for the module's class dirs."""
    if os.path.exists(entry) or "/build/libs/" not in entry:
        return [entry]
    build = entry.split("/libs/")[0]
    source_set = "testFixtures" if entry.endswith("-test-fixtures.jar") else "main"
    dirs = [f"{build}/classes/java/{source_set}", f"{build}/classes/kotlin/{source_set}",
            f"{build}/resources/{source_set}"]
    return [d for d in dirs if os.path.isdir(d)]


def gradle_classpaths(spring_root, java_home, modules):
    """{module: [entries]}; online on purpose: a stale host cache lacks jars the checkout pins."""
    # --continue: one module that cannot resolve must not hide the others.
    cmd = ["./gradlew", "--no-daemon", "-q", "--continue", "-I", INIT] + \
          [f":{m}:printTestClasspath" for m in modules]
    proc = subprocess.run(cmd, cwd=spring_root, capture_output=True, text=True,
                          env={**os.environ, "JAVA_HOME": java_home})
    out = {}
    for line in proc.stdout.splitlines():
        if line.startswith("CPENTRY\t"):
            _, mod, entry = line.split("\t", 2)
            out.setdefault(mod.lstrip(":"), []).append(entry)
    if not out:
        sys.exit(f"gradle printed no classpath (rc={proc.returncode}): {proc.stderr[-2000:]}")
    return {m: list(dict.fromkeys(x for e in es for x in materialize(e))) for m, es in out.items()}


def test_classes(spring_root, module):
    """Top-level test classes compiled for one module (java and kotlin source sets)."""
    for lang in ("java", "kotlin"):
        base = os.path.join(spring_root, module, "build", "classes", lang, "test")
        for root, _, files in os.walk(base):
            pkg = os.path.relpath(root, base).replace(os.sep, ".")
            for f in files:
                if f.endswith(".class") and "$" not in f:
                    yield f[:-6] if pkg == "." else f"{pkg}.{f[:-6]}"


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--spring-root", default="/root/CVM/apps/spring-framework")
    ap.add_argument("--runner-dir", default="/root/CVM/apps/spring-suite-runner")
    ap.add_argument("--java-home", default="/usr/lib/jvm/java-25-openjdk-amd64")
    ap.add_argument("--out-dir", default="/root/CVM/apps/spring-suite-runner/.af_spring")
    a = ap.parse_args()

    modules = sorted(d for d in os.listdir(a.spring_root)
                     if (d.startswith("spring-") or d == "integration-tests")
                     and os.path.isdir(os.path.join(a.spring_root, d)))
    os.makedirs(a.out_dir, exist_ok=True)
    cps = gradle_classpaths(a.spring_root, a.java_home, modules)
    with open(os.path.join(a.out_dir, "classes.tsv"), "w") as tsv:
        for mod, entries in sorted(cps.items()):
            bad = version_conflicts(entries)
            if bad:
                sys.exit(f"{mod}: two versions of one artifact: " +
                         "; ".join(f"{k} {v}" for k, v in bad.items()))
            with open(os.path.join(a.out_dir, f"{mod}.txt"), "w") as f:
                f.write("-cp\n" + ":".join([a.runner_dir] + entries) + "\n")
            classes = sorted(test_classes(a.spring_root, mod))
            for c in classes:
                tsv.write(f"{c}\t{mod}\n")
            print(f"{mod}: {len(entries)} entries, {len(classes)} classes")
    missing = [m for m in modules if m not in cps]
    if missing:
        sys.exit("no classpath resolved for: " + ", ".join(missing))


if __name__ == "__main__":
    main()
