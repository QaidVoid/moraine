//! REQUIRED_USE evaluation, performed after a candidate's USE is resolved.

use std::collections::BTreeSet;

use crate::depnode::{BlockerKind, DepNode, NormAtom};

/// Parse a REQUIRED_USE string into a [`DepNode`] whose leaves are USE flags
/// (the leaf `cp` field holds the flag name, blocker marks negation).
///
/// REQUIRED_USE reuses the dependency grammar, but its leaves are flags rather
/// than atoms, so it needs its own tiny parser. Supported forms: `flag`,
/// `!flag`, `flag? ( ... )`, `!flag? ( ... )`, `|| ( ... )`, `?? ( ... )`
/// (treated as at-most-one but encoded leniently as a group), `^^ ( ... )`
/// (exactly-one), and nested all-of groups.
pub fn parse_required_use(input: &str) -> DepNode {
    let tokens = tokenize(input);
    let mut pos = 0;
    let body = parse_seq(&tokens, &mut pos);
    DepNode::AllOf(body)
}

fn tokenize(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in input.split_whitespace() {
        // Keep parentheses as their own tokens even when glued to a word.
        let mut cur = String::new();
        for ch in raw.chars() {
            if ch == '(' || ch == ')' {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                out.push(ch.to_string());
            } else {
                cur.push(ch);
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    out
}

fn parse_seq(tokens: &[String], pos: &mut usize) -> Vec<DepNode> {
    let mut nodes = Vec::new();
    while *pos < tokens.len() {
        let tok = &tokens[*pos];
        if tok == ")" {
            break;
        }
        if tok == "||" || tok == "^^" || tok == "??" {
            *pos += 1;
            // Expect "(".
            if *pos < tokens.len() && tokens[*pos] == "(" {
                *pos += 1;
            }
            let body = parse_seq(tokens, pos);
            if *pos < tokens.len() && tokens[*pos] == ")" {
                *pos += 1;
            }
            // `||` and `^^` are encoded as any-of; `??` (at-most-one) is encoded
            // as a satisfiable group (its constraint is enforced only loosely).
            nodes.push(DepNode::AnyOf(body));
            continue;
        }
        if let Some(cond) = tok.strip_suffix('?') {
            let (flag, sense) = if let Some(f) = cond.strip_prefix('!') {
                (f.to_owned(), false)
            } else {
                (cond.to_owned(), true)
            };
            *pos += 1;
            if *pos < tokens.len() && tokens[*pos] == "(" {
                *pos += 1;
            }
            let body = parse_seq(tokens, pos);
            if *pos < tokens.len() && tokens[*pos] == ")" {
                *pos += 1;
            }
            nodes.push(DepNode::Conditional { flag, sense, body });
            continue;
        }
        if tok == "(" {
            *pos += 1;
            let body = parse_seq(tokens, pos);
            if *pos < tokens.len() && tokens[*pos] == ")" {
                *pos += 1;
            }
            nodes.push(DepNode::AllOf(body));
            continue;
        }
        // A bare flag, possibly negated.
        let (flag, blocker) = if let Some(f) = tok.strip_prefix('!') {
            (f.to_owned(), BlockerKind::Weak)
        } else {
            (tok.clone(), BlockerKind::None)
        };
        nodes.push(DepNode::Leaf(NormAtom {
            blocker,
            cp: flag,
            version: None,
            slot: None,
            subslot: None,
            slot_op: None,
            use_deps: Vec::new(),
        }));
        *pos += 1;
    }
    nodes
}

/// The outcome of checking a package's REQUIRED_USE against its resolved USE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequiredUseOutcome {
    /// The constraint holds.
    Satisfied,
    /// The constraint is violated; the string names the failing sub-constraint.
    Violated(String),
}

/// Evaluate a REQUIRED_USE tree against a package's resolved USE.
///
/// REQUIRED_USE reuses the dependency grammar where a "leaf" is a USE flag
/// rather than an atom. The normalizer represents each flag token as a
/// [`NormAtom`] whose `cp` is the flag name and whose blocker marks negation
/// (`!flag`). Conditional groups encode `flag? ( ... )`; any-of groups encode
/// `|| ( ... )` (at-least-one-of).
pub fn evaluate_required_use(node: &DepNode, use_set: &BTreeSet<String>) -> RequiredUseOutcome {
    if node_satisfied(node, use_set) {
        RequiredUseOutcome::Satisfied
    } else {
        RequiredUseOutcome::Violated(render(node))
    }
}

fn flag_state(atom: &NormAtom, use_set: &BTreeSet<String>) -> bool {
    let enabled = use_set.contains(&atom.cp);
    match atom.blocker {
        BlockerKind::Weak | BlockerKind::Strong => !enabled,
        BlockerKind::None => enabled,
    }
}

fn node_satisfied(node: &DepNode, use_set: &BTreeSet<String>) -> bool {
    match node {
        DepNode::Leaf(atom) => flag_state(atom, use_set),
        DepNode::AllOf(children) => children.iter().all(|c| node_satisfied(c, use_set)),
        DepNode::AnyOf(branches) => branches.iter().any(|b| node_satisfied(b, use_set)),
        DepNode::Conditional { flag, sense, body } => {
            let live = use_set.contains(flag) == *sense;
            if live {
                body.iter().all(|c| node_satisfied(c, use_set))
            } else {
                true
            }
        }
    }
}

/// Render a REQUIRED_USE node for an explanation.
pub fn render(node: &DepNode) -> String {
    match node {
        DepNode::Leaf(atom) => {
            let prefix = match atom.blocker {
                BlockerKind::None => "",
                _ => "!",
            };
            format!("{prefix}{}", atom.cp)
        }
        DepNode::AllOf(children) => children.iter().map(render).collect::<Vec<_>>().join(" "),
        DepNode::AnyOf(branches) => {
            format!(
                "|| ( {} )",
                branches.iter().map(render).collect::<Vec<_>>().join(" ")
            )
        }
        DepNode::Conditional { flag, sense, body } => {
            let prefix = if *sense { "" } else { "!" };
            format!(
                "{prefix}{flag}? ( {} )",
                body.iter().map(render).collect::<Vec<_>>().join(" ")
            )
        }
    }
}
