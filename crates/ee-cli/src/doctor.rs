//! `ee do doctor` command.
use super::language_cmd::print_language_query_diagnostics;
use super::*;

// ── Subcommand handlers ───────────────────────────────────────────────────────

pub(crate) fn doctor_report(config_path: Option<&PathBuf>) -> String {
    let mut report = String::new();
    writeln!(&mut report, "ee do doctor").unwrap();
    writeln!(&mut report, "─────────").unwrap();

    if let Some(explicit) = config_path {
        let status = if explicit.exists() { "found" } else { "not found" };
        writeln!(&mut report, "  --config {explicit:?}  [{status}]").unwrap();
    } else {
        let search_report = config::config_search_report(None);
        writeln!(&mut report, "  anchor {:?}", search_report.anchor).unwrap();
        writeln!(&mut report, "  layers (low -> high)").unwrap();
        for layer in search_report.layers {
            let status = if layer.loaded {
                "loaded"
            } else if layer.exists {
                "skipped"
            } else {
                "not found"
            };
            write!(&mut report, "  {:?}  [{}] [{}]", layer.path, layer.kind.label(), status)
                .unwrap();
            if let Some(root) = layer.root {
                write!(&mut report, " [root={root}]").unwrap();
            }
            if let Some(note) = layer.note {
                write!(&mut report, " {note}").unwrap();
            }
            writeln!(&mut report).unwrap();
        }
        if !search_report.editorconfig_applies {
            writeln!(
                &mut report,
                "  .editorconfig  [file-specific] [not evaluated without file path]"
            )
            .unwrap();
        }
    }

    writeln!(&mut report).unwrap();
    writeln!(&mut report, "  log files").unwrap();
    for candidate in logs::discover_log_paths() {
        let status = if candidate.path.is_file() { "found" } else { "not found" };
        writeln!(&mut report, "  {:?}  [{}] [{}]", candidate.path, candidate.label, status)
            .unwrap();
    }

    report
}

pub(crate) fn cmd_doctor(config_path: Option<&PathBuf>) {
    print!("{}", doctor_report(config_path));
    println!();
    print_language_query_diagnostics();
    println!();
    println!("No problems detected.");
}
