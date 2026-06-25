//! The `emerge`-style merge-list and tree renderer.
//!
//! Rendering is a pure function of the resolved plan. The binary builds a
//! [`MergePlan`] of [`MergeEntry`] values from `moraine-resolve`'s ordered task
//! list joined with installed state, then this module formats it. Nothing here
//! recomputes resolution decisions, which keeps the output snapshot-testable
//! against constructed fixtures with no real Gentoo system.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::io::IsTerminal as _;

use moraine_config::UseExpandGroup;
use moraine_resolve::{AutounmaskChange, DepEdge};
use tracing::instrument;

/// Render the autounmask changes emerge-style: each soft-masked package shows the
/// `# required by` chain up to the requested argument, then the
/// `package.accept_keywords` / `package.license` line to add. Returns an empty
/// string when no changes are required.
pub fn render_autounmask(
    changes: &[AutounmaskChange],
    plan: &MergePlan,
    edges: &[DepEdge],
    args: &BTreeSet<String>,
) -> String {
    if changes.is_empty() {
        return String::new();
    }

    // `cp -> cp-version::repo` for every merged entry, used in the reason chain.
    let cpv: HashMap<&str, String> = plan
        .entries
        .iter()
        .map(|e| {
            let mut s = format!("{}-{}", e.cp, e.version);
            if let Some(r) = &e.repository {
                let _ = write!(s, "::{r}");
            }
            (e.cp.as_str(), s)
        })
        .collect();

    // `child cp -> requirer cps`, from the dependency edges.
    let mut parents: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in edges {
        if e.from != e.to {
            parents
                .entry(e.to.as_str())
                .or_default()
                .push(e.from.as_str());
        }
    }

    // A chain ends at a requested argument or a graph root (no requirer), which
    // also breaks dependency cycles cleanly.
    let is_terminus = |cp: &str| args.contains(cp) || parents.get(cp).is_none_or(Vec::is_empty);
    let req_line = |cp: &str| {
        let label = cpv.get(cp).cloned().unwrap_or_else(|| cp.to_owned());
        paint(&format!("# required by {label}"), "2")
    };
    let arg_line = |cp: &str| {
        let pn = cp.rsplit('/').next().unwrap_or(cp);
        paint(&format!("# required by {pn} (argument)"), "2")
    };

    // Breadth-first search up through requirers to the nearest terminus, so the
    // shortest representative path is shown, matching emerge's intent.
    let chain_for = |start: &str| -> Vec<String> {
        if is_terminus(start) {
            return vec![arg_line(start)];
        }
        let mut queue: std::collections::VecDeque<&str> = std::collections::VecDeque::from([start]);
        let mut prev: HashMap<&str, &str> = HashMap::new();
        let mut visited: HashSet<&str> = HashSet::from([start]);
        let mut terminus: Option<&str> = None;
        'bfs: while let Some(node) = queue.pop_front() {
            for &p in parents.get(node).map(Vec::as_slice).unwrap_or(&[]) {
                if !visited.insert(p) {
                    continue;
                }
                prev.insert(p, node);
                if is_terminus(p) {
                    terminus = Some(p);
                    break 'bfs;
                }
                queue.push_back(p);
            }
        }
        let Some(term) = terminus else {
            return vec![arg_line(start)];
        };
        let mut path = Vec::new();
        let mut cur = term;
        while cur != start {
            path.push(cur);
            cur = prev[cur];
        }
        path.reverse();
        let mut out: Vec<String> = path.iter().map(|cp| req_line(cp)).collect();
        out.push(arg_line(term));
        out
    };

    let mut keyword_block: Vec<String> = Vec::new();
    let mut license_block: Vec<String> = Vec::new();
    for c in changes {
        let atom = paint(&format!("={}-{}", c.cp, c.version), "32");
        if let Some(kw) = &c.change.keyword {
            keyword_block.extend(chain_for(&c.cp));
            keyword_block.push(format!("{atom} {kw}"));
        }
        if !c.change.licenses.is_empty() {
            license_block.extend(chain_for(&c.cp));
            license_block.push(format!("{atom} {}", c.change.licenses.join(" ")));
        }
    }

    let mut out = String::new();
    if !keyword_block.is_empty() {
        let _ = writeln!(
            out,
            "\n{}",
            paint(
                "The following keyword changes are necessary to proceed:",
                "33"
            )
        );
        out.push_str(
            " (see \"package.accept_keywords\" in the portage(5) man page for more details)\n",
        );
        for line in keyword_block {
            let _ = writeln!(out, "{line}");
        }
    }
    if !license_block.is_empty() {
        let _ = writeln!(
            out,
            "\n{}",
            paint(
                "The following license changes are necessary to proceed:",
                "33"
            )
        );
        out.push_str(" (see \"package.license\" in the portage(5) man page for more details)\n");
        for line in license_block {
            let _ = writeln!(out, "{line}");
        }
    }
    out
}

/// Fold flat `prefix_value` USE flags into their USE_EXPAND group, stripping the
/// prefix for display and marking hidden groups for suppression. `groups` must
/// be ordered longest-prefix first so the most specific group wins.
pub fn apply_use_expand_groups(flags: &mut [UseFlag], groups: &[UseExpandGroup]) {
    for flag in flags.iter_mut() {
        if flag.group.is_some() {
            continue;
        }
        if let Some(g) = groups.iter().find(|g| flag.name.starts_with(&g.prefix)) {
            flag.name = flag.name[g.prefix.len()..].to_owned();
            flag.group = Some(g.name.clone());
            flag.hidden = g.hidden;
        }
    }
}

/// Whether to emit ANSI color: true only when stdout is an interactive terminal,
/// so captured output (tests, pipes, redirects) stays plain.
fn color_enabled() -> bool {
    std::io::stdout().is_terminal()
}

/// Wrap `text` in an ANSI color when color is enabled.
fn paint(text: &str, code: &str) -> String {
    if color_enabled() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

/// How a task changes the installed state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// A package not previously installed.
    New,
    /// A higher version replacing the installed one.
    Upgrade,
    /// A lower version replacing the installed one.
    Downgrade,
    /// The same version reinstalled.
    Reinstall,
    /// A rebuild forced by a slot or sub-slot change.
    Rebuild,
    /// An uninstall, for example a blocker removal.
    Uninstall,
}

impl Operation {
    /// The single-letter `emerge` indicator for the operation.
    pub fn letter(self) -> char {
        match self {
            Operation::New => 'N',
            Operation::Upgrade => 'U',
            Operation::Downgrade => 'D',
            Operation::Reinstall => 'R',
            Operation::Rebuild => 'r',
            Operation::Uninstall => 'C',
        }
    }
}

/// How the selected version is accepted by visibility rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Acceptance {
    /// Accepted as a stable keyword.
    #[default]
    Stable,
    /// Accepted only via a testing (`~arch`) keyword.
    Testing,
    /// Accepted only because a mask was lifted.
    Masked,
}

/// A USE-flag in the diff between installed and selected state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UseFlag {
    /// The flag name. For USE_EXPAND members this is the bare value.
    pub name: String,
    /// Whether the flag is enabled in the selected build.
    pub enabled: bool,
    /// Whether the flag's state changed relative to the installed package.
    pub changed: bool,
    /// Whether the flag was enabled on the installed package but is not part of
    /// the selected build (a removed flag).
    pub removed: bool,
    /// The USE_EXPAND group this flag belongs to, if any. `None` is the plain
    /// USE group.
    pub group: Option<String>,
    /// Whether the flag's group is hidden and should be suppressed.
    pub hidden: bool,
    /// Whether the flag is forced or masked (`use.force`/`use.mask`), shown
    /// parenthesized like `emerge`.
    pub forced: bool,
}

/// One entry in the merge plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeEntry {
    /// The `category/package`.
    pub cp: String,
    /// The selected version.
    pub version: String,
    /// The previously installed version, when replacing one.
    pub old_version: Option<String>,
    /// The operation this entry performs.
    pub operation: Operation,
    /// The keyword/mask acceptance of the selected version.
    pub acceptance: Acceptance,
    /// The selected slot.
    pub slot: String,
    /// The selected sub-slot, if any.
    pub subslot: Option<String>,
    /// The source repository.
    pub repository: Option<String>,
    /// Whether this entry installs a binary package rather than building from
    /// source.
    pub binary: bool,
    /// Whether the binary package is fetched from a binhost (`g`).
    pub fetched: bool,
    /// The binary package build id, shown as a `-N` version suffix like `emerge`.
    pub build_id: Option<String>,
    /// The USE-flag diff against the installed package.
    pub use_flags: Vec<UseFlag>,
    /// The download size in bytes when a fetch is required.
    pub fetch_size: Option<u64>,
    /// The `category/package` values that pulled this entry in, for the tree.
    pub parents: Vec<String>,
}

/// A complete merge plan ready to render.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergePlan {
    /// The entries in serialized merge order.
    pub entries: Vec<MergeEntry>,
}

/// The non-default slot considered the default.
const DEFAULT_SLOT: &str = "0";

/// Render the flat merge list with summary totals.
///
/// `verbose` adds the repository column for every entry. The output ends with
/// the package count and total download size in human-readable units.
#[instrument(skip(plan), fields(entries = plan.entries.len()))]
pub fn render_merge_list(plan: &MergePlan, verbose: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "These are the packages that would be merged, in order:"
    );
    let _ = writeln!(out);
    for entry in &plan.entries {
        let _ = writeln!(out, "{}", render_entry(entry, verbose));
    }
    let _ = writeln!(out);
    out.push_str(&render_totals(plan));
    out
}

/// Render a single `[ebuild ...]` merge-list line.
pub fn render_entry(entry: &MergeEntry, verbose: bool) -> String {
    // `emerge` shows `:slot/subslot` after the version when the slot is not the
    // default or carries a distinguishing sub-slot (for example `ghc-9.2.8:0/9.2.8`).
    let mut slot_suffix = String::new();
    let interesting_subslot = entry.subslot.as_deref().is_some_and(|s| s != entry.slot);
    if entry.slot != DEFAULT_SLOT || interesting_subslot {
        let mut slot = entry.slot.clone();
        if let Some(sub) = &entry.subslot {
            let _ = write!(slot, "/{sub}");
        }
        let _ = write!(slot_suffix, ":{slot}");
    }

    // Color each element distinctly: a cyan `binary` / green `ebuild` label, the
    // operation indicator in its own color, a green atom, a blue repository, and
    // a yellow size, so the parts stay visually separable.
    let label = if entry.binary {
        paint("binary", "36")
    } else {
        paint("ebuild", "32")
    };
    let indicator = paint(&indicator_block(entry), operation_color(entry.operation));
    // A binary package shows its build id as a `-N` version suffix.
    let mut version = entry.version.clone();
    if entry.binary
        && let Some(build_id) = &entry.build_id
    {
        let _ = write!(version, "-{build_id}");
    }
    let mut line = format!(
        "[{label} {indicator}] {}-{version}{slot_suffix}",
        paint(&entry.cp, "32"),
    );
    if let Some(old) = &entry.old_version {
        let _ = write!(line, " {}", paint(&format!("[{old}]"), "33"));
    }

    if verbose && let Some(repo) = &entry.repository {
        let _ = write!(line, "{}", paint(&format!("::{repo}"), "34"));
    }

    let use_str = render_use_string(&entry.use_flags);
    if !use_str.is_empty() {
        let _ = write!(line, " {use_str}");
    }

    if let Some(size) = entry.fetch_size {
        let _ = write!(line, " {}", paint(&human_size(size), "33"));
    }

    line
}

/// Build the operation, keyword/mask, and binhost indicator block.
fn indicator_block(entry: &MergeEntry) -> String {
    let mut block = String::new();
    block.push(entry.operation.letter());
    match entry.acceptance {
        Acceptance::Stable => {}
        Acceptance::Testing => block.push_str(" ~"),
        Acceptance::Masked => block.push_str(" *"),
    }
    if entry.fetched {
        block.push_str(" g");
    }
    block
}

/// The ANSI color code for an operation's indicator.
fn operation_color(op: Operation) -> &'static str {
    match op {
        Operation::New => "32",                            // green
        Operation::Upgrade => "36",                        // cyan
        Operation::Downgrade => "34",                      // blue
        Operation::Reinstall | Operation::Rebuild => "33", // yellow
        Operation::Uninstall => "31",                      // red
    }
}

/// Render the `_create_use_string`-style USE diff.
///
/// Plain flags come first, then USE_EXPAND groups in name order. Hidden groups
/// are suppressed entirely. Changed flags carry a `*` marker, removed flags are
/// shown as `(-flag*)`, disabled flags carry a leading `-`.
pub fn render_use_string(flags: &[UseFlag]) -> String {
    let visible: Vec<&UseFlag> = flags.iter().filter(|f| !f.hidden).collect();
    if visible.is_empty() {
        return String::new();
    }

    let plain: Vec<&UseFlag> = visible
        .iter()
        .copied()
        .filter(|f| f.group.is_none())
        .collect();
    let mut groups: Vec<String> = visible
        .iter()
        .filter_map(|f| f.group.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    groups.sort();

    let mut sections: Vec<String> = Vec::new();
    let plain_body = render_flag_group(&plain);
    if !plain_body.is_empty() {
        sections.push(format!("USE=\"{plain_body}\""));
    }
    for group in groups {
        let members: Vec<&UseFlag> = visible
            .iter()
            .copied()
            .filter(|f| f.group.as_deref() == Some(group.as_str()))
            .collect();
        let body = render_flag_group(&members);
        if !body.is_empty() {
            let var = group.to_uppercase();
            sections.push(format!("{var}=\"{body}\""));
        }
    }
    sections.join(" ")
}

/// Render the flag tokens for one group.
///
/// Tokens are grouped enabled-first, then disabled, then removed (Portage's
/// default, non-`--alphabetical` order). Each group preserves the caller's
/// alphabetical order.
fn render_flag_group(flags: &[&UseFlag]) -> String {
    let mut enabled: Vec<String> = Vec::new();
    let mut disabled: Vec<String> = Vec::new();
    let mut removed: Vec<String> = Vec::new();
    for flag in flags {
        if flag.removed {
            // Removed flags: yellow, matching `emerge`'s `(-flag*)`.
            removed.push(paint(&format!("(-{}*)", flag.name), "33"));
            continue;
        }
        let mark = if flag.changed { "*" } else { "" };
        // A changed flag is green; otherwise enabled is red and disabled blue, as
        // `emerge` colors them, so a state change stands out from a fixed flag.
        let sign = if flag.enabled { "" } else { "-" };
        let color = if flag.changed {
            "32"
        } else if flag.enabled {
            "31"
        } else {
            "34"
        };
        let body = format!("{sign}{}{mark}", flag.name);
        let mut token = paint(&body, color);
        if flag.forced {
            // Forced/masked flags are parenthesized, as `emerge` shows them.
            token = format!("({token})");
        }
        if flag.enabled {
            enabled.push(token);
        } else {
            disabled.push(token);
        }
    }
    enabled.extend(disabled);
    enabled.extend(removed);
    enabled.join(" ")
}

/// Render the package-count and total-download summary.
pub fn render_totals(plan: &MergePlan) -> String {
    let merge_entries: Vec<&MergeEntry> = plan
        .entries
        .iter()
        .filter(|e| e.operation != Operation::Uninstall)
        .collect();
    let merges = merge_entries.len();
    let new = merge_entries
        .iter()
        .filter(|e| e.operation == Operation::New)
        .count();
    let binary = merge_entries.iter().filter(|e| e.binary).count();
    let uninstalls = plan
        .entries
        .iter()
        .filter(|e| e.operation == Operation::Uninstall)
        .count();
    let total: u64 = plan.entries.iter().filter_map(|e| e.fetch_size).sum();

    let mut breakdown = Vec::new();
    if new > 0 {
        breakdown.push(format!("{new} new"));
    }
    if binary > 0 {
        breakdown.push(format!("{binary} binary"));
    }
    if uninstalls > 0 {
        breakdown.push(format!("{uninstalls} uninstall"));
    }
    let breakdown = if breakdown.is_empty() {
        String::new()
    } else {
        format!(" ({})", breakdown.join(", "))
    };

    format!(
        "Total: {merges} package{}{breakdown}, Size of downloads: {}\n",
        if merges == 1 { "" } else { "s" },
        human_size(total)
    )
}

/// Render the merge list as an indented dependency tree.
///
/// Each entry is shown under the parents that pulled it in. Entries with no
/// recorded parent are treated as roots. The tree is derived from the same plan
/// as the flat list.
#[instrument(skip(plan), fields(entries = plan.entries.len()))]
pub fn render_tree(plan: &MergePlan, verbose: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Dependency tree:");
    let _ = writeln!(out);

    let roots: Vec<&MergeEntry> = plan
        .entries
        .iter()
        .filter(|e| e.parents.is_empty())
        .collect();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for root in roots {
        render_tree_node(plan, root, 0, verbose, &mut seen, &mut out);
    }
    out.push('\n');
    out.push_str(&render_totals(plan));
    out
}

fn render_tree_node(
    plan: &MergePlan,
    entry: &MergeEntry,
    depth: usize,
    verbose: bool,
    seen: &mut BTreeSet<String>,
    out: &mut String,
) {
    let indent = "  ".repeat(depth);
    let _ = writeln!(out, "{indent}{}", render_entry(entry, verbose));
    if !seen.insert(entry.cp.clone()) {
        return;
    }
    let children: Vec<&MergeEntry> = plan
        .entries
        .iter()
        .filter(|child| child.parents.iter().any(|p| p == &entry.cp))
        .collect();
    for child in children {
        render_tree_node(plan, child, depth + 1, verbose, seen, out);
    }
    seen.remove(&entry.cp);
}

/// Format a byte count in human-readable units.
///
/// Uses binary units (KiB, MiB, ...) with two decimals above bytes, matching the
/// spirit of `emerge`'s download-size summary.
pub fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.2} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flag(name: &str, enabled: bool, changed: bool) -> UseFlag {
        UseFlag {
            name: name.to_owned(),
            enabled,
            changed,
            removed: false,
            group: None,
            hidden: false,
            forced: false,
        }
    }

    fn base_entry() -> MergeEntry {
        MergeEntry {
            cp: "dev-libs/openssl".to_owned(),
            version: "3.0.1".to_owned(),
            old_version: None,
            operation: Operation::New,
            acceptance: Acceptance::Stable,
            slot: "0".to_owned(),
            subslot: None,
            repository: Some("gentoo".to_owned()),
            binary: false,
            fetched: false,
            build_id: None,
            use_flags: vec![],
            fetch_size: None,
            parents: vec![],
        }
    }

    #[test]
    fn binary_indicator_and_size() {
        let mut entry = base_entry();
        entry.binary = true;
        entry.fetched = true;
        entry.fetch_size = Some(2510 * 1024);
        let line = render_entry(&entry, true);
        assert!(line.contains("[binary N g]"));
        assert!(line.contains("::gentoo"));
        assert!(line.contains("2.45 MiB"));
    }

    #[test]
    fn ebuild_indicator_for_source() {
        let entry = base_entry();
        assert!(render_entry(&entry, false).contains("[ebuild N]"));
    }

    #[test]
    fn new_versus_upgrade_indicator() {
        let mut new = base_entry();
        new.operation = Operation::New;
        assert!(render_entry(&new, false).contains("[ebuild N]"));

        let mut up = base_entry();
        up.operation = Operation::Upgrade;
        up.old_version = Some("2.9.0".to_owned());
        let line = render_entry(&up, false);
        assert!(line.contains("[ebuild U]"));
        assert!(line.contains("-3.0.1 [2.9.0]"));
    }

    #[test]
    fn testing_keyword_annotated() {
        let mut entry = base_entry();
        entry.acceptance = Acceptance::Testing;
        assert!(render_entry(&entry, false).contains("[ebuild N ~]"));
    }

    #[test]
    fn changed_flags_are_distinguished() {
        let mut entry = base_entry();
        entry.use_flags = vec![flag("ssl", true, true), flag("zlib", false, false)];
        let line = render_entry(&entry, false);
        assert!(line.contains("ssl*"));
        assert!(line.contains("-zlib"));
        assert!(!line.contains("-zlib*"));
    }

    #[test]
    fn forced_flags_are_parenthesized() {
        let mut entry = base_entry();
        let mut forced_on = flag("unicode", true, false);
        forced_on.forced = true;
        let mut masked_off = flag("selinux", false, false);
        masked_off.forced = true;
        entry.use_flags = vec![flag("ssl", true, false), forced_on, masked_off];
        let line = render_entry(&entry, false);
        assert!(
            line.contains("(unicode)"),
            "forced-on flag parenthesized: {line}"
        );
        assert!(
            line.contains("(-selinux)"),
            "masked flag parenthesized: {line}"
        );
        assert!(!line.contains("(ssl)"), "unforced flag is bare: {line}");
    }

    #[test]
    fn hidden_group_is_suppressed() {
        let mut entry = base_entry();
        let mut hidden = flag("amd64", true, false);
        hidden.group = Some("abi_x86".to_owned());
        hidden.hidden = true;
        entry.use_flags = vec![flag("ssl", true, false), hidden];
        let line = render_entry(&entry, false);
        assert!(line.contains("ssl"));
        assert!(!line.contains("amd64"));
        assert!(!line.contains("ABI_X86"));
    }

    #[test]
    fn use_expand_group_is_grouped() {
        let mut entry = base_entry();
        let mut a = flag("x86_64", true, false);
        a.group = Some("cpu_flags_x86".to_owned());
        entry.use_flags = vec![flag("ssl", true, false), a];
        let s = render_use_string(&entry.use_flags);
        assert!(s.contains("USE=\"ssl\""));
        assert!(s.contains("CPU_FLAGS_X86=\"x86_64\""));
    }

    #[test]
    fn apply_use_expand_groups_folds_prefixed_flags() {
        let groups = vec![
            UseExpandGroup {
                prefix: "python_single_target_".to_owned(),
                name: "python_single_target".to_owned(),
                hidden: false,
            },
            UseExpandGroup {
                prefix: "abi_x86_".to_owned(),
                name: "abi_x86".to_owned(),
                hidden: true,
            },
        ];
        let mut flags = vec![
            flag("ssl", true, false),
            flag("python_single_target_python3_14", true, false),
            flag("abi_x86_64", true, false),
        ];
        apply_use_expand_groups(&mut flags, &groups);

        let pst = flags.iter().find(|f| f.name == "python3_14").unwrap();
        assert_eq!(pst.group.as_deref(), Some("python_single_target"));
        assert!(!pst.hidden);

        let abi = flags.iter().find(|f| f.name == "64").unwrap();
        assert_eq!(abi.group.as_deref(), Some("abi_x86"));
        assert!(abi.hidden, "hidden USE_EXPAND group is marked hidden");

        // A plain flag is untouched.
        let ssl = flags.iter().find(|f| f.name == "ssl").unwrap();
        assert!(ssl.group.is_none());
    }

    #[test]
    fn non_default_slot_is_shown() {
        let mut entry = base_entry();
        entry.slot = "1.1".to_owned();
        entry.subslot = Some("1.1".to_owned());
        let line = render_entry(&entry, false);
        assert!(line.contains(":1.1/1.1"));
    }

    #[test]
    fn repository_shown_only_when_verbose() {
        let entry = base_entry();
        assert!(!render_entry(&entry, false).contains("::gentoo"));
        assert!(render_entry(&entry, true).contains("::gentoo"));
    }

    #[test]
    fn totals_count_and_size() {
        let mut entry = base_entry();
        entry.fetch_size = Some(2 * 1024 * 1024);
        let plan = MergePlan {
            entries: vec![entry],
        };
        let totals = render_totals(&plan);
        assert!(totals.contains("Total: 1 package"));
        assert!(totals.contains("2.00 MiB"));
    }

    #[test]
    fn human_size_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.00 KiB");
        assert_eq!(human_size(1536), "1.50 KiB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.00 MiB");
    }

    #[test]
    fn tree_indents_children_under_parents() {
        let mut parent = base_entry();
        parent.cp = "app/top".to_owned();
        let mut child = base_entry();
        child.cp = "lib/dep".to_owned();
        child.parents = vec!["app/top".to_owned()];
        let plan = MergePlan {
            entries: vec![parent, child],
        };
        let tree = render_tree(&plan, false);
        let lines: Vec<&str> = tree.lines().collect();
        let top = lines.iter().position(|l| l.contains("app/top")).unwrap();
        let dep = lines.iter().position(|l| l.contains("lib/dep")).unwrap();
        assert!(dep > top);
        assert!(lines[dep].starts_with("  "));
    }
}
