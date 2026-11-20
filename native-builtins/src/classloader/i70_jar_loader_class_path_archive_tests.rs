use super::{jar_loader_archive, jar_loader_class_path_row};
use crate::test_utils::mock_ctx;
use std::io::Write;

/// `CRATONVM_JAR_LOADER_CLASS_PATH_ARCHIVE` (interpreter round i2 wave
/// 70, lane L5) is default off: a context that does not turn it on (the
/// mock, as a VM without the switch) lends nothing, and the jar-loader
/// row is opened from the file exactly as before.
#[test]
fn without_the_switch_the_row_is_opened_from_the_file() {
    let dir = std::env::temp_dir().join("cratonvm_i70_jar_loader_lend_off");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let jar = dir.join("lib.jar");
    {
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let mut z = zip::ZipWriter::new(std::fs::File::create(&jar).unwrap());
        z.start_file("p/A.class", opts).unwrap();
        z.write_all(b"p/A.class").unwrap();
        z.finish().unwrap();
    }
    let ctx = mock_ctx();
    let spelled = jar.to_string_lossy().into_owned();
    assert!(
        jar_loader_class_path_row(&ctx, &spelled).is_none(),
        "off: nothing lent"
    );
    let row = jar_loader_archive(&ctx, &spelled).expect("opened from the file");
    assert!(row.plain_view());
    assert_eq!(
        row.entry_bytes("p/A.class").as_deref(),
        Some(&b"p/A.class"[..])
    );
    drop(row);
    let _ = std::fs::remove_dir_all(&dir);
}
