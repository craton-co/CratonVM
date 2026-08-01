#!/usr/bin/env python3
"""Find natives that keep an ObjectRef local live across a GC-capable call
without a `pin_native_root` covering it (the "Family 1" stale-ObjectRef bug).

Heuristic, deliberately noisy on the safe side: for every top-level `fn` in the
file, report it when
  * it contains at least one GC-capable call (allocation, Java-callback
    dispatch, or a GC-safe blocking wait), AND
  * it contains NO `pin_native_root` / `pin_value` at all, AND
  * an `ObjectRef`-typed binding (a parameter or a `let` initialised from a
    `get_field`/`get_array_element`) is used textually AFTER the first
    GC-capable call.

Every hit still needs reading: a function whose refs all come from the
allocation result, or that never dereferences anything afterwards, is fine.
"""
import re
import sys

GC_CALLS = re.compile(
    r"\b("
    r"alloc_ref_array|alloc_synthetic|alloc_bucket_table|alloc_object|alloc_array|"
    r"new_object_initialized|new_object|"
    r"invoke_virtual|invoke_special|invoke_static|invoke_interface|"
    r"map_hash_key|map_keys_equal|element_hash_code|"
    r"monitor_enter_gc_safe|acquire_gc_safe|"
    r"intern_string|new_string|make_string"
    r")\s*\("
)
PIN = re.compile(
    r"\b(pin_native_root|pin_value|pin_value_slice|handle_root"
    r"|rooted_across|rooted_across1|add_global_root)\s*\("
)


def functions(path):
    src = open(path, encoding="utf-8").read().splitlines()
    starts = [i for i, l in enumerate(src) if re.match(r"^(pub )?(async )?fn \w+", l)]
    starts.append(len(src))
    for a, b in zip(starts, starts[1:]):
        name = re.match(r"^(pub )?(async )?fn (\w+)", src[a]).group(3)
        yield name, a + 1, src[a:b]


def main(paths):
    total = 0
    for path in paths:
        for name, lineno, body in functions(path):
            text = "\n".join(body)
            if not GC_CALLS.search(text):
                continue
            if PIN.search(text):
                continue
            # index of the first GC-capable call line
            first = next(i for i, l in enumerate(body) if GC_CALLS.search(l))
            tail = "\n".join(body[first + 1:])
            # Names bound before the GC point that look like heap references.
            pre = "\n".join(body[:first + 1])
            names = set(re.findall(r"\blet (?:mut )?(\w+)\s*(?::\s*ObjectRef)?\s*=", pre))
            names |= set(re.findall(r"(\w+)\s*:\s*ObjectRef", pre))
            hot = sorted(n for n in names
                         if re.search(r"\b" + re.escape(n) + r"\b", tail)
                         and n not in ("ctx", "args", "_"))
            if hot:
                total += 1
                print(f"{path}:{lineno}: fn {name}  reuses-after-GC: {', '.join(hot)}")
    print(f"\n{total} candidate function(s)")


if __name__ == "__main__":
    main(sys.argv[1:])
