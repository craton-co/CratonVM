use super::*;

fn tokens(value: &str) -> Result<Vec<String>, String> {
    jdk_java_options_tokens(value, |path| match path {
        "opts" => Some("-Dzt14=from-file \"-Dq=a b\"".to_string()),
        "jarfile" => Some("-Dx=1 -jar app.jar".to_string()),
        "mainfile" => Some("-Dx=1 Main".to_string()),
        _ => None,
    })
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn zt14x11_the_variable_splits_on_white_space_and_keeps_quoted_runs() {
    assert_eq!(
        tokens("  -Xmx64m\t-Dzt14=1 \n-Da=\"b c\"'d e' ").unwrap(),
        strings(&["-Xmx64m", "-Dzt14=1", "-Da=b cd e"])
    );
    assert_eq!(tokens("").unwrap(), Vec::<String>::new());
    assert_eq!(
        tokens("-Da=\"open").unwrap_err(),
        "Error: unmatched quote in environment variable JDK_JAVA_OPTIONS"
    );
    // No argfile escapes or comments in the variable itself.
    assert_eq!(
        tokens("-Da=x#y -Db=\\n").unwrap(),
        strings(&["-Da=x#y", "-Db=\\n"])
    );
}

#[test]
fn zt14x11_argfiles_in_the_variable_expand_and_are_judged_too() {
    assert_eq!(
        tokens("-Xmx64m @opts").unwrap(),
        strings(&["-Xmx64m", "-Dzt14=from-file", "-Dq=a b"])
    );
    assert_eq!(tokens("@@lit -cp x").unwrap_err(), main_class_refusal());
    // zt t15/x12: libjli `expandArgFile` (`CFG_ERROR6`, exit 1).
    assert_eq!(
        tokens("@missing -Xmx64m").unwrap_err(),
        "Error: could not open `missing'"
    );
    assert_eq!(
        tokens("@jarfile").unwrap_err(),
        "Error: Option -jar in @jarfile is not allowed in environment variable \
         JDK_JAVA_OPTIONS"
    );
    assert_eq!(tokens("@mainfile").unwrap_err(), main_class_refusal());
    assert_eq!(tokens("@").unwrap_err(), main_class_refusal(), "`@` alone is an argument");
}

fn main_class_refusal() -> String {
    "Error: Cannot specify main class in environment variable JDK_JAVA_OPTIONS".to_string()
}

#[test]
fn zt14x11_terminal_options_and_a_main_class_are_refused() {
    for opt in [
        "-jar",
        "-m",
        "--module",
        "--module=m/p.Main",
        "--dry-run",
        "-h",
        "-?",
        "-help",
        "--help",
        "-X",
        "--help-extra",
        "-version",
        "--version",
        "-fullversion",
        "--full-version",
    ] {
        assert_eq!(
            tokens(&format!("-Xmx64m {opt}")).unwrap_err(),
            format!("Error: Option {opt} is not allowed in environment variable JDK_JAVA_OPTIONS"),
            "{opt}"
        );
    }
    assert_eq!(tokens("-Xmx64m Main").unwrap_err(), main_class_refusal());
    // A white-space option's value is not a main class.
    assert_eq!(
        tokens("-cp lib --add-modules java.sql -p mods").unwrap(),
        strings(&["-cp", "lib", "--add-modules", "java.sql", "-p", "mods"])
    );
    // ... but an `=`-form option takes no next token.
    assert_eq!(
        tokens("--add-modules=java.sql Main").unwrap_err(),
        main_class_refusal()
    );
    // Positive control: the same options pass.
    assert!(tokens("-Xmx64m -Dzt14=1 --add-modules=java.sql").is_ok());
}
