//! Plain-text output for --sources and --list.
use crate::catalog::Catalog;
use crate::format::local_time;
use crate::model::LaunchSpec;
use crate::search;
use std::collections::HashSet;
use std::fmt::Write;

pub fn describe_launch(spec: &LaunchSpec) -> String {
    let mut parts: Vec<String> = spec.env_remove.iter().map(|k| format!("-u {k}")).collect();
    parts.extend(spec.env_set.iter().map(|(k, v)| format!("{k}={v}")));
    parts.extend(spec.argv_prefix.iter().cloned());
    parts.join(" ")
}

/// agent, name, config dir, store, launch — one line per source, then warnings.
pub fn sources_report(catalog: &Catalog) -> String {
    let mut out = String::new();
    for (si, source) in catalog.sources.iter().enumerate() {
        let store = catalog
            .stores
            .iter()
            .find(|s| s.sources.contains(&si))
            .map(|s| s.path.display().to_string())
            .unwrap_or_else(|| "-".into());
        let _ = writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}",
            source.agent,
            source.name,
            source.config_dir.display(),
            store,
            describe_launch(&source.launch)
        );
    }
    for warning in &catalog.warnings {
        let _ = writeln!(out, "warning: {warning}");
    }
    out
}

/// last activity, source, running pid or "-", id, cwd, title.
pub fn list_tsv(catalog: &Catalog, query: &str) -> String {
    let all: Vec<usize> = (0..catalog.sessions.len()).collect();
    let mut order = search::fuzzy(catalog, &all, query);
    let mut seen: HashSet<usize> = order.iter().copied().collect();
    if !query.trim().is_empty() {
        for hit in search::full_text(catalog, &all, query, None).unwrap_or_default() {
            if seen.insert(hit.session) {
                order.push(hit.session);
            }
        }
    }
    let mut out = String::new();
    for i in order {
        let s = &catalog.sessions[i];
        let _ = writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}\t{}",
            local_time(s.meta.last_ts),
            catalog.sources[s.default_source].name,
            s.live
                .map(|(pid, _)| pid.to_string())
                .unwrap_or_else(|| "-".into()),
            s.meta.id,
            s.meta
                .cwd
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "-".into()),
            s.meta.title
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fake_catalog;
    use crate::model::LaunchSpec;

    #[test]
    fn describes_launch_specs() {
        let spec = LaunchSpec {
            argv_prefix: vec!["claude".into()],
            env_set: vec![("A".into(), "1".into())],
            env_remove: vec!["B".into()],
        };
        assert_eq!(describe_launch(&spec), "-u B A=1 claude");
    }

    #[test]
    fn sources_report_lists_each_source() {
        let out = sources_report(&fake_catalog());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1], "fake\ttwo\t/fake/two\t/s\tfake two");
    }

    #[test]
    fn list_includes_fuzzy_then_text_hits() {
        let c = fake_catalog();
        let all = list_tsv(&c, "");
        assert_eq!(all.lines().count(), 4);
        let row: Vec<&str> = all.lines().nth(2).unwrap().split('\t').collect();
        assert_eq!(&row[1..], &["two", "4242", "c", "/tmp", "Running thing"]);

        let text = list_tsv(&c, "pineapple");
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("\tb\t"));
    }
}
