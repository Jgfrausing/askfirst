//! Refuse to let a tool call quietly change askfirst itself.
//!
//! The Edit and Write tools can be fenced off with `permissions.deny`, but a
//! Bash command reaches the same files by other routes: `sed -i`, a `>`
//! redirect, `mv`, `rm`, `tee`, `cp`, an editor. A gate whose own rules the
//! agent can rewrite is decoration, so any command that so much as names one
//! of askfirst's files is floored at `ask`.
//!
//! Naming a file is not the same as writing to it, and telling the two apart
//! would mean knowing the semantics of every program. Reading the config is
//! harmless and asking about it is cheap, so this errs toward asking.

use std::path::{Component, Path, PathBuf};

/// Everything that decides what askfirst does, plus the binary that does it.
///
/// Rules, verdicts, the review queue and the current mode all live in one
/// directory, so guarding it is a single entry rather than a list to keep in
/// step with whatever files get added later.
pub fn protected_paths() -> Vec<PathBuf> {
    let mut v = vec![crate::config::dir()];

    // Also the location ASKFIRST_HOME cannot move. Resolving the guard from
    // the same variable it guards meant that setting the variable in the
    // hook's environment pointed the fence at a decoy and left the real files
    // open.
    let default = crate::config::default_dir();
    if !v.contains(&default) {
        v.push(default);
    }

    // Claude Code's settings are where the hook is registered and where an
    // `env` block could set ASKFIRST_HOME for every later hook invocation.
    // Changing them changes askfirst, so they belong here.
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    for f in ["settings.json", "settings.local.json"] {
        v.push(home.join(".claude").join(f));
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Ok(real) = std::fs::canonicalize(&exe) {
            v.push(real);
        }
        v.push(exe);
    }
    v
}

/// The file tools name a path directly rather than hiding it in a command, so
/// they need no parsing: resolve it and check it like any other word.
pub fn file_tool_touches_self(path: &str, cwd: &str) -> Option<String> {
    touches_self(&[path.to_string()], cwd, &protected_paths())
}

/// The names askfirst answers to, for spotting a call to itself that carries
/// no path: a bare `askfirst review` has no `/` in it, so the path check below
/// never sees it.
fn own_names() -> Vec<String> {
    let mut v = vec!["askfirst".to_string()];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(n) = exe.file_name().and_then(|o| o.to_str()) {
            if !v.iter().any(|x| x == n) {
                v.push(n.to_string());
            }
        }
    }
    v
}

/// The one subcommand an agent may run unprompted.
///
/// `check` reports what would happen to a command and changes nothing: no
/// verdict, no mode, no queue entry. Letting the agent run it is useful,
/// because it can find out where a boundary is instead of walking into it.
/// Every other subcommand either writes state or is a bare invocation that
/// would append to the queue, so all of them ask.
const READ_ONLY_SUBCOMMANDS: &[&str] = &["check"];

/// Is this command an invocation of askfirst that should ask?
///
/// Pass the normalised argv so a wrapper cannot hide the call: `sudo askfirst
/// review` must be caught as readily as `askfirst review`.
pub fn invokes_self(argv: &[String]) -> Option<String> {
    let first = argv.first()?;
    let name = Path::new(first)
        .file_name()
        .and_then(|o| o.to_str())
        .unwrap_or(first);
    if !own_names().iter().any(|n| n == name) {
        return None;
    }
    // `askfirst check "..."` is the exception. Anything else, including a
    // bare `askfirst` reading a crafted event on stdin, asks.
    if let Some(sub) = argv.get(1) {
        if READ_ONLY_SUBCOMMANDS.contains(&sub.as_str()) {
            return None;
        }
    }
    Some(first.clone())
}

/// Does this rule pattern name askfirst as the program it decides?
///
/// A rule has to name askfirst to say what running askfirst does. A wildcard
/// that happens to sweep it up, `*` or `* mode *`, does not: those are written
/// about other commands, and letting one of them allow `askfirst mode` would
/// widen the gate by accident.
pub fn pattern_names_self(pattern: &str) -> bool {
    let Some(first) = pattern.split_whitespace().next() else { return false };
    let name = Path::new(first)
        .file_name()
        .and_then(|o| o.to_str())
        .unwrap_or(first);
    own_names().iter().any(|n| n.eq_ignore_ascii_case(name))
}

/// Does this command name any protected path?
///
/// `cwd` is the session's working directory, used to resolve a relative path
/// such as `rules.toml`.
pub fn touches_self(words: &[String], cwd: &str, protected: &[PathBuf]) -> Option<String> {
    for w in words {
        let Some(candidate) = resolve(w, cwd) else { continue };
        for p in protected {
            // The path itself, or anything inside it.
            if candidate == *p || candidate.starts_with(p) {
                return Some(w.clone());
            }
            // Its immediate parent, so `rm -rf ~/.config` is caught. Any
            // ancestor would be far too wide: `.` resolves to the working
            // directory, and `~` to the home directory, both of which are
            // ancestors of everything askfirst owns. Matching those flagged
            // every command with a `.` argument.
            if p.parent() == Some(candidate.as_path()) {
                return Some(w.clone());
            }
        }
    }
    None
}

/// Turn a word into the path it would refer to, expanding `~` and `$HOME` and
/// resolving a relative path against `cwd`. Lexical only: it never touches the
/// filesystem, so it works for a file that does not exist yet.
fn resolve(word: &str, cwd: &str) -> Option<PathBuf> {
    if word.is_empty() || word.starts_with('-') {
        return None;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let mut s = word.to_string();
    if s == "~" {
        s = home.clone();
    } else if let Some(rest) = s.strip_prefix("~/") {
        s = format!("{home}/{rest}");
    }
    for var in ["$HOME", "${HOME}"] {
        if s.contains(var) {
            s = s.replace(var, &home);
        }
    }
    // A word with no path separator is a program name or a plain argument,
    // unless it is a bare file name we should resolve against cwd. Only treat
    // it as a path when it looks like one.
    let looks_like_path = s.contains('/') || s.contains('.');
    if !looks_like_path {
        return None;
    }
    let p = Path::new(&s);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new(cwd).join(p)
    };
    Some(lexical_normalise(&joined))
}

/// Resolve `.` and `..` without consulting the filesystem.
fn lexical_normalise(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    fn protected() -> Vec<PathBuf> {
        vec![
            PathBuf::from("/home/u/.config/askfirst"),
            PathBuf::from("/home/u/.config/askfirst/rules.toml"),
            PathBuf::from("/home/u/.local/state/askfirst"),
            PathBuf::from("/home/u/.local/bin/askfirst"),
        ]
    }

    fn hit(cmd: &str, cwd: &str) -> bool {
        touches_self(&words(cmd), cwd, &protected()).is_some()
    }

    #[test]
    fn catches_an_absolute_path() {
        assert!(hit("sed -i s/a/b/ /home/u/.config/askfirst/rules.toml", "/tmp"));
    }

    #[test]
    fn catches_a_tilde_path() {
        std::env::set_var("HOME", "/home/u");
        assert!(hit("vim ~/.config/askfirst/rules.toml", "/tmp"));
        assert!(hit("rm -rf ~/.local/state/askfirst", "/tmp"));
    }

    #[test]
    fn catches_a_home_variable() {
        std::env::set_var("HOME", "/home/u");
        assert!(hit("cp /dev/null $HOME/.config/askfirst/rules.toml", "/tmp"));
        assert!(hit("cp /dev/null ${HOME}/.config/askfirst/rules.toml", "/tmp"));
    }

    #[test]
    fn catches_a_relative_path_from_the_config_directory() {
        assert!(hit("sed -i s/a/b/ rules.toml", "/home/u/.config/askfirst"));
        assert!(hit("cat ./rules.toml", "/home/u/.config/askfirst"));
    }

    #[test]
    fn catches_a_dot_dot_route() {
        assert!(hit("cat /home/u/.config/other/../askfirst/rules.toml", "/tmp"));
    }

    #[test]
    fn catches_replacing_the_binary() {
        assert!(hit("cp /tmp/evil /home/u/.local/bin/askfirst", "/tmp"));
    }

    #[test]
    fn catches_naming_the_directory() {
        assert!(hit("ls /home/u/.config/askfirst", "/tmp"));
    }

    #[test]
    fn catches_a_call_to_itself_with_no_path() {
        // The route the path check cannot see: `askfirst review` writes
        // verdicts, so an agent could approve its own queue with it.
        assert!(invokes_self(&words("askfirst review")).is_some());
        assert!(invokes_self(&words("askfirst mode contributor")).is_some());
        assert!(invokes_self(&words("askfirst pending")).is_some());
        assert!(invokes_self(&words("/Users/x/.local/bin/askfirst review")).is_some());
        assert!(invokes_self(&words("./askfirst review")).is_some());
    }

    #[test]
    fn check_is_the_one_subcommand_that_does_not_ask() {
        assert!(invokes_self(&words("askfirst check ls")).is_none());
        // Everything else, including a bare call that would append to the queue.
        assert!(invokes_self(&words("askfirst")).is_some());
        assert!(invokes_self(&words("askfirst modes")).is_some());
        assert!(invokes_self(&words("askfirst pending")).is_some());
        assert!(invokes_self(&words("askfirst paths")).is_some());
    }

    #[test]
    fn only_a_pattern_naming_askfirst_decides_askfirst() {
        assert!(pattern_names_self("askfirst *"));
        assert!(pattern_names_self("askfirst mode *"));
        assert!(pattern_names_self("/usr/local/bin/askfirst *"));
        // A wildcard written about other commands sweeps askfirst up without
        // naming it, and must not decide what running the gate does.
        assert!(!pattern_names_self("*"));
        assert!(!pattern_names_self("* mode *"));
        assert!(!pattern_names_self("git *"));
        assert!(!pattern_names_self(""));
    }

    #[test]
    fn does_not_mistake_other_programs_for_itself() {
        assert!(invokes_self(&words("cargo run -- review")).is_none());
        assert!(invokes_self(&words("echo askfirst")).is_none());
        assert!(invokes_self(&words("git commit -m askfirst")).is_none());
    }

    #[test]
    fn an_ancestor_directory_does_not_drag_everything_in() {
        // Regression: `.` resolves to the working directory and `~` to home,
        // both ancestors of the protected directory. Treating any ancestor as
        // a hit flagged every command with a `.` argument.
        std::env::set_var("HOME", "/home/u");
        assert!(!hit("jq . data.json", "/home/u"));
        assert!(!hit("ls .", "/home/u"));
        assert!(!hit("du -sh ~", "/tmp"));
        assert!(!hit("ls -la", "/home/u/.config"));
        assert!(!hit("find / -name x", "/tmp"));
        // The immediate parent is still caught: this would take the whole
        // directory with it.
        assert!(hit("rm -rf /home/u/.config", "/tmp"));
        // Naming that parent as `.` from inside it counts too. Reading is not
        // writing, but telling them apart needs the semantics of every
        // program, and working from ~/.config is rare enough to just ask.
        assert!(hit("rg pattern .", "/home/u/.config"));
    }

    #[test]
    fn the_file_tools_are_checked_on_their_path_alone() {
        std::env::set_var("HOME", "/home/u");
        // No shell involved, so the command parsing never sees these.
        assert!(file_tool_touches_self("/home/u/.config/askfirst/rules.json", "/tmp").is_some());
        assert!(file_tool_touches_self("rules.json", "/home/u/.config/askfirst").is_some());
        assert!(file_tool_touches_self("~/.claude/settings.local.json", "/tmp").is_some());
        assert!(file_tool_touches_self("/home/u/code/app/main.rs", "/tmp").is_none());
    }

    #[test]
    fn askfirst_home_cannot_move_the_fence_off_the_real_files() {
        // The exploit: point ASKFIRST_HOME at a decoy and the guard used to
        // protect the decoy instead of the real directory.
        std::env::set_var("HOME", "/home/u");
        std::env::set_var("ASKFIRST_HOME", "/tmp/decoy");
        let p = protected_paths();
        std::env::remove_var("ASKFIRST_HOME");
        assert!(p.contains(&PathBuf::from("/tmp/decoy")), "redirect target should be guarded");
        assert!(
            p.contains(&PathBuf::from("/home/u/.config/askfirst")),
            "the real directory must stay guarded: {p:?}"
        );
    }

    #[test]
    fn claude_code_settings_are_protected() {
        // Where the hook is registered, and where an `env` block could set
        // ASKFIRST_HOME for every later hook invocation.
        std::env::set_var("HOME", "/home/u");
        let p = protected_paths();
        assert!(p.contains(&PathBuf::from("/home/u/.claude/settings.json")));
        assert!(p.contains(&PathBuf::from("/home/u/.claude/settings.local.json")));
    }

    #[test]
    fn leaves_unrelated_commands_alone() {
        assert!(!hit("cargo test", "/home/u/code/askfirst"));
        assert!(!hit("ls -la /tmp", "/tmp"));
        assert!(!hit("git commit -m askfirst", "/home/u/code/askfirst"));
        assert!(!hit("cat /home/u/.config/other/rules.toml", "/tmp"));
    }
}
