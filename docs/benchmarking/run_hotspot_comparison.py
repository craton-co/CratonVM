import sys
import os
import subprocess
import time

JAVA_HOME = "/usr/lib/jvm/java-25-openjdk-amd64"
RUNNER_DIR = "/root/CVM/apps/spring-suite-runner"
CP_DIR = "/root/CVM/apps/spring-suite-runner/.af_spring"
TIMEOUT_S = 180

SPRING_JVM_ARGS = [
    "--add-opens=java.base/java.lang=ALL-UNNAMED",
    "--add-opens=java.base/java.util=ALL-UNNAMED",
    "-Djava.awt.headless=true",
    "-XX:+EnableDynamicAgentLoading",
]

BIN_TODAY = "/root/custom-bins/cratonvm-spring-eval-20261109"
FLAGS_TODAY = ["--fast-boot"]

BIN_AUG10 = "/root/custom-bins/cratonvm-aug10-20260810"
FLAGS_AUG10 = ["--real-jdk"]

CLASSES = [
    ("DefaultWebClientTests", "org.springframework.web.reactive.function.client.DefaultWebClientTests", "spring-webflux"),
    ("XmlConfigTests", "org.springframework.test.web.servlet.samples.context.XmlConfigTests", "spring-test"),
    ("JavaConfigTests", "org.springframework.test.web.servlet.samples.context.JavaConfigTests", "spring-test"),
    ("ImportHttpServiceRegistrarTests", "org.springframework.web.service.registry.ImportHttpServiceRegistrarTests", "spring-web"),
    ("HttpServiceProxyRegistrationAotProcessorTests", "org.springframework.web.service.registry.HttpServiceProxyRegistrationAotProcessorTests", "spring-web"),
]

def run_test(bin_path, flags, mod, cls, agent_jar=None):
    cp_file = f"{CP_DIR}/{mod}.txt"
    agent_flags = [f"-javaagent:{agent_jar}"] if agent_jar else []
    cmd = [
        bin_path,
    ] + flags + SPRING_JVM_ARGS + agent_flags + [
        "-Xmx4g",
        f"--java-home={JAVA_HOME}",
        f"@{cp_file}",
        "CratonRunner",
        cls,
    ]
    t0 = time.time()
    try:
        proc = subprocess.run(cmd, cwd=RUNNER_DIR, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, errors="replace", timeout=TIMEOUT_S)
        wall_ms = int((time.time() - t0) * 1000)
        output = proc.stdout

        for line in output.splitlines():
            line = line.strip()
            if line.startswith("@@RESULT"):
                # format: @@RESULT <class> found=X started=X ok=X failed=X aborted=X skipped=X ms=X
                parts = line.split()
                data = {}
                for p in parts[2:]:
                    if "=" in p:
                        k, v = p.split("=", 1)
                        data[k] = int(v) if v.isdigit() else v
                data["status"] = "PASS" if data.get("failed", 0) == 0 and data.get("ok", 0) > 0 else "FAIL"
                data["wall_ms"] = wall_ms
                return data

        first_err = [l for l in output.splitlines() if "Error" in l or "Exception" in l][:2]
        return {"status": "FAIL", "ok": 0, "failed": 0, "found": 0, "ms": wall_ms, "wall_ms": wall_ms, "err": first_err}
    except subprocess.TimeoutExpired:
        return {"status": "TIMEOUT", "ok": 0, "failed": 0, "found": 0, "ms": TIMEOUT_S * 1000, "wall_ms": TIMEOUT_S * 1000}

def main():
    print("=" * 110)
    print("INTERLEAVED PROBE: Today Fast-Boot vs 10 Aug 2026 Default on User-Selected Hotspots")
    print(f"Today Fast-Boot binary : {BIN_TODAY} {' '.join(FLAGS_TODAY)}")
    print(f"10 Aug 2026 binary     : {BIN_AUG10} {' '.join(FLAGS_AUG10)}")
    print(f"10 Aug commit          : c180028dc4 (Merge fix/h2-deopt-tls-hangs-20260810)")
    print("=" * 110)

    results = []

    for idx, (short_name, full_name, mod) in enumerate(CLASSES, 1):
        print(f"\n[{idx}/5] Evaluating {short_name} ({mod})")
        # Interleave arm execution order
        if idx % 2 == 1:
            print("  -> Running Today Fast-Boot...   ", end="", flush=True)
            res_today = run_test(BIN_TODAY, FLAGS_TODAY, mod, full_name)
            print(f"{res_today['status']} ({res_today.get('ok', 0)}/{res_today.get('found', 0)} ok, test_ms={res_today['ms']}ms, wall={res_today['wall_ms']}ms)")

            print("  -> Running 10 Aug Default...    ", end="", flush=True)
            res_aug10 = run_test(BIN_AUG10, FLAGS_AUG10, mod, full_name)
            print(f"{res_aug10['status']} ({res_aug10.get('ok', 0)}/{res_aug10.get('found', 0)} ok, test_ms={res_aug10['ms']}ms, wall={res_aug10['wall_ms']}ms)")
        else:
            print("  -> Running 10 Aug Default...    ", end="", flush=True)
            res_aug10 = run_test(BIN_AUG10, FLAGS_AUG10, mod, full_name)
            print(f"{res_aug10['status']} ({res_aug10.get('ok', 0)}/{res_aug10.get('found', 0)} ok, test_ms={res_aug10['ms']}ms, wall={res_aug10['wall_ms']}ms)")

            print("  -> Running Today Fast-Boot...   ", end="", flush=True)
            res_today = run_test(BIN_TODAY, FLAGS_TODAY, mod, full_name)
            print(f"{res_today['status']} ({res_today.get('ok', 0)}/{res_today.get('found', 0)} ok, test_ms={res_today['ms']}ms, wall={res_today['wall_ms']}ms)")

        speedup_test = res_aug10['ms'] / max(res_today['ms'], 1)
        speedup_wall = res_aug10['wall_ms'] / max(res_today['wall_ms'], 1)
        results.append({
            "name": short_name,
            "today": res_today,
            "aug10": res_aug10,
            "speedup_test": speedup_test,
            "speedup_wall": speedup_wall,
        })

    print("\n" + "=" * 115)
    print("DETAILED ADVANCEMENT MATRIX: 10 Aug 2026 Default vs Today Fast-Boot")
    print("=" * 115)
    hdr = f"{'Class':<46} | {'10 Aug Default (ms)':<22} | {'Today Fast-Boot (ms)':<22} | {'Test Speedup':<12} | {'Wall Speedup':<12}"
    print(hdr)
    print("-" * 115)

    tot_aug10_test = sum(r['aug10']['ms'] for r in results)
    tot_today_test = sum(r['today']['ms'] for r in results)
    tot_aug10_wall = sum(r['aug10']['wall_ms'] for r in results)
    tot_today_wall = sum(r['today']['wall_ms'] for r in results)

    for r in results:
        aug_str = f"{r['aug10']['status']} ({r['aug10']['ms']}ms / {r['aug10']['wall_ms']}ms)"
        tod_str = f"{r['today']['status']} ({r['today']['ms']}ms / {r['today']['wall_ms']}ms)"
        sp_test = f"{r['speedup_test']:.2f}x"
        sp_wall = f"{r['speedup_wall']:.2f}x"
        print(f"{r['name']:<46} | {aug_str:<22} | {tod_str:<22} | {sp_test:<12} | {sp_wall:<12}")

    print("-" * 115)
    overall_test_sp = tot_aug10_test / max(tot_today_test, 1)
    overall_wall_sp = tot_aug10_wall / max(tot_today_wall, 1)
    print(f"{'CUMULATIVE (All 5 hotspot classes)':<46} | {tot_aug10_test/1000.0:>8.1f}s / {tot_aug10_wall/1000.0:>6.1f}s | {tot_today_test/1000.0:>8.1f}s / {tot_today_wall/1000.0:>6.1f}s | {overall_test_sp:>10.2f}x | {overall_wall_sp:>10.2f}x")
    print("=" * 115)

if __name__ == "__main__":
    main()
