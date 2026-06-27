//! The depend-phase metadata generator.
//!
//! Sources the ebuild with a working `inherit` (the stock `depend` phase) and
//! reads back the package metadata keys. The build engine uses this to derive
//! the `INHERITED`/`INHERIT` provenance from the eclasses actually sourced
//! rather than copying the cache token, and it is the fallback that regenerates
//! metadata when a repository's md5-cache entry is missing or stale.

use std::collections::BTreeMap;
use std::path::Path;

use tracing::instrument;

use crate::bashlib::{self, PhaseLibrary};
use crate::error::Result;
use crate::runner::{CommandRunner, CommandSpec};

/// Source the ebuild with `inherit` active and return its metadata keys.
///
/// `base_env` must carry the package identity (`EAPI`, `CATEGORY`, `PF`, `P`,
/// ...) and `PORTAGE_ECLASS_LOCATIONS` so `inherit` can find eclasses. The
/// returned map is keyed by metadata name (`INHERITED`, `INHERIT`, `IUSE`,
/// `DEPEND`, `DEFINED_PHASES`, ...); a key whose value the ebuild left empty is
/// present with an empty string. An empty map is returned when the generator
/// process produces no tagged output, so callers fall back to cached metadata.
#[instrument(name = "generate_metadata", skip_all)]
pub fn generate_metadata<R: CommandRunner>(
    runner: &R,
    library: &PhaseLibrary,
    ebuild_path: &Path,
    base_env: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    let mut env = base_env.clone();
    env.insert("EBUILD_PHASE".to_string(), "depend".to_string());

    let mut script = String::new();
    for lib in &library.scripts {
        script.push_str(&format!(
            ". {} || exit 1\n",
            shquote(&lib.to_string_lossy())
        ));
    }
    script.push_str(&format!(
        "[ -f {ebuild} ] && {{ . {ebuild} || die \"error sourcing ebuild\"; }}\n\
         {emit}\n",
        ebuild = shquote(&ebuild_path.to_string_lossy()),
        emit = bashlib::EMIT_FUNC,
    ));

    let spec = CommandSpec {
        program: "bash".to_string(),
        args: vec!["-c".to_string(), script],
        env,
        cwd: ebuild_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default(),
        // Capture stdout directly rather than into a log file so the tagged
        // metadata lines can be parsed.
        log_path: None,
        // Metadata generation runs no build phase, so it needs no isolation.
        isolation: crate::isolation::Isolation::default(),
    };

    let output = runner
        .run(&spec)
        .map_err(|e| crate::error::BuildError::Ipc { reason: e.reason })?;
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(parse_metadata(&text))
}

/// Parse the `MORAINE_META key=value` lines the emitter prints.
fn parse_metadata(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MORAINE_META ")
            && let Some((key, value)) = rest.split_once('=')
        {
            out.insert(key.to_string(), value.to_string());
        }
    }
    out
}

/// Quote a string for safe inclusion in a single-quoted bash context.
fn shquote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r#"'\''"#))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tagged_metadata_lines() {
        let text = "noise\nMORAINE_META IUSE=ssl threads\nMORAINE_META INHERITED= foo bar\n\
                    not tagged\nMORAINE_META DEFINED_PHASES=compile install\n";
        let meta = parse_metadata(text);
        assert_eq!(meta.get("IUSE").map(String::as_str), Some("ssl threads"));
        assert_eq!(meta.get("INHERITED").map(String::as_str), Some(" foo bar"));
        assert_eq!(
            meta.get("DEFINED_PHASES").map(String::as_str),
            Some("compile install")
        );
        assert_eq!(meta.get("noise"), None);
    }
}
