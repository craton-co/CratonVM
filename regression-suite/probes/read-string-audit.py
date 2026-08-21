"""Narrow an "audit every call site of X" question to its answerable subset.

G78-1 / G70-1 N1. A grep for `read_string(` returns 2904 hits. That is a census,
not an audit, and most of it is noise: the majority of those sites read a class
name, a descriptor or a charset, where `read_string` is the CORRECT reader.

Two filters cut it to 18, neither of which is a grep:

  1. DATAFLOW, statically. A read can only lose something observable if the
     value comes back. Parse each `r.register(...)` block by brace matching and
     keep the ones containing BOTH a read and a `create_string`.  2904 -> 136.

  2. REACHABILITY, by measurement. Under --jdk-only most natives are dead --
     real JDK bytecode answers instead. Run the corpus with
     `--dump-native-registry <f>` per vector; each row carries `owns_slot`,
     `invocations` and a `registered_by` file:line that joins straight back to
     the static scan. Keep the live, owning ones.  136 -> 18.

The per-site judgement the nomination asked for -- is this value INSPECTED, or
handed BACK to Java -- still has to be made by a person. This just makes it
eighteen judgements instead of 2904, ranked by invocation count so the reading
starts where the traffic is.

Usage:
    python regression-suite/probes/read-string-audit.py      # writes rows.json

Then, to add the reachability filter, dump a registry per vector and intersect
by (file, line-in-block-span). The method generalises: change the two regexes
below to audit `create_string` callers, `read_string_chars` callers, or the
`to_string_lossy` sites (G78-1 N3).
"""
import os, re, json, sys

CRATES = ["native-builtins","native-collections","native-io","native-awt",
          "native-builtins-crypto","native-builtins-security","vm","native-api"]

REG = re.compile(r'\br\.register(?:_static)?\s*\(')

def blocks(src):
    """Yield (start,end) spans of each r.register(...) call, by paren matching."""
    for m in REG.finditer(src):
        i = m.end()-1
        depth = 0
        while i < len(src):
            c = src[i]
            if c == '"':                      # skip string literals
                i += 1
                while i < len(src) and src[i] != '"':
                    if src[i] == chr(92): i += 1
                    i += 1
            elif c == '(' : depth += 1
            elif c == ')':
                depth -= 1
                if depth == 0:
                    yield m.start(), i+1
                    break
            i += 1

ARGS = re.compile(r'^\s*([A-Za-z_0-9]+|"[^"]*")\s*,\s*"([^"]*)"\s*,\s*"([^"]*)"')

rows = []
for crate in CRATES:
    for root, _, files in os.walk(crate):
        for f in files:
            if not f.endswith(".rs"): continue
            p = os.path.join(root, f).replace("\\","/")
            src = open(p, encoding="utf-8", errors="replace").read()
            for s, e in blocks(src):
                body = src[s:e]
                head = ARGS.search(body[body.index("(")+1:])
                cls  = head.group(1) if head else "?"
                name = head.group(2) if head else "?"
                desc = head.group(3) if head else "?"
                reads  = len(re.findall(r'\bread_string\s*\(', body))
                readsu = len(re.findall(r'\bread_string_(?:units|chars|utf16)\s*\(', body))
                creates= len(re.findall(r'\bcreate_string\s*\(', body))
                createu= len(re.findall(r'\bcreate_string_from_units\s*\(', body))
                if reads == 0: continue
                rows.append(dict(file=p, line=src[:s].count("\n")+1, endline=src[:e].count(chr(10))+1, cls=cls,
                                 name=name, desc=desc, reads=reads, readsu=readsu,
                                 creates=creates, createu=createu,
                                 rt = creates > 0))
json.dump(rows, open("scratchpad/g81/rows.json","w"), indent=0)
tot = len(rows)
rt  = [r for r in rows if r["rt"]]
print("register-blocks containing read_string : %d" % tot)
print("  ... and also create_string (candidate): %d" % len(rt))
print("  ... inspect-only (no create_string)   : %d" % (tot-len(rt)))
print("returns a String by descriptor          : %d" %
      len([r for r in rows if r["desc"].endswith(")Ljava/lang/String;")]))
