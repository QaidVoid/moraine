//! Argument parsing for the `moraine` frontend.
//!
//! The surface mirrors `emerge` for the core read-only actions. A small
//! [`prepass`] normalizes the ergonomics that `clap` does not model natively
//! (clustered short flags such as `-puDN`, and `--deep` with an optional
//! non-negative integer) before the derive parser runs. Atom and `@`-set
//! positionals are collected verbatim as raw targets for the expansion layer.

use std::path::PathBuf;

use clap::Parser;

/// The parsed `moraine` command line.
///
/// Built by [`Cli::parse_from_args`], which runs the [`prepass`] before handing
/// the normalized arguments to the derive parser.
#[derive(Debug, Clone, Parser, PartialEq, Eq)]
#[command(
    name = "moraine",
    bin_name = "moraine",
    about = "Moraine: a fast, read-only emerge-compatible resolver frontend",
    long_about = None,
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Resolve and display the merge list without making any changes.
    #[arg(short = 'p', long)]
    pub pretend: bool,

    /// Update packages to the best visible version.
    #[arg(short = 'u', long)]
    pub update: bool,

    /// Consider the entire dependency tree, not just direct dependencies.
    ///
    /// Accepts an optional non-negative depth limit, for example `--deep=2`.
    #[arg(short = 'D', long, value_name = "DEPTH", num_args = 0..=1, default_missing_value = "")]
    pub deep: Option<String>,

    /// Reinstall packages whose effective USE flags changed.
    #[arg(short = 'N', long)]
    pub newuse: bool,

    /// Reinstall installed packages whose dependencies changed in the ebuild,
    /// ignoring slot-operator bindings (`--changed-deps`).
    #[arg(long = "changed-deps")]
    pub changed_deps: bool,

    /// Reinstall installed packages whose ebuild slot or sub-slot changed
    /// (`--changed-slot`).
    #[arg(long = "changed-slot")]
    pub changed_slot: bool,

    /// Prompt before proceeding with any mutation.
    #[arg(short = 'a', long)]
    pub ask: bool,

    /// Unmerge the named installed packages.
    #[arg(short = 'C', long)]
    pub unmerge: bool,

    /// Remove installed packages not needed by the world or system sets.
    #[arg(short = 'c', long)]
    pub depclean: bool,

    /// Remove installed versions superseded by a higher version in the slot.
    #[arg(long)]
    pub prune: bool,

    /// Synchronize the configured repositories.
    #[arg(long)]
    pub sync: bool,

    /// Resolve and apply pending CONFIG_PROTECT updates.
    #[arg(long = "config-update")]
    pub config_update: bool,

    /// Resume the unfinished portion of the most recent transaction.
    #[arg(long)]
    pub resume: bool,

    /// Build a binary package alongside merging each package.
    #[arg(short = 'b', long)]
    pub buildpkg: bool,

    /// Build a binary package for each package without merging it.
    #[arg(short = 'B', long)]
    pub buildpkgonly: bool,

    /// Prefer an available binary package over building from source.
    #[arg(short = 'k', long)]
    pub usepkg: bool,

    /// Use only binary packages: a package with no compatible binary is
    /// unsatisfiable rather than built from source.
    #[arg(short = 'K', long)]
    pub usepkgonly: bool,

    /// Fetch binary packages from the configured binhost.
    #[arg(short = 'g', long)]
    pub getbinpkg: bool,

    /// Increase output detail. Repeat for more verbosity.
    #[arg(short = 'v', long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Do not add the target packages to the world set. This is a display note
    /// in the read-only phase.
    #[arg(short = '1', long)]
    pub oneshot: bool,

    /// Emit a per-phase resolution timing breakdown.
    #[arg(long)]
    pub timing: bool,

    /// Render the merge list as an indented dependency tree.
    #[arg(short = 't', long)]
    pub tree: bool,

    /// Exclude an atom from the request, leaving any installed version in
    /// place. May be given more than once.
    #[arg(long, value_name = "ATOM")]
    pub exclude: Vec<String>,

    /// The installed-tree root the resolution reads from.
    #[arg(long, value_name = "DIR")]
    pub root: Option<PathBuf>,

    /// The configuration root that holds `etc/portage`.
    #[arg(long, value_name = "DIR")]
    pub config_root: Option<PathBuf>,

    /// Override the active profile directory.
    #[arg(long, value_name = "DIR")]
    pub profile: Option<PathBuf>,

    /// Trigger the bootstrap demonstration error and exit non-zero.
    #[arg(long, hide = true)]
    pub demo_error: bool,

    /// Target atoms and `@`-prefixed package sets to resolve.
    #[arg(value_name = "TARGET")]
    pub targets: Vec<String>,
}

impl Cli {
    /// Parse from an argument list that excludes the program name.
    ///
    /// Returns the clap error so the caller can print help or usage and choose
    /// the exit code.
    pub fn parse_from_args<I, S>(args: I) -> Result<Cli, clap::Error>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let raw: Vec<String> = args.into_iter().map(Into::into).collect();
        let normalized = prepass(&raw);
        let mut with_bin = Vec::with_capacity(normalized.len() + 1);
        with_bin.push("moraine".to_owned());
        with_bin.extend(normalized);
        Cli::try_parse_from(with_bin)
    }

    /// Whether the active verbosity should surface extra detail such as the
    /// repository column and USE-flag descriptions.
    pub fn is_verbose(&self) -> bool {
        self.verbose > 0
    }

    /// Whether the tree view should be rendered.
    pub fn show_tree(&self) -> bool {
        self.tree || self.verbose >= 2
    }

    /// The `--deep` depth limit, if a value was supplied.
    ///
    /// Returns `None` when `--deep` was absent or given with no value.
    pub fn deep_depth(&self) -> Option<u32> {
        self.deep
            .as_deref()
            .filter(|value| !value.is_empty())
            .and_then(|value| value.parse().ok())
    }

    /// Whether the deep modifier is active in any form.
    pub fn is_deep(&self) -> bool {
        self.deep.is_some()
    }
}

/// Normalize `emerge`-style argument ergonomics before the derive parser.
///
/// This splits clustered short flags (`-puDN` becomes `-p -u -D -N`) so each
/// maps to its long option, while leaving a clustered group that ends in a
/// value-taking short flag intact for `clap` to handle. A bare `-D` keeps its
/// optional-integer behavior, and an attached form such as `-D2` becomes
/// `--deep=2`. Long options and positionals pass through unchanged.
pub fn prepass(args: &[String]) -> Vec<String> {
    const SHORT_LONG: &[(char, &str)] = &[
        ('p', "--pretend"),
        ('u', "--update"),
        ('N', "--newuse"),
        ('a', "--ask"),
        ('v', "--verbose"),
        ('1', "--oneshot"),
        ('t', "--tree"),
        ('C', "--unmerge"),
        ('c', "--depclean"),
        ('b', "--buildpkg"),
        ('B', "--buildpkgonly"),
        ('k', "--usepkg"),
        ('K', "--usepkgonly"),
        ('g', "--getbinpkg"),
    ];

    let mut out = Vec::with_capacity(args.len());
    let mut positional_only = false;

    for arg in args {
        if positional_only {
            out.push(arg.clone());
            continue;
        }
        if arg == "--" {
            positional_only = true;
            out.push(arg.clone());
            continue;
        }
        // Long options and non-options pass through untouched.
        if !arg.starts_with('-') || arg.starts_with("--") || arg == "-" {
            out.push(arg.clone());
            continue;
        }

        let chars: Vec<char> = arg.chars().skip(1).collect();
        let mut emitted = Vec::new();
        let mut handled = true;
        let mut idx = 0;
        while idx < chars.len() {
            let ch = chars[idx];
            if ch == 'D' {
                // `-D` only consumes an attached non-negative integer, matching
                // `emerge`. Trailing flag letters stay in the cluster.
                let rest: String = chars[idx + 1..].iter().collect();
                if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                    emitted.push(format!("--deep={rest}"));
                    break;
                }
                emitted.push("--deep".to_owned());
                idx += 1;
                continue;
            }
            match SHORT_LONG.iter().find(|(c, _)| *c == ch) {
                Some((_, long)) => emitted.push((*long).to_owned()),
                None => {
                    handled = false;
                    break;
                }
            }
            idx += 1;
        }

        if handled {
            out.extend(emitted);
        } else {
            out.push(arg.clone());
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::parse_from_args(args.iter().map(|s| s.to_string())).expect("parses")
    }

    #[test]
    fn clustered_shorts_equal_long_forms() {
        let short = parse(&["-puDN", "@world"]);
        let long = parse(&["--pretend", "--update", "--deep", "--newuse", "@world"]);
        assert_eq!(short, long);
        assert!(short.pretend && short.update && short.is_deep() && short.newuse);
        assert_eq!(short.targets, vec!["@world".to_owned()]);
    }

    #[test]
    fn deep_takes_optional_integer() {
        let attached = parse(&["-D2", "cat/pkg"]);
        assert_eq!(attached.deep_depth(), Some(2));
        let long = parse(&["--deep=3", "cat/pkg"]);
        assert_eq!(long.deep_depth(), Some(3));
        let bare = parse(&["-D", "cat/pkg"]);
        assert!(bare.is_deep());
        assert_eq!(bare.deep_depth(), None);
    }

    #[test]
    fn exclude_is_repeatable() {
        let cli = parse(&["-p", "--exclude", "cat/a", "--exclude", "cat/b", "@world"]);
        assert_eq!(cli.exclude, vec!["cat/a".to_owned(), "cat/b".to_owned()]);
    }

    #[test]
    fn global_config_options_parse() {
        let cli = parse(&[
            "-p",
            "--root",
            "/mnt/root",
            "--config-root",
            "/mnt/cfg",
            "--profile",
            "/mnt/prof",
            "@system",
        ]);
        assert_eq!(cli.root, Some(PathBuf::from("/mnt/root")));
        assert_eq!(cli.config_root, Some(PathBuf::from("/mnt/cfg")));
        assert_eq!(cli.profile, Some(PathBuf::from("/mnt/prof")));
    }

    #[test]
    fn mixed_atoms_and_sets_collected() {
        let cli = parse(&["-p", "cat/a", "@world", ">=cat/b-2"]);
        assert_eq!(
            cli.targets,
            vec![
                "cat/a".to_owned(),
                "@world".to_owned(),
                ">=cat/b-2".to_owned()
            ]
        );
    }

    #[test]
    fn invalid_option_is_rejected() {
        let err = Cli::parse_from_args(["--no-such-flag".to_string()]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn verbose_counts_repeats() {
        let cli = parse(&["-vv", "@world"]);
        assert_eq!(cli.verbose, 2);
        assert!(cli.show_tree());
    }
}
