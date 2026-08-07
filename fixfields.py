import json, subprocess, sys, collections

# Convert `h.field` -> `h.field()` at exactly the spans rustc reports as
# "method, not a field" (E0615). Driven by the compiler, so it can only touch
# real ObjectHeader accesses -- a textual sweep would also hit `.kind` on the
# dozens of unrelated types that have one.
def pass_once(pkgs):
    cmd = ["cargo","check","--all-targets","--message-format=json"]
    for p in pkgs: cmd += ["-p", p]
    out = subprocess.run(cmd, capture_output=True, text=True, encoding="utf-8", errors="replace").stdout
    edits = collections.defaultdict(list)
    for line in out.splitlines():
        try: msg = json.loads(line)
        except Exception: continue
        if msg.get("reason") != "compiler-message": continue
        d = msg["message"]
        if d.get("code", {}) and d["code"].get("code") == "E0615":
            for sp in d["spans"]:
                if not sp.get("is_primary"): continue
                edits[sp["file_name"]].append((sp["line_start"], sp["column_end"]))
    n = 0
    for path, spans in edits.items():
        try: lines = open(path, "rb").read().decode("utf-8").split("\n")
        except OSError: continue
        # apply right-to-left so earlier columns stay valid
        for ln, col in sorted(set(spans), reverse=True):
            i = ln - 1
            if i >= len(lines): continue
            s = lines[i]
            at = col - 1
            if at <= len(s) and not s[at:at+1] == "(":
                lines[i] = s[:at] + "()" + s[at:]
                n += 1
        open(path, "wb").write("\n".join(lines).encode("utf-8"))
    return n

pkgs = sys.argv[1:]
total = 0
for it in range(12):
    n = pass_once(pkgs)
    print("pass %d: %d field->method conversions" % (it+1, n))
    total += n
    if n == 0: break
print("TOTAL:", total)
