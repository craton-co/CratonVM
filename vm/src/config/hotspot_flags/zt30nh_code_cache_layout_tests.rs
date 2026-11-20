use super::code_cache_layout;
use crate::config::VmConfig;

fn layout(args: &[&str]) -> cratonvm_types::code_cache_layout::CodeCacheLayout {
    let config = VmConfig {
        jvm_args: args.iter().map(|a| a.to_string()).collect(),
        container_effective_processors: Some(4),
        ..VmConfig::default()
    };
    code_cache_layout(&config)    }

#[test]
fn zt30nh_options_shape_the_layout() {
    let d = layout(&[]);
    assert!(d.segmented);
    assert_eq!(d.inits, [2_555_904; 3]);
    assert!(!layout(&["-XX:+SegmentedCodeCache", "-XX:-SegmentedCodeCache"]).segmented);
    let small = layout(&["-XX:ReservedCodeCacheSize=64m"]);
    assert!(!small.segmented);
    assert_eq!((small.inits[0], small.reserved), (2_555_904, 64 << 20));
    assert!(layout(&["-XX:ReservedCodeCacheSize=300m"]).segmented);
    assert!(!layout(&["-XX:-TieredCompilation"]).segmented);
    assert!(!layout(&["-Xint"]).segmented);
    assert!(!layout(&["-XX:TieredStopAtLevel=1"]).segmented);
    assert!(layout(&["-XX:ReservedCodeCacheSize=64m", "-XX:+SegmentedCodeCache"]).segmented);
    #[cfg(not(windows))]
    assert_eq!(
        layout(&["-XX:InitialCodeCacheSize=8m"]).inits,
        [5_836_800, 8_388_608, 8_388_608]
    );
    assert_eq!(
        layout(&["-XX:InitialCodeCacheSize=8m", "-XX:ActiveProcessorCount=16"]).inits[1],
        8_388_608
    );
}
