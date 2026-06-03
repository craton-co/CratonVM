#!/usr/bin/env python3
"""Guard against growth of the synthetic-stub native bucket in the default build.

CratonVM classifies every registered native method into one of three kinds:
  - intrinsic       a correct fast-path matching real bytecode (kept)
  - bridge          a native the VM genuinely needs, no real bytecode (kept)
  - synthetic-stub  a confirmed fake OR an as-yet-unclassified registration —
                    the "audit backlog" that the ongoing sweep is driving down.

See docs/synthetic-vs-real-explained.md for the full intrinsic/bridge/stub
taxonomy and why the registry conservatively defaults untagged registrations
to `synthetic-stub`.

This script is the CI guard for that sweep: the `synthetic-stub` count must
NEVER GROW versus the committed baseline. It is allowed to shrink (that is the
whole point — classify clusters as bridge/intrinsic or delete the fake). A
growth means a NEW fake-main shim or a NEW unclassified native slipped into the
default path; CI must fail so it gets reviewed.

USAGE
  python check_no_stub_growth.py <fresh-census.json> <baseline-census.json>

  <fresh-census.json>     A freshly-generated census, produced in CI by:
                            cratonvm --dump-native-registry <fresh-census.json> \\
                              -cp vm-cli/tests/resources HelloWorld
  <baseline-census.json>  The committed baseline, normally
                            test-infra/native-census/default-build-baseline.json

Both files share the schema:
  { "counts": { "intrinsic": int, "bridge": int,
                "synthetic-stub": int, "total": int },
    "natives": [ { "class": str, "name": str,
                   "descriptor": str, "kind": str }, ... ] }

EXIT STATUS
  0  fresh synthetic-stub count <= baseline (no regression; improvement printed)
  1  fresh count > baseline (newly-synthetic natives printed as a stable diff),
     OR either file is missing / unparseable / malformed.
"""

import argparse
import json
import sys

STUB_KIND = "synthetic-stub"


def load_census(path):
    """Load and minimally validate a census JSON file.

    Returns the parsed dict. Raises SystemExit(1) with a clear message on any
    missing / unparseable / malformed input so CI fails loudly rather than
    silently passing.
    """
    try:
        with open(path, "r", encoding="utf-8") as fh:
            data = json.load(fh)
    except FileNotFoundError:
        fail("census file not found: {}".format(path))
    except OSError as exc:
        fail("could not read census file {}: {}".format(path, exc))
    except json.JSONDecodeError as exc:
        fail("census file {} is not valid JSON: {}".format(path, exc))

    if not isinstance(data, dict):
        fail("census file {} must be a JSON object, got {}".format(
            path, type(data).__name__))
    counts = data.get("counts")
    if not isinstance(counts, dict) or STUB_KIND not in counts:
        fail("census file {} is missing counts[\"{}\"]".format(path, STUB_KIND))
    if not isinstance(counts.get(STUB_KIND), int):
        fail("census file {} has a non-integer counts[\"{}\"]".format(
            path, STUB_KIND))
    if not isinstance(data.get("natives"), list):
        fail("census file {} is missing a natives[] array".format(path))
    return data


def fail(message):
    print("ERROR: {}".format(message), file=sys.stderr)
    sys.exit(1)


def stub_keys(data):
    """Return the set of (class, name, descriptor) for synthetic-stub natives."""
    keys = set()
    for entry in data["natives"]:
        if isinstance(entry, dict) and entry.get("kind") == STUB_KIND:
            keys.add((
                entry.get("class", ""),
                entry.get("name", ""),
                entry.get("descriptor", ""),
            ))
    return keys


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="Fail if the synthetic-stub native count grew vs baseline.",
    )
    parser.add_argument("fresh", help="freshly-generated census JSON")
    parser.add_argument("baseline", help="committed baseline census JSON")
    args = parser.parse_args(argv)

    fresh = load_census(args.fresh)
    baseline = load_census(args.baseline)

    fresh_count = fresh["counts"][STUB_KIND]
    base_count = baseline["counts"][STUB_KIND]

    print("synthetic-stub census guard")
    print("  baseline ({}): {}".format(args.baseline, base_count))
    print("  fresh    ({}): {}".format(args.fresh, fresh_count))

    if fresh_count <= base_count:
        delta = base_count - fresh_count
        if delta > 0:
            print("OK: synthetic-stub bucket shrank by {} "
                  "({} -> {}). The sweep is making progress.".format(
                      delta, base_count, fresh_count))
        else:
            print("OK: synthetic-stub bucket unchanged ({}).".format(fresh_count))
        return 0

    # Regression: the bucket grew. Show exactly which natives are newly
    # synthetic-stub so the diff is actionable and stable across runs.
    base_keys = stub_keys(baseline)
    fresh_keys = stub_keys(fresh)
    new_keys = sorted(fresh_keys - base_keys)

    print("")
    print("FAIL: synthetic-stub bucket GREW by {} ({} -> {}).".format(
        fresh_count - base_count, base_count, fresh_count))
    print("A new fake-main shim or unclassified native entered the default "
          "build. Each new synthetic-stub must be classified bridge/intrinsic "
          "or removed — see docs/synthetic-vs-real-explained.md.")
    print("")
    if new_keys:
        print("Newly synthetic-stub natives ({}):".format(len(new_keys)))
        for cls, name, desc in new_keys:
            print("  + {}.{}{}".format(cls, name, desc))
    else:
        # Count rose but no new (class,name,descriptor) keys differ — e.g. a
        # native flipped FROM bridge/intrinsic TO synthetic-stub while another
        # was deleted. The count is authoritative, so still fail.
        print("(count rose but no new (class,name,descriptor) keys appeared; "
              "an existing native was reclassified TO synthetic-stub.)")
    return 1


if __name__ == "__main__":
    sys.exit(main())
