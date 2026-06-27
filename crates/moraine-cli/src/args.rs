//! Argument parsing for the `moraine` frontend.
//!
//! The surface mirrors `emerge` for the core read-only actions. A small
//! [`prepass`] normalizes the ergonomics that `clap` does not model natively
//! (clustered short flags such as `-puDN`, and `--deep` with an optional
//! non-negative integer) before the derive parser runs. Atom and `@`-set
//! positionals are collected verbatim as raw targets for the expansion layer.
//! [`parse_with_default_opts`] prepends `EMERGE_DEFAULT_OPTS` from `make.conf`
//! before the authoritative parse, unless `--ignore-default-opts` is given.

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
    /// Accepts an optional non-negative depth limit, for example `--deep=2`. The
    /// value must be attached with `=`; a following token is never consumed, so
    /// `--deep cat/pkg` leaves `cat/pkg` as a target. The [`prepass`] attaches a
    /// space-separated integer (`--deep 2` and `-D 2` become `--deep=2`).
    #[arg(short = 'D', long, value_name = "DEPTH", num_args = 0..=1, default_missing_value = "", require_equals = true)]
    pub deep: Option<String>,

    /// Reinstall packages whose effective USE flags changed.
    #[arg(short = 'N', long)]
    pub newuse: bool,

    /// Reinstall installed packages whose enabled USE flags changed, ignoring a
    /// change limited to IUSE (`--changed-use`).
    #[arg(long = "changed-use")]
    pub changed_use: bool,

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

    /// Re-offer a differing protected config even when the same content was
    /// already offered, disabling the config-memory suppression.
    #[arg(long)]
    pub noconfmem: bool,

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

    /// Ignore the options persisted in `EMERGE_DEFAULT_OPTS`, honoring only the
    /// literal command line.
    #[arg(long = "ignore-default-opts")]
    pub ignore_default_opts: bool,

    /// Control whether build-time dependencies are considered during removal.
    /// Only the value `n` is removal-relevant: it excludes DEPEND and BDEPEND
    /// from depclean and prune reachability, matching `emerge --with-bdeps=n`.
    #[arg(long, value_name = "y|n|auto")]
    pub with_bdeps: Option<String>,

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

/// Parse `argv` with `EMERGE_DEFAULT_OPTS` prepended, mirroring `emerge`.
///
/// `argv` is parsed once to discover the roots and `--ignore-default-opts`.
/// Unless ignored, `read_opts` supplies the `EMERGE_DEFAULT_OPTS` string from
/// the effective `make.conf`; it is shell-split and prepended to `argv` before
/// the authoritative parse. The command-line arguments follow the prepended
/// tokens, so a last-wins option typed on the command line overrides the
/// persisted value. Keeping the reader as a closure makes the prepend logic
/// testable without a real configuration.
pub fn parse_with_default_opts(
    argv: &[String],
    read_opts: impl FnOnce(&Cli) -> String,
) -> Result<Cli, clap::Error> {
    let cli = Cli::parse_from_args(argv.iter().cloned())?;
    if cli.ignore_default_opts {
        return Ok(cli);
    }
    let opts = read_opts(&cli);
    let Some(tokens) = shlex::split(&opts).filter(|tokens| !tokens.is_empty()) else {
        return Ok(cli);
    };
    let mut combined = tokens;
    combined.extend(argv.iter().cloned());
    Cli::parse_from_args(combined)
}

/// Normalize `emerge`-style argument ergonomics before the derive parser.
///
/// This splits clustered short flags (`-puDN` becomes `-p -u -D -N`) so each
/// maps to its long option, while leaving a clustered group that ends in a
/// value-taking short flag intact for `clap` to handle. A long `--deep` and a
/// bare short `-D` consume a following token only when it is a non-negative
/// integer, rewriting `--deep 2` and `-D 2` to `--deep=2`; otherwise `--deep`
/// stays valueless and the token remains a positional, matching `emerge`'s
/// `insert_optional_args`. An attached form such as `-D2` becomes `--deep=2`.
/// Other long options and positionals pass through unchanged.
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
    let mut idx = 0;

    while idx < args.len() {
        let arg = &args[idx];
        idx += 1;

        if positional_only {
            out.push(arg.clone());
            continue;
        }
        if arg == "--" {
            positional_only = true;
            out.push(arg.clone());
            continue;
        }

        // A long `--deep` or a bare two-character `-D` consumes a following
        // non-negative integer as its depth value, otherwise it is valueless and
        // the next token stays a positional. This mirrors `emerge` consuming
        // from the argument stack only for `--deep` and the bare `-D`.
        if arg == "--deep" || arg == "-D" {
            match args.get(idx) {
                Some(next) if is_non_negative_integer(next) => {
                    out.push(format!("--deep={next}"));
                    idx += 1;
                }
                _ => out.push("--deep".to_owned()),
            }
            continue;
        }

        // Other long options and non-options pass through untouched.
        if !arg.starts_with('-') || arg.starts_with("--") || arg == "-" {
            out.push(arg.clone());
            continue;
        }

        let chars: Vec<char> = arg.chars().skip(1).collect();
        let mut emitted = Vec::new();
        let mut handled = true;
        let mut cidx = 0;
        while cidx < chars.len() {
            let ch = chars[cidx];
            if ch == 'D' {
                // A clustered `D` only consumes an attached non-negative integer
                // (`-D2`), never a following token. Trailing flag letters stay in
                // the cluster.
                let rest: String = chars[cidx + 1..].iter().collect();
                if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                    emitted.push(format!("--deep={rest}"));
                    break;
                }
                emitted.push("--deep".to_owned());
                cidx += 1;
                continue;
            }
            match SHORT_LONG.iter().find(|(c, _)| *c == ch) {
                Some((_, long)) => emitted.push((*long).to_owned()),
                None => {
                    handled = false;
                    break;
                }
            }
            cidx += 1;
        }

        if handled {
            out.extend(emitted);
        } else {
            out.push(arg.clone());
        }
    }

    out
}

/// Whether `s` is a non-negative integer, matching `emerge`'s `valid_integers`.
fn is_non_negative_integer(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
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
    fn bare_deep_does_not_swallow_following_target() {
        for args in [["-uD", "@world"], ["--deep", "@world"]] {
            let cli = parse(&args);
            assert!(cli.is_deep());
            assert_eq!(cli.deep_depth(), None);
            assert_eq!(cli.targets, vec!["@world".to_owned()]);
        }
    }

    #[test]
    fn clustered_deep_keeps_every_following_target() {
        let cli = parse(&["-uD", "cat/pkg", "@world"]);
        assert!(cli.update && cli.is_deep());
        assert_eq!(cli.deep_depth(), None);
        assert_eq!(cli.targets, vec!["cat/pkg".to_owned(), "@world".to_owned()]);
    }

    #[test]
    fn numeric_deep_value_attaches_and_keeps_target() {
        for args in [["--deep", "2", "cat/pkg"], ["-D", "2", "cat/pkg"]] {
            let cli = parse(&args);
            assert_eq!(cli.deep_depth(), Some(2));
            assert_eq!(cli.targets, vec!["cat/pkg".to_owned()]);
        }
        let attached = parse(&["--deep=2", "cat/pkg"]);
        assert_eq!(attached.deep_depth(), Some(2));
        assert_eq!(attached.targets, vec!["cat/pkg".to_owned()]);
    }

    #[test]
    fn default_opts_are_prepended() {
        let argv = vec!["@world".to_owned()];
        let cli = parse_with_default_opts(&argv, |_| "--deep --newuse".to_owned()).unwrap();
        assert!(cli.is_deep() && cli.newuse);
        assert_eq!(cli.targets, vec!["@world".to_owned()]);
    }

    #[test]
    fn ignore_default_opts_suppresses_reader() {
        let argv = vec!["--ignore-default-opts".to_owned(), "@world".to_owned()];
        let cli = parse_with_default_opts(&argv, |_| "--deep".to_owned()).unwrap();
        assert!(!cli.is_deep());
        assert_eq!(cli.targets, vec!["@world".to_owned()]);
    }

    #[test]
    fn empty_default_opts_leave_argv_unchanged() {
        let argv = vec!["-p".to_owned(), "@world".to_owned()];
        let cli = parse_with_default_opts(&argv, |_| String::new()).unwrap();
        let plain = Cli::parse_from_args(argv.iter().cloned()).unwrap();
        assert_eq!(cli, plain);
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
