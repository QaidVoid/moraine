//! Primary write-action selection.
//!
//! Each invocation runs exactly one primary action. [`select_action`] maps the
//! parsed flags to an [`Action`], rejecting mutually exclusive actions with a
//! diagnostic. Install is the default when no action flag is given.

use crate::args::Cli;

/// The primary action an invocation performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Resolve targets and merge them (the default).
    Install,
    /// Unmerge the named installed packages.
    Unmerge,
    /// Remove packages not needed by the world or system sets.
    Depclean,
    /// Remove installed versions superseded within a slot.
    Prune,
    /// Synchronize the configured repositories.
    Sync,
    /// Resolve pending CONFIG_PROTECT updates.
    ConfigUpdate,
    /// Resume the unfinished portion of the most recent transaction.
    Resume,
}

/// Select the single primary action for the invocation.
///
/// Returns an error message naming the conflict when more than one primary
/// action flag is set.
pub fn select_action(cli: &Cli) -> Result<Action, String> {
    let mut selected: Vec<(&str, Action)> = Vec::new();
    if cli.unmerge {
        selected.push(("--unmerge", Action::Unmerge));
    }
    if cli.depclean {
        selected.push(("--depclean", Action::Depclean));
    }
    if cli.prune {
        selected.push(("--prune", Action::Prune));
    }
    if cli.sync {
        selected.push(("--sync", Action::Sync));
    }
    if cli.config_update {
        selected.push(("--config-update", Action::ConfigUpdate));
    }
    if cli.resume {
        selected.push(("--resume", Action::Resume));
    }

    match selected.as_slice() {
        [] => Ok(Action::Install),
        [(_, action)] => Ok(*action),
        many => {
            let names: Vec<&str> = many.iter().map(|(name, _)| *name).collect();
            Err(format!(
                "conflicting actions: {} cannot be combined",
                names.join(", ")
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::parse_from_args(args.iter().map(|s| s.to_string())).unwrap()
    }

    #[test]
    fn default_is_install() {
        assert_eq!(
            select_action(&parse(&["cat/pkg"])).unwrap(),
            Action::Install
        );
    }

    #[test]
    fn single_action_selected() {
        assert_eq!(
            select_action(&parse(&["-C", "cat/pkg"])).unwrap(),
            Action::Unmerge
        );
        assert_eq!(select_action(&parse(&["--sync"])).unwrap(), Action::Sync);
        assert_eq!(
            select_action(&parse(&["--depclean"])).unwrap(),
            Action::Depclean
        );
    }

    #[test]
    fn conflicting_actions_rejected() {
        let err = select_action(&parse(&["-C", "--depclean", "cat/pkg"])).unwrap_err();
        assert!(err.contains("--unmerge"));
        assert!(err.contains("--depclean"));
    }
}
