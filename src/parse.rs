//! Turn one shell command string into the list of simple commands it will run.
//!
//! This is the part a pattern-matching rule cannot do. `a && b`, `a | b`,
//! `a; b`, `$(a)`, `` `a` ``, `(a)`, `if a; then b; fi` and `bash -c "a"` all
//! contain commands that a rule written about `a` should see.

use tree_sitter::{Node, Parser};

#[derive(Debug, PartialEq, Eq)]
pub struct Simple {
    /// argv with quotes resolved and leading VAR=x assignments dropped.
    pub argv: Vec<String>,
}

#[derive(Debug)]
pub struct Parsed {
    pub commands: Vec<Simple>,
    /// Every redirect target in the command, such as the `out.txt` in
    /// `echo x > out.txt`. These are not arguments to any program, so they
    /// never appear in an argv, but they are still files being written.
    pub redirects: Vec<String>,
    /// True when the grammar could not read the input. The caller escalates
    /// rather than guessing, since an unreadable command is the one most
    /// likely to be hiding something.
    pub had_error: bool,
}

pub fn parse(source: &str) -> Parsed {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .is_err()
    {
        return Parsed { commands: vec![], redirects: vec![], had_error: true };
    }
    let Some(tree) = parser.parse(source, None) else {
        return Parsed { commands: vec![], redirects: vec![], had_error: true };
    };

    let mut out = Vec::new();
    let mut redirects = Vec::new();
    collect(tree.root_node(), source.as_bytes(), &mut out, &mut redirects, 0);
    Parsed { commands: out, redirects, had_error: tree.root_node().has_error() }
}

fn collect(
    node: Node,
    src: &[u8],
    out: &mut Vec<Simple>,
    redirects: &mut Vec<String>,
    depth: usize,
) {
    if depth > 16 {
        return; // runaway nesting; the caller still sees what we found
    }
    if node.kind() == "command" {
        if let Some(cmd) = simple_from(node, src) {
            // `bash -c "..."` hides a whole program in an argument. Parse it
            // too, so a rule about the inner command still fires.
            if let Some(inner) = shell_c_payload(&cmd.argv) {
                let sub = parse(&inner);
                out.extend(sub.commands);
                redirects.extend(sub.redirects);
            }
            out.push(cmd);
        }
    }
    if matches!(node.kind(), "file_redirect" | "heredoc_redirect") {
        let mut c = node.walk();
        for child in node.named_children(&mut c) {
            if let Some(w) = word_of(child, src) {
                redirects.push(w);
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, src, out, redirects, depth + 1);
    }
}

fn simple_from(node: Node, src: &[u8]) -> Option<Simple> {
    let mut argv = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            // Leading `FOO=bar cmd` assignments are not part of argv, and
            // redirects are not arguments to the command.
            "variable_assignment" | "file_redirect" | "heredoc_redirect" => continue,
            _ => {}
        }
        if let Some(w) = word_of(child, src) {
            argv.push(w);
        }
    }
    if argv.is_empty() {
        None
    } else {
        Some(Simple { argv })
    }
}

/// Resolve a node to the literal word it contributes.
///
/// Quoted words lose their quotes, so `git 'push'` reads as `git push`, a
/// spelling the docs list as one a permission rule misses. A word containing
/// an expansion (`$X`, `$(...)`) keeps its text: we do not know its value, and
/// pretending otherwise would be worse than matching it literally.
fn word_of(node: Node, src: &[u8]) -> Option<String> {
    let text = node.utf8_text(src).ok()?;
    let out = match node.kind() {
        "string" | "raw_string" => text
            .trim_start_matches(['"', '\''])
            .trim_end_matches(['"', '\''])
            .to_string(),
        "command_name" => {
            // command_name wraps a word/string/concatenation
            let mut c = node.walk();
            let inner = node
                .named_children(&mut c)
                .next()
                .and_then(|inner| word_of(inner, src))
                .unwrap_or_else(|| text.to_string());
            inner
        }
        _ => text.to_string(),
    };
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// If argv is a shell invoked with -c, return the program it was handed.
fn shell_c_payload(argv: &[String]) -> Option<String> {
    let prog = std::path::Path::new(argv.first()?)
        .file_name()?
        .to_str()?;
    if !matches!(prog, "bash" | "sh" | "zsh" | "dash" | "ksh") {
        return None;
    }
    let mut it = argv.iter().skip(1);
    while let Some(a) = it.next() {
        if a == "-c" {
            return it.next().cloned();
        }
        // combined short flags such as -lc
        if a.starts_with('-') && !a.starts_with("--") && a.contains('c') {
            return it.next().cloned();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argvs(s: &str) -> Vec<Vec<String>> {
        parse(s).commands.into_iter().map(|c| c.argv).collect()
    }

    #[test]
    fn plain_command() {
        assert_eq!(argvs("git push origin main"), vec![vec!["git", "push", "origin", "main"]]);
    }

    #[test]
    fn splits_on_operators() {
        for sep in ["&&", "||", ";", "|"] {
            let got = argvs(&format!("ls {sep} git push"));
            assert!(got.iter().any(|a| a[0] == "git" && a[1] == "push"), "{sep} lost the push");
        }
    }

    #[test]
    fn finds_commands_in_substitutions_and_subshells() {
        assert!(argvs("echo $(git push)").iter().any(|a| a[0] == "git"));
        assert!(argvs("echo `git push`").iter().any(|a| a[0] == "git"));
        assert!(argvs("(cd /tmp && git push)").iter().any(|a| a[0] == "git"));
    }

    #[test]
    fn unwraps_shell_dash_c() {
        let got = argvs(r#"bash -c "git push origin main""#);
        assert!(got.iter().any(|a| a[0] == "git" && a[1] == "push"), "{got:?}");
        let got = argvs(r#"sh -lc 'git push'"#);
        assert!(got.iter().any(|a| a[0] == "git"), "{got:?}");
    }

    #[test]
    fn strips_quotes_so_git_quote_push_is_seen() {
        let got = argvs("git 'push' origin");
        assert_eq!(got[0][1], "push");
    }

    #[test]
    fn drops_leading_assignments_but_keeps_the_command() {
        let got = argvs("FOO=bar git push");
        assert_eq!(got[0][0], "git");
    }

    #[test]
    fn finds_commands_inside_control_flow() {
        assert!(argvs("if true; then git push; fi").iter().any(|a| a[0] == "git"));
        assert!(argvs("for f in a b; do rm $f; done").iter().any(|a| a[0] == "rm"));
    }

    #[test]
    fn reports_a_parse_error() {
        assert!(parse("git push 'unterminated").had_error);
    }
}
