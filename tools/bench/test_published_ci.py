import importlib.util
from pathlib import Path
import re
import unittest


ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("published_ci", Path(__file__).with_name("run-published-ci.py"))
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class PublishedRowsTest(unittest.TestCase):
    def test_cpu_tables_match_runner(self):
        document = (ROOT / "BENCHMARK.md").read_text(encoding="utf-8")
        sections = {"arithmetic": "1. Arithmetic", "fib": "2. Fibonacci", "sieve": "3. Sieve",
                    "matrix": "4. Matrix", "bintrees": "5. Binary Trees", "hashmap": "6. HashMap",
                    "stringregex": "7. String / Regex", "matrix-tiled": "Matrix (Tiled)"}
        for phase, title in sections.items():
            with self.subTest(phase=phase):
                section = re.search(rf"^#### {re.escape(title)}\s*$([\s\S]*?)(?=^#### |^### |^## |\Z)",
                                    document, re.MULTILINE)
                self.assertIsNotNone(section)
                labels = [re.sub(r"[\s,]", "", label.replace("×", "x")) for label in
                          re.findall(r"^\| \*\*(.*?)\*\*\s*\|", section.group(1), re.MULTILINE)]
                self.assertEqual(labels, [row[0] for row in runner.CPU[phase]])

    def test_kernel_result_requires_checksum(self):
        self.assertEqual(runner.KERNEL.findall("1. arithmetic: 12 ms [123]"), [("12", "123")])
        self.assertEqual(runner.KERNEL.findall("1. arithmetic: 12 ms"), [])

    def test_first_application_table_has_a_case_for_every_row(self):
        document = (ROOT / "PERFORMANCE.MD").read_text(encoding="utf-8")
        section = document.split("## Comprehensive Interleaved Benchmark Results", 1)[1].split("\n---", 1)[0]
        rows = re.findall(r"^\| \*\*[^|]+\*\* \|", section, re.MULTILINE)
        script = (ROOT / "tools/bench/apps/run-apps-benches-interleaved.sh").read_text(encoding="utf-8")
        self.assertEqual(len(rows), len(re.findall(r"^run_bench ", script, re.MULTILINE)) + 1)
        self.assertIn('run-bytebuf-probe.sh', script)


if __name__ == "__main__":
    unittest.main()
