/// The publication calls whose last argument is the `jdk_only` bit.
const CALLS: [&str; 3] = ["mic.update(", "pic.install(", ".install_with_class_slot("];
/// Top-level function heads in the helper sources. t1 wave 7 (lane V6):
/// every visibility / `unsafe` / `extern "C"` spelling, so a publication
/// in a `pub(super)` or `extern "C"` function is not attributed to the
/// function above it (whose own scope would then pass for it).
const HEADS: [&str; 15] = [
    "\nunsafe fn ",
    "\nfn ",
    "\npub unsafe extern \"C\" fn ",
    "\npub(crate) unsafe fn ",
    "\npub(crate) fn ",
    "\npub unsafe fn ",
    "\npub fn ",
    "\npub(super) fn ",
    "\npub(super) unsafe fn ",
    "\nunsafe extern \"C\" fn ",
    "\nextern \"C\" fn ",
    "\npub extern \"C\" fn ",
    "\npub(crate) extern \"C\" fn ",
    "\npub(crate) unsafe extern \"C\" fn ",
    "\npub(super) unsafe extern \"C\" fn ",
];

/// The call's last top-level argument, trimmed (`open` indexes its `(`).
fn last_argument(src: &str, open: usize) -> String {
    let mut depth = 0usize;
    let mut last_comma = open;
    for (i, c) in src[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    let arg = src[last_comma + 1..open + i].trim();
                    return arg.trim_end_matches(',').to_string();
                }
            }
            ',' if depth == 1 => {
                // A trailing comma before `)` keeps the previous argument.
                if !src[open + i + 1..].trim_start().starts_with(')') {
                    last_comma = open + i;
                }
            }
            _ => {}
        }
    }
    String::new()
}

#[test]
fn t1v4_every_jdk_only_ic_publication_opens_the_vm_scope() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let mut judged = 0usize;
    let mut unscoped = Vec::new();
    for call in CALLS {
        for (at, _) in src.match_indices(call) {
            let line_start = src[..at].rfind('\n').map_or(0, |n| n + 1);
            let line = src[line_start..at].trim_start();
            // Comments and source-pin string literals are not calls.
            let quoted = src[..at].ends_with('"') || src[..at].ends_with('`');
            if line.starts_with("//") || quoted {
                continue;
            }
            let open = at + call.len() - 1;
            let last = last_argument(&src, open);
            // A literal `false` (a test's Compatible publication) can never refuse.
            if last == "false" || last.is_empty() {
                continue;
            }
            let head = HEADS
                .iter()
                .filter_map(|h| src[..at].rfind(h))
                .max()
                .unwrap_or(0);
            judged += 1;
            if !src[head..at].contains("jdk_only_ic_publication_scope(") {
                let name = src[head..].lines().nth(1).unwrap_or("").to_string();
                unscoped.push(format!("{call} in {name}"));
            }
        }
    }
    assert!(judged >= 12, "the guard judged only {judged} publication sites");
    assert!(unscoped.is_empty(), "IC publications with no VM sink scope: {unscoped:?}");
}
