/// The helper table's own source, with `\n` line ends.
fn table_src() -> String {
    include_str!("table.rs").replace("\r\n", "\n")
}

/// The text of the compile-time pin block, whitespace squeezed out.
fn pin_block_squeezed(src: &str) -> String {
    let from = src
        .find("    use cratonvm_jit::direct_helpers::*;")
        .expect("the direct-helper pin block");
    let to = from
        + src[from..]
            .find("// Helper-slot signature checks")
            .expect("the end of the direct-helper pin block");
    src[from..to].split_whitespace().collect()
}

#[test]
fn rt22_every_direct_helper_alias_is_pinned() {
    let aliases_src =
        include_str!("../../../../jit/src/direct_helpers.rs").replace("\r\n", "\n");
    // Both spellings: a `pub type DirectHelperFn.. =` line, and a row of
    // the `direct_helper_fn_slots!` table (`DirectHelperFn.., field, ..;`).
    let aliases: Vec<String> = aliases_src
        .lines()
        .map(str::trim_start)
        .filter_map(|line| {
            line.strip_prefix("pub type DirectHelperFn")
                .or_else(|| line.strip_prefix("DirectHelperFn"))
        })
        .filter(|rest| {
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            rest[end..].starts_with(',') || rest[end..].starts_with(" =")
        })
        .map(|rest| {
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            format!("DirectHelperFn{}", &rest[..end])
        })
        .collect();
    assert!(
        aliases.len() >= 60,
        "the alias scan found only {} aliases",
        aliases.len()
    );
    let block = pin_block_squeezed(&table_src());
    for alias in &aliases {
        assert!(
            block.contains(&format!("let_:{alias}=")),
            "{alias} has no `let _: {alias} = ..` line in table.rs's pin block"
        );
    }
}

#[test]
fn rt22_every_stored_direct_helper_word_is_pinned() {
    let src = table_src();
    let start = src
        .find("pub fn direct_helper_table() -> cratonvm_jit::DirectHelperTable {")
        .expect("the table constructor");
    let end = start
        + src[start..]
            .find("// Direct-helper signature checks")
            .expect("the end of the table constructor");
    let code: Vec<&str> = src[start..end]
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect();
    let code = code.join("\n");
    let block = pin_block_squeezed(&src);
    let mut stored = 0usize;
    for (at, _) in code.match_indices(" as *const ()") {
        // rustfmt may break the line between the function and its cast.
        let head = code[..at].trim_end();
        let from = head
            .char_indices()
            .rev()
            .find(|(_, c)| !(c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '<' | '>')))
            .map_or(0, |(i, c)| i + c.len_utf8());
        let function = &head[from..];
        stored += 1;
        assert!(
            block.contains(&format!("={function};")),
            "the table stores `{function}` but no `let _: DirectHelperFn.. = {function};` pins its signature"
        );
    }
    assert!(
        stored >= 60,
        "the table scan found only {stored} stored words"
    );
}
