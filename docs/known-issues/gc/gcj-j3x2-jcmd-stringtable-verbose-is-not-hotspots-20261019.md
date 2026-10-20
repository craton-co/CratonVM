# `jcmd <pid> VM.stringtable -verbose` prints the statistics block plus quoted entries, not HotSpot's dump

> **STATUS (2026-10-19, gcj j3/x2): OPEN -- cross-lane (ob2 owns `vm/src/runtime/serviceability.rs`, which
> wave j3's rules keep out of other lanes while ob2 judges its rows).** Found by reading the j3 base
> `43edda6dbe`; the HotSpot shape below is from a reading of HotSpot 25's `StringTable::dump` /
> `HashtableTextDump::put_utf8` FROM MEMORY, not from a capture: capture row `ob2_cap_st_verbose`
> (ob2's report lists it as unverified) decides it before the diff lands.

## What CratonVM prints

`CRATONVM_GEN_JCMD_STRINGTABLE_LIVE` (ob2, default ON on Generational) feeds the live pool into
`default_vm_stringtable_text` (`vm/src/runtime/serviceability.rs` ~5355). Before it, the trait default
ignored its arguments, so `-verbose` printed the statistics. Now (~5402-5406) the statistics block is
printed and, under `-verbose`, one line per entry is appended:
`format!("{}: \"{}\"\n", s.len(), s)` -- the Rust UTF-8 byte length and the string in double quotes,
unescaped. The default-ON switch therefore exposes a new shape on every Generational `-verbose` call.

## What HotSpot 25 prints (to be confirmed by the capture)

- non-verbose: `print_table_statistics` only (unchanged here);
- verbose: `VERSION: 1.1`, then one line per live entry: `"%d: "` with the MODIFIED-UTF-8 length,
  then the bytes through `put_utf8`, no quotes, escaping `\t \r \n \\` and writing non-printable bytes
  as `\xNN`; no statistics block. (A supplementary character is 6 modified-UTF-8 bytes, U+0000 is 2;
  Rust's UTF-8 gives 4 and 1.)

## The fix (for ob2, after the capture agrees)

```diff
--- a/vm/src/runtime/serviceability.rs
+++ b/vm/src/runtime/serviceability.rs
@@ pub(crate) fn default_vm_stringtable_text(
-    let mut out = format!(
+    if verbose {
+        let mut out = String::from("VERSION: 1.1\n");
+        for s in verbose_items {
+            let mut bytes: Vec<u8> = Vec::new();
+            for c in s.encode_utf16() {
+                match c {
+                    0x0001..=0x007F => bytes.push(c as u8),
+                    0x0000 | 0x0080..=0x07FF => {
+                        bytes.push(0xC0 | (c >> 6) as u8);
+                        bytes.push(0x80 | (c & 0x3F) as u8);
+                    }
+                    _ => {
+                        bytes.push(0xE0 | (c >> 12) as u8);
+                        bytes.push(0x80 | ((c >> 6) & 0x3F) as u8);
+                        bytes.push(0x80 | (c & 0x3F) as u8);
+                    }
+                }
+            }
+            out.push_str(&format!("{}: ", bytes.len()));
+            for b in bytes {
+                match b {
+                    b'\t' => out.push_str("\\t"),
+                    b'\r' => out.push_str("\\r"),
+                    b'\n' => out.push_str("\\n"),
+                    b'\\' => out.push_str("\\\\"),
+                    0x20..=0x7E => out.push(b as char),
+                    _ => out.push_str(&format!("\\x{b:02x}")),
+                }
+            }
+            out.push('\n');
+        }
+        return out;
+    }
+    let mut out = format!(
@@
-    if verbose {
-        for s in verbose_items {
-            out.push_str(&format!("{}: \"{}\"\n", s.len(), s));
-        }
-    }
```

Fail-first unit test: items `["a\tb", "\u{e9}"]`, verbose -> exactly `"VERSION: 1.1\n3: a\\tb\n2: \\xc3\\xa9\n"`,
and no `StringTable statistics`. (Entry order: HotSpot walks its concurrent hash table; a byte-for-byte row
needs the order masked or a single-entry shape.)

Until it lands, the narrow alternative is to keep `-verbose` on the trait-default text (statistics only,
the pre-ob2 behaviour) under the default switch.

## Related nits (same function)

`Maximum bucket size` is hard-coded to 2 when there are entries (~5398), the variance is printed as the
mean, buckets are fixed at 65536, literal bytes leave out object headers (ob2 masks that column), and
HotSpot prints a `Shared String Table statistics` block when CDS archived strings exist (UNSURE for
Temurin 25's default archive). `gcj_j2_ob2_stringtable_live_is_generational_only` (~11952) pins only
`!gen_jcmd_stringtable_live(false)`: nothing pins the kill or that `SharedVm::vm_stringtable` takes the gate.
