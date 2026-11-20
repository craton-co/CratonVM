use super::{jvm_arg_bool, resolved_flags};
use crate::config::{GcAlgorithm, VmConfig};

const KEY: &str = concat!("CRATON", "VM_JMX_MANAGEABLE_EFFECT_FLAGS");

fn row(config: &VmConfig, name: &str) -> Option<(&'static str, String, &'static str, &'static str)> {
    resolved_flags(config)
        .into_iter()
        .find(|row| row.name == name)
        .map(|row| (row.ty, row.value, row.kind, row.origin))
}

#[test]
fn zt27lg1_effect_rows_follow_the_command_line() {
    let mut config = VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    };
    cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
        assert_eq!(
            row(&config, "HeapDumpOnOutOfMemoryError"),
            Some(("bool", "false".to_string(), "{manageable}", "{default}"))
        );
        assert_eq!(
            row(&config, "HeapDumpPath"),
            Some(("ccstr", String::new(), "{manageable}", "{default}"))
        );
        assert_eq!(
            row(&config, "PrintClassHistogram").map(|r| r.1),
            Some("false".to_string())
        );
        // zt t31/x10: on every collector (HotSpot lists it on Serial too).
        assert_eq!(
            row(&config, "HeapDumpBeforeFullGC"),
            Some(("bool", "false".to_string(), "{manageable}", "{default}"))
        );
        // zt t28/mf: modelled (the SIGQUIT locked-synchronizer sections).
        assert_eq!(
            row(&config, "PrintConcurrentLocks"),
            Some(("bool", "false".to_string(), "{manageable}", "{default}"))
        );
        assert_eq!(
            row(&config, "HeapDumpGzipLevel"),
            Some(("int", "0".to_string(), "{manageable}", "{default}"))
        );
        assert_eq!(
            row(&config, "G1PeriodicGCInterval").map(|r| r.1),
            Some("0".to_string())
        );
    });
    config.heap_dump_on_oom = true;
    config.heap_dump_path = Some("/tmp/d".to_string());
    config.jvm_args = vec![
        "-XX:+HeapDumpOnOutOfMemoryError".to_string(),
        "-XX:HeapDumpPath=/tmp/d".to_string(),
        "-XX:+PrintClassHistogram".to_string(),
        "-XX:-PrintClassHistogram".to_string(),
        "-XX:+PrintClassHistogram".to_string(),
    ];
    assert!(jvm_arg_bool(&config, "PrintClassHistogram", false), "the last one wins");
    assert!(!jvm_arg_bool(&config, "PrintClass", false), "a whole name only");
    cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
        assert_eq!(
            row(&config, "HeapDumpOnOutOfMemoryError").map(|r| (r.1, r.3)),
            Some(("true".to_string(), "{command line}"))
        );
        assert_eq!(
            row(&config, "HeapDumpPath").map(|r| (r.1, r.3)),
            Some(("/tmp/d".to_string(), "{command line}"))
        );
        assert_eq!(
            row(&config, "PrintClassHistogram").map(|r| (r.1, r.3)),
            Some(("true".to_string(), "{command line}"))
        );
    });
    #[cfg(feature = "zgc")]
    {
        config.gc_algorithm = GcAlgorithm::Zgc;
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert_eq!(
                row(&config, "HeapDumpAfterFullGC").map(|r| (r.0, r.1, r.2)),
                Some(("bool", "false".to_string(), "{manageable}"))
            );
        });
    }

}
