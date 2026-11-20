/// `lambda_site_refusal`'s production text, whitespace removed.
fn refusal_text() -> String {
    let src: String = concat!(include_str!("../invokedynamic/mod.rs"), "\n", include_str!("../invokedynamic/dbg_indy_all.rs"), "\n", include_str!("../invokedynamic/execute_invokedynamic_at_uncensused.rs"), "\n", include_str!("../invokedynamic/object_methods_getter_refusal.rs"), "\n", include_str!("../invokedynamic/descriptor_param_count_and_last_is.rs"))
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    let start = src.find("fnlambda_site_refusal(").expect("the refusal");
    let end = src[start..]
        .find("fnlambda_site_load_types(")
        .expect("the refusal's end");
    src[start..start + end].to_string()
}

/// gci i4/jd: an undecided `--real-jdk` lambda site is checked again after
/// the type load even when this thread loaded nothing (another thread may
/// have published the class in between), so a concurrent first use does
/// not take the JDK route a sequential one never takes.
#[test]
fn an_undecided_site_is_checked_again_even_when_nothing_was_loaded() {
    let text = refusal_text();
    let load = text
        .find("let(loaded,resolved)=lambda_site_load_types(shared,thread,class_id,info)?;")
        .expect("the type load");
    let after = &text[load..];
    let recheck = after.find("verdict=verdict_now(&resolved);").expect("the re-check");
    assert!(
        !after[..recheck].contains("ifloaded>0{"),
        "the re-check is gated on this thread having loaded a type again"
    );
}
