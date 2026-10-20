#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Fresh-process HotSpot oracle for java.* user-loader resolution and attach.

Requires a debug-feature-enabled CratonVM, JDK 25 and byte-buddy-agent 1.17.7.
All generated classes and logs live in the explicitly supplied work directory.
"""
import argparse
import os
import pathlib
import socket
import signal
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cvm", type=pathlib.Path, required=True)
    parser.add_argument("--jdk", type=pathlib.Path, required=True)
    parser.add_argument("--byte-buddy-agent", type=pathlib.Path, required=True)
    parser.add_argument("--work", type=pathlib.Path, required=True)
    args = parser.parse_args()
    repo = pathlib.Path(__file__).resolve().parents[2]
    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    classes = work / "classes"
    classes.mkdir(exist_ok=True)
    suffix = ".exe" if sys.platform == "win32" else ""
    java = args.jdk.resolve() / "bin" / ("java" + suffix)
    javac = args.jdk.resolve() / "bin" / ("javac" + suffix)
    cvm = args.cvm.resolve()
    bb = args.byte_buddy_agent.resolve()
    fixtures = [
        repo / "tools/probes/interp/L5/I47VerifierLoadsAtLink.java",
        repo / "tools/probes/interp/L5/I47JavaNameLoads.java",
        repo / "tools/probes/interp/L1/L1W59AttachListenerOps.java",
        repo / "tools/probes/interp/L1/L1W56AttachRefusals.java",
        repo / "tools/probes/interp/L1/L1W59DisableAttachMechanism.java",
    ]
    subprocess.run([str(javac), "-d", str(classes), *map(str, fixtures)], check=True)
    (classes / "I47VerifierLoadsAtLink$Gone.class").unlink()
    driver = work / "BBDriver.java"
    driver.write_text('''public class BBDriver {
        public static void main(String[] args) {
            System.out.println("allowAttachSelf=" + System.getProperty("jdk.attach.allowAttachSelf"));
            System.out.println("install=" + (net.bytebuddy.agent.ByteBuddyAgent.install() != null));
        }
    }''')
    subprocess.run([str(javac), "-cp", str(bb), "-d", str(classes), str(driver)], check=True)

    def seed_stale_attach_socket():
        # Runs in the child immediately before exec, so the abandoned socket
        # has the target's actual PID. Both HotSpot and CratonVM must remove it
        # at startup, before a JDK client can mistake it for a live endpoint.
        path = pathlib.Path('/tmp/.java_pid' + str(os.getpid()))
        path.unlink(missing_ok=True)
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as endpoint:
            endpoint.bind(str(path))
            os.chmod(path, 0o600)
        # Background shell jobs inherit SIG_IGN across exec. HotSpot installs
        # its attach wakeup handler even with this inherited disposition.
        signal.signal(signal.SIGQUIT, signal.SIG_IGN)

    def run(label, launcher, flags, cp, cls, tail=(), stale=False):
        command = [str(launcher), *flags, "-cp", cp, cls, *map(str, tail)]
        options = {"preexec_fn": seed_stale_attach_socket} if stale else {}
        result = subprocess.run(command, cwd=work, capture_output=True, timeout=180, **options)
        (work / (label + ".out")).write_bytes(result.stdout)
        (work / (label + ".err")).write_bytes(result.stderr)
        if result.returncode:
            raise RuntimeError(f"{label}: exit {result.returncode}; see {work}")
        return result.stdout.decode().replace("\r\n", "\n")

    cases = [
        ("verifier", "I47VerifierLoadsAtLink", (), (classes,), str(classes)),
        ("java-names", "I47JavaNameLoads", (), (classes,), str(classes)),
        ("external-attach", "L1W59AttachListenerOps", (), (), str(classes)),
        ("external-attach-v1", "L1W59AttachListenerOps", ("-Di47.attach.compat=true",), (), str(classes)),
        ("refusals", "L1W56AttachRefusals", (), (), str(classes)),
        ("disabled", "L1W59DisableAttachMechanism", (), (), str(classes)),
        ("byte-buddy", "BBDriver", (), (), str(classes) + (";" if suffix else ":") + str(bb)),
        ("byte-buddy-false", "BBDriver", ("-Djdk.attach.allowAttachSelf=false",), (), str(classes) + (";" if suffix else ":") + str(bb)),
        ("byte-buddy-true", "BBDriver", ("-Djdk.attach.allowAttachSelf=true",), (), str(classes) + (";" if suffix else ":") + str(bb)),
    ]
    if sys.platform == "linux":
        cases.append(("external-attach-stale", "L1W59AttachListenerOps", (), (), str(classes)))
    checks = 0
    for name, cls, flags, tail, cp in cases:
        stale = name == "external-attach-stale"
        oracle = run("hotspot-" + name, java, flags, cp, cls, tail, stale=stale)
        for mode in ("--jdk-only", "--compatible"):
            for jit in ((), ("--nojit",)):
                label = mode[2:] + ("-interp" if jit else "-jit") + "-" + name
                actual = run(label, cvm, (mode, *jit, *flags), cp, cls, tail, stale=stale)
                if actual != oracle:
                    import difflib
                    sys.stderr.writelines(difflib.unified_diff(oracle.splitlines(True), actual.splitlines(True), fromfile="HotSpot", tofile=label))
                    raise RuntimeError(f"{label}: stdout differs; see {work}")
                checks += 1
                print("PASS " + label, flush=True)
    print(f"I47-I56-OK {checks} fresh-process oracle comparisons", flush=True)


if __name__ == "__main__":
    main()
