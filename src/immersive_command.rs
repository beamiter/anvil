//! Conservative command-line classification for the space-saving Blocks bar.
//! Keep in sync between Ember and Frost. No subprocesses or shell evaluation.

pub(crate) fn immersive_block_command(command: &str) -> bool {
    classify(command, false)
}

pub(crate) fn reading_agent_command(command: &str) -> bool {
    classify(command, true)
}

fn classify(command: &str, reading_agent_only: bool) -> bool {
    let Some(words) = shell_words(command) else {
        return false;
    };
    let mut args = words.iter().map(String::as_str).peekable();
    let name = loop {
        let Some(word) = args.next() else {
            return false;
        };
        if assignment(word) {
            continue;
        }
        let name = match word {
            "@anthropic-ai/claude-code" => "claude",
            "@google/gemini-cli" => "gemini",
            _ => word.rsplit('/').next().unwrap_or(word),
        };
        match name {
            "command" | "exec" => {
                if args.peek() == Some(&"--") {
                    args.next();
                }
            }
            "env" => {
                while let Some(&arg) = args.peek() {
                    match arg {
                        "--" => {
                            args.next();
                            break;
                        }
                        "-i" | "--ignore-environment" => {
                            args.next();
                        }
                        "-u" | "--unset" => {
                            args.next();
                            args.next();
                        }
                        _ if assignment(arg) => {
                            args.next();
                        }
                        _ => break,
                    }
                }
            }
            "sudo" => {
                while let Some(&arg) = args.peek() {
                    match arg {
                        "--" => {
                            args.next();
                            break;
                        }
                        "-u" | "-g" => {
                            args.next();
                            args.next();
                        }
                        "-E" | "-H" | "-n" => {
                            args.next();
                        }
                        _ => break,
                    }
                }
            }
            "npx" | "bunx" => {
                if matches!(args.peek(), Some(&"--yes" | &"-y" | &"--no-install")) {
                    args.next();
                }
            }
            "pnpm" => {
                if !matches!(args.next(), Some("dlx" | "exec")) {
                    return false;
                }
            }
            _ => break name,
        }
    };
    let agent = matches!(name, "codex" | "claude" | "gemini" | "opencode" | "aider");
    let mut positional = false;
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        if matches!(arg, "-h" | "--help" | "--version") {
            return false;
        }
        // Flags belong to individual CLIs: Codex -p is a profile, not print.
        if matches!(
            (name, arg),
            ("claude", "-p" | "--print")
                | ("gemini", "-p" | "--prompt")
                | ("aider", "--message" | "-m")
        ) || (name == "gemini" && arg.starts_with("--prompt="))
            || (name == "aider" && arg.starts_with("--message="))
        {
            return false;
        }
        if matches!(arg, "--model" | "-m" | "--config" | "-c")
            || (name == "codex"
                && matches!(
                    arg,
                    "-p" | "--profile"
                        | "-C"
                        | "--cd"
                        | "-s"
                        | "--sandbox"
                        | "-a"
                        | "--ask-for-approval"
                ))
        {
            args.next();
            continue;
        }
        if agent && !positional && !arg.starts_with('-') {
            positional = true;
            if matches!(
                (name, arg),
                (
                    "codex",
                    "exec"
                        | "e"
                        | "review"
                        | "login"
                        | "logout"
                        | "completion"
                        | "mcp"
                        | "mcp-server"
                        | "app-server"
                        | "apply"
                ) | ("claude", "auth" | "mcp" | "update" | "doctor" | "install")
                    | ("gemini", "mcp" | "extensions")
                    | ("opencode", "run" | "serve" | "auth" | "models" | "mcp")
            ) {
                return false;
            }
        }
    }
    if reading_agent_only {
        return matches!(name, "codex" | "claude");
    }
    matches!(
        name,
        "vi" | "vim"
            | "nvim"
            | "view"
            | "vimdiff"
            | "emacs"
            | "nano"
            | "hx"
            | "helix"
            | "kak"
            | "less"
            | "most"
            | "more"
            | "top"
            | "htop"
            | "btop"
            | "yazi"
            | "ranger"
            | "nnn"
            | "tmux"
            | "screen"
            | "lazygit"
            | "lazydocker"
            | "k9s"
    ) || agent
}

fn assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(key, _)| {
        !key.is_empty()
            && key
                .bytes()
                .enumerate()
                .all(|(i, c)| c == b'_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
    })
}

// Parse only simple argv, preserving quoted option values. Compound commands
// are ambiguous; alternate-screen detection remains authoritative for them.
fn shell_words(command: &str) -> Option<Vec<String>> {
    if command.len() > 16 * 1024 {
        return None;
    }
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut started = false;
    for ch in command.chars() {
        if escaped {
            word.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            started = true;
            continue;
        }
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else {
                word.push(ch);
            }
        } else {
            match ch {
                '\'' | '"' => {
                    quote = Some(ch);
                    started = true;
                }
                ';' | '|' | '&' | '<' | '>' | '`' | '\n' => return None,
                c if c.is_whitespace() => {
                    if started {
                        words.push(std::mem::take(&mut word));
                        started = false;
                    }
                }
                _ => {
                    word.push(ch);
                    started = true;
                }
            }
        }
    }
    if escaped || quote.is_some() {
        return None;
    }
    if started {
        words.push(word);
    }
    Some(words)
}

#[cfg(test)]
mod tests {
    use super::immersive_block_command as tui;
    #[test]
    fn interactive_launchers_and_profiles() {
        for command in [
            "vim",
            "/usr/bin/vim README.md",
            "codex --no-alt-screen",
            "codex -p work",
            "codex --profile exec",
            "codex -c 'model=\"local model\"'",
            "TERM=xterm env FOO=bar codex",
            "env -u TERM codex",
            "command -- vim",
            "exec vim",
            "sudo -u root vim",
            "npx -y @openai/codex",
            "pnpm dlx @openai/codex",
            "bunx @anthropic-ai/claude-code",
            "'/opt/my apps/codex'",
            "codex 'please explain exec and -p'",
        ] {
            assert!(tui(command), "{command}");
        }
    }
    #[test]
    fn batch_and_ambiguous_commands_keep_navigation() {
        for command in [
            "",
            "echo codex",
            "cargo test",
            "codex exec --json",
            "env A=1 codex -p work exec",
            "npx -y @openai/codex exec",
            "claude -p hello",
            "gemini --prompt=hello",
            "opencode run hello",
            "aider --message hello",
            "vim --version",
            "codex --help",
            "codex completion",
            "sudo -l",
            "command -v vim",
            "echo ok; vim",
            "codex &",
            "codex 'unfinished",
        ] {
            assert!(!tui(command), "{command}");
        }
    }
}

#[cfg(test)]
mod reading_tests {
    use super::reading_agent_command as reads;
    #[test]
    fn only_interactive_agent_executables_own_session_paging() {
        for command in [
            "codex --no-alt-screen",
            "claude",
            "env FOO=bar npx -y @anthropic-ai/claude-code",
            "command /opt/bin/codex resume abc",
        ] {
            assert!(reads(command), "{command}");
        }
        for command in [
            "echo codex",
            "vim codex",
            "less claude",
            "codex exec task",
            "claude --print test",
            "claude --help",
            "ssh host codex",
        ] {
            assert!(!reads(command), "{command}");
        }
    }
}
