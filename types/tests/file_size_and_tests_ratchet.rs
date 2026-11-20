use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// Grandfathered `.rs` files exceeding 5,000 lines (and the single >10,000-line
/// file `jit/src/lib_parts/ir_tier_attempt.rs`). No file in this table may grow past
/// its recorded line count, and no new `.rs` file may exceed 5,000 lines.
const OVER_5K_BASELINE: &[(&str, usize)] = &[
    ("classloading/src/class_path.rs", 9856),
    ("classloading/src/class_path/tests.rs", 5051),
    ("docs/internal/archive/retired-tests/native-builtins-stub_ratchet.rs", 6522),
    ("gc-core/src/arena.rs", 7617),
    ("gc-core/src/gc_metrics.rs", 5262),
    ("gc-core/src/gc_quiescence.rs", 7705),
    ("gc-core/src/reference.rs", 7651),
    ("gc-gen/src/gen_heap/collect_garbage_inner_with_pins.rs", 6068),
    ("gc-gen/src/gen_heap/sweep_young_non_moving.rs", 6423),
    ("gc-gen/src/old_gen.rs", 9172),
    ("gc-gen/src/old_gen/tests.rs", 6092),
    ("gc-zgc/src/zgc/mark.rs", 6243),
    ("jit-cuda/src/lowering/emit.rs", 5563),
    ("jit/src/deopt.rs", 8574),
    ("jit/src/ea_ir_bridge.rs", 6282),
    ("jit/src/ir/ir_core_tests.rs", 6562),
    ("jit/src/ir/attach_sb_virtual_builders.rs", 6036),
    ("jit/src/ir_check_elim.rs", 5426),
    ("jit/src/ir_lower/ir_ic_cascade_admits_kind.rs", 6807),
    ("jit/src/ir_optimize/loop_field_forward.rs", 5717),
    ("jit/src/ir_schedule.rs", 6311),
    ("jit/src/lib_parts/ir_tier_attempt.rs", 10757),
    ("jit/src/lib_parts/build_single_pass_tables.rs", 5115),
    ("jit/src/platform.rs", 5419),
    ("jit/src/regalloc/linear_scan_tests.rs", 5371),
    ("jit/src/runtime_lowering.rs", 6733),
    ("jit/src/tiered.rs", 8701),
    ("jit/src/x64/deopt_stubs.rs", 5812),
    ("jit/src/x64/driver.rs", 5634),
    ("jit/src/x64/inlining.rs", 8250),
    ("jit/src/x64/isel.rs", 6464),
    ("jit/src/x64/licm.rs", 6106),
    ("jit/src/x64/objects.rs", 7378),
    ("jit/src/x64/op_invoke/walk_invoke_instance.rs", 6658),
    ("jit/tests/ir_vs_singlepass.rs", 8146),
    ("native-awt/src/natives.rs", 5387),
    ("native-builtins/src/antlr_intrinsics.rs", 7190),
    ("native-builtins/src/cglib_enhancer.rs", 5335),
    ("native-builtins/src/crypto_impl.rs", 6631),
    ("native-builtins/src/http_url_connection.rs", 8151),
    ("native-builtins/src/jboss_jdkspecific.rs", 5141),
    ("native-builtins/src/jca/cipher.rs", 7015),
    ("native-builtins/src/jca/provider_chain.rs", 9227),
    ("native-builtins/src/keystore.rs", 6292),
    ("native-builtins/src/lang_invoke/call_site.rs", 9615),
    ("native-builtins/src/lang_invoke/combinators.rs", 6074),
    ("native-builtins/src/lang_invoke/direct_dispatch.rs", 9075),
    ("native-builtins/src/lang_math.rs", 9156),
    ("native-builtins/src/lang_misc.rs", 7312),
    ("native-builtins/src/lang_stackwalker.rs", 7327),
    ("native-builtins/src/locale_resources.rs", 5470),
    ("native-builtins/src/math_bignum.rs", 9357),
    ("native-builtins/src/panama/tests.rs", 6073),
    ("native-builtins/src/phases_late.rs", 9554),
    ("native-builtins/src/phases_late/concurrent.rs", 9886),
    ("native-builtins/src/phases_late/foreign_ffm.rs", 9446),
    ("native-builtins/src/phases_late/net_channels.rs", 6279),
    ("native-builtins/src/phases_late/reflect_invoke.rs", 7272),
    ("native-builtins/src/phases_late/ssl_security/register_p68_ssl.rs", 5179),
    ("native-builtins/src/phases_late/streams.rs", 5213),
    ("native-builtins/src/properties_sidetable.rs", 6435),
    ("native-builtins/src/reflect_annotations.rs", 8353),
    ("native-builtins/src/serialization.rs", 6089),
    ("native-builtins/src/spring_startup_bootstrap.rs", 5181),
    ("native-builtins/src/test_frameworks.rs", 5598),
    ("native-builtins/src/util_time.rs", 5995),
    ("native-builtins/src/x509_manager.rs", 6212),
    ("native-builtins/tests/registrar_drift.rs", 5930),
    ("native-collections/src/array_list.rs", 5885),
    ("native-collections/src/collections_util.rs", 5800),
    ("native-collections/src/concurrent_queue.rs", 6819),
    ("native-collections/src/deque.rs", 6220),
    ("native-collections/src/hash_map.rs", 9426),
    ("native-collections/src/hash_set.rs", 6560),
    ("native-collections/src/tree_map.rs", 9649),
    ("native-io/src/net.rs", 6322),
    ("native-io/src/nio_selector.rs", 6033),
    ("native-io/src/process.rs", 7735),
    ("native-io/src/socket_channel.rs", 8083),
    ("types/src/flag_groups.rs", 9774),
    ("types/src/flags.rs", 6570),
    ("vm-cli/src/main_parts/arch_label.rs", 5531),
    ("vm-cli/src/tests.rs", 5131),
    ("vm/src/config.rs", 5045),
    ("vm/src/jit/helpers/access.rs", 8106),
    ("vm/src/jit/helpers/alloc.rs", 5604),
    ("vm/src/jit/helpers/table.rs", 5406),
    ("vm/src/jit/helpers/typecheck.rs", 5447),
    ("vm/src/jit/xt_root_scan.rs", 7717),
    ("vm/src/runtime/exceptions.rs", 8563),
    ("vm/src/runtime/frame.rs", 6488),
    ("vm/src/runtime/instrument.rs", 6355),
    ("vm/src/runtime/interpreter/constants.rs", 8910),
    ("vm/src/runtime/interpreter/dispatch_virtual.rs", 8168),
    ("vm/src/runtime/interpreter/exception_dispatch.rs", 5264),
    ("vm/src/runtime/interpreter/gc_events.rs", 9952),
    ("vm/src/runtime/interpreter/invoke.rs", 9521),
    ("vm/src/runtime/interpreter/jit_bridge/folded_handle_member_row.rs", 6379),
    ("vm/src/runtime/interpreter/jvmti_events.rs", 5939),
    ("vm/src/runtime/interpreter/lambda.rs", 8205),
    ("vm/src/runtime/interpreter/native_override.rs", 9300),
    ("vm/src/runtime/interpreter/opcodes.rs", 7483),
    ("vm/src/runtime/interpreter/execute_frame_from_index_loop.rs", 6227),
    ("vm/src/runtime/interpreter/tests.rs", 6118),
    ("vm/src/runtime/jvmti.rs", 5334),
    ("vm/src/runtime/offload.rs", 9810),
    ("vm/src/runtime/serviceability.rs", 8411),
    ("vm/src/runtime/unified_logging.rs", 6055),
    ("vm/src/threading/gc_barrier.rs", 5501),
    ("vm/src/threading/jvm_thread.rs", 6309),
    ("vm/src/threading/monitor.rs", 9025),
    ("vm/src/vm/vm_exec/invoke_on_class_shared_inner.rs", 5980),
    ("vm/src/vm/vm_init/real_jdk_to_array_typed.rs", 5072),
    ("vm/src/vm/vm_init/tests/suite_atomics_zip.rs", 9148),
    ("vm/src/vm/vm_init/tests/suite_basics.rs", 9587),
    ("vm/src/vm/vm_init/tests/suite_blocking_ffi.rs", 9939),
    ("vm/src/vm/vm_init/tests/suite_classloading_jtreg.rs", 9894),
    ("vm/src/vm/vm_init/tests/suite_streams.rs", 9795),
    ("vm/src/vm/vm_init/tests/suite_vm_init.rs", 8586),
    ("vm/src/vm/vm_util.rs", 7455),
    ("vm/tests/interpreter_tests.rs", 5997),
];

fn walk_rs(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        if path.is_dir() {
            if matches!(name_s.as_ref(), "target" | ".git" | "vendor" | "apps") {
                continue;
            }
            walk_rs(&path, out);
        } else if name_s.ends_with(".rs") {
            out.push(path);
        }
    }
}

fn collect_cfg_test_modules(root: &Path, files: &[std::path::PathBuf]) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for path in files {
        let Ok(content) = fs::read_to_string(path) else { continue };
        let lines: Vec<&str> = content.lines().collect();
        let parent = path.parent().unwrap().strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
        let stem = path.file_stem().unwrap().to_string_lossy();
        let mod_dir = if stem == "mod" || stem == "lib" || stem == "main" {
            parent.clone()
        } else {
            format!("{parent}/{stem}")
        };
        let mut i = 0;
        while i < lines.len() {
            let t = lines[i].trim();
            if t.starts_with("#[cfg(test)]") || t.starts_with("#[cfg(all(test,") {
                let mut j = i + 1;
                while j < lines.len() && (lines[j].trim().starts_with("#[") || lines[j].trim().starts_with("//") || lines[j].trim().is_empty()) {
                    j += 1;
                }
                if j < lines.len() {
                    let m = lines[j].trim();
                    let rest = m.strip_prefix("pub ").unwrap_or(m);
                    if let Some(after_mod) = rest.strip_prefix("mod ") {
                        if let Some(name) = after_mod.split(';').next().filter(|_| after_mod.contains(';')) {
                            let name = name.trim();
                            out.insert(format!("{mod_dir}/{name}.rs"));
                            out.insert(format!("{mod_dir}/{name}/"));
                        }
                    }
                }
            }
            i += 1;
        }
    }
    out
}

fn is_test_only_file(rel: &str, cfg_test_mods: &std::collections::HashSet<String>) -> bool {
    rel.contains("/tests/")
        || rel.ends_with("/tests.rs")
        || rel.ends_with("_tests.rs")
        || rel.ends_with("_test.rs")
        || rel.ends_with("_bench.rs")
        || rel.contains("test_utils")
        || rel.contains("witness")
        || rel.contains("_probe.rs")
        || rel.ends_with("golden.rs")
        || rel.ends_with("_audit.rs")
        || rel.ends_with("define_rows.rs")
        || cfg_test_mods.contains(rel)
        || cfg_test_mods.iter().any(|p| p.ends_with('/') && rel.starts_with(p.as_str()))
        || rel.starts_with("tools/")
        || rel.starts_with("fuzz/")
        || rel.starts_with("docs/")
}

#[test]
fn file_size_ratchet_and_no_tests_in_production_files() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let baseline: HashMap<&str, usize> = OVER_5K_BASELINE.iter().copied().collect();
    let mut files = Vec::new();
    walk_rs(root, &mut files);
    let cfg_test_mods = collect_cfg_test_modules(root, &files);

    let mut violations = Vec::new();
    for path in files {
        let rel = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
        let Ok(content) = fs::read_to_string(&path) else { continue };
        let lines = content.lines().count();

        if lines > 5000 && !rel.starts_with("docs/") {
            match baseline.get(rel.as_str()) {
                Some(&max_lines) => {
                    if lines > max_lines {
                        violations.push(format!(
                            "{rel}: grew from {max_lines} to {lines} lines (file size ratchet)"
                        ));
                    }
                }
                None => {
                    violations.push(format!(
                        "{rel}: new file has {lines} lines (> 5000 limit)"
                    ));
                }
            }
        }

        if rel.contains("/src/") && !is_test_only_file(&rel, &cfg_test_mods) {
            for (idx, line) in content.lines().enumerate() {
                let t = line.trim_start();
                if t.starts_with("#[test]") || (t.starts_with("#[cfg(test)]") && content.lines().nth(idx + 1).map_or(false, |nx| nx.trim_start().starts_with("mod ") && nx.contains('{'))) {
                    violations.push(format!(
                        "{rel}:{}: production file contains inline test item",
                        idx + 1
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "File size / production-tests ratchet violations:\n{}",
        violations.join("\n")
    );
}
