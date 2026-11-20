#[test]
fn report_value_size() {
    eprintln!(
        "size_of::<Value>()        = {}",
        std::mem::size_of::<cratonvm_types::Value>()
    );
    eprintln!(
        "16-slot args_buf bytes    = {}",
        16 * std::mem::size_of::<cratonvm_types::Value>()
    );
    eprintln!(
        "size_of::<CompactValue>() = {}",
        std::mem::size_of::<cratonvm_types::CompactValue>()
    );
}
