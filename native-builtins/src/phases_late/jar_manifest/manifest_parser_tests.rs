use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn parses_signed_jar_entry_sections_after_main_attributes() {
    let parsed = p59_parse_manifest_bytes(
        b"Manifest-Version: 1.0\r\nCreated-By: CratonVM\r\n\r\nName: com/example/App.class\r\nSHA-256-Digest: abc\r\n def\r\n\r\nName: META-INF/services/example\r\nSHA-512-Digest: ghi\r\n\r\n",
    )
    .expect("valid signed-jar manifest");

    assert_eq!(
        parsed.main,
        vec![
            ("Manifest-Version".into(), "1.0".into()),
            ("Created-By".into(), "CratonVM".into())
        ]
    );
    assert_eq!(parsed.entries.len(), 2);
    assert_eq!(parsed.entries[0].0, "com/example/App.class");
    assert_eq!(
        parsed.entries[0].1,
        vec![("SHA-256-Digest".into(), "abcdef".into())]
    );
    assert_eq!(parsed.entries[1].0, "META-INF/services/example");
    assert_eq!(
        parsed.entries[1].1,
        vec![("SHA-512-Digest".into(), "ghi".into())]
    );

    let entry_only =
        p59_parse_manifest_bytes(b"\r\nName: a/b/C.class\r\nSHA1-Digest: 0000\r\n\r\n")
            .expect("valid manifest with an empty main section");
    assert!(entry_only.main.is_empty());
    assert_eq!(
        entry_only.entries,
        vec![(
            "a/b/C.class".into(),
            vec![("SHA1-Digest".into(), "0000".into())],
        )]
    );
}
