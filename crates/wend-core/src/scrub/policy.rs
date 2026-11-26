//! Which tool calls are sensitive, and which ones ran wend itself.

use regex::Regex;
use std::sync::OnceLock;

/// Does this path or command touch a secrets file, or print the environment
/// or a credential?
pub(crate) fn is_sensitive(s: &str) -> bool {
    static PATH: OnceLock<Regex> = OnceLock::new();
    static CMD: OnceLock<Regex> = OnceLock::new();
    let path = PATH.get_or_init(|| {
        Regex::new(
            r#"(?i)(?:^|[\s/'"=:(])(?:\.env(?:\.[\w.-]+)?|\.envrc|[\w.-]*\.pem|[\w.-]*\.key|id_(?:rsa|dsa|ecdsa|ed25519)(?:\.pub)?|credentials[\w.-]*|\.netrc|\.npmrc|\.pypirc|\.git-credentials|[\w.-]*\.tfvars|kubeconfig)(?:$|[\s'";|&)>,])|\.aws/|\.kube/config|\.config/gh/hosts\.ya?ml"#,
        )
        .expect("sensitive path pattern must compile")
    });
    let cmd = CMD.get_or_init(|| {
        Regex::new(
            r"(?:^|[\s;&|(`])(?:printenv\b|env\s*(?:$|[;&|)])|gh\s+auth\s+token|aws\s+configure\s+export-credentials|docker\s+compose\s+config|docker-compose\s+config|terraform\s+output|vercel\s+env\s+pull|heroku\s+config|kubectl\s+get\s+secrets?)",
        )
        .expect("sensitive command pattern must compile")
    });
    path.is_match(s) || cmd.is_match(s)
}

/// `bash`, `/bin/zsh`, `sh` ... by basename.
pub(crate) fn is_shell(program: &str) -> bool {
    let base = program.rsplit('/').next().unwrap_or(program);
    matches!(base, "bash" | "sh" | "zsh" | "dash" | "fish" | "ksh")
}

/// A flag cluster that ends in `c` (`-c`, `-lc`, `-ic`): the next argument is
/// a script.
pub(crate) fn is_c_flag(flag: &str) -> bool {
    flag.starts_with('-') && !flag.starts_with("--") && flag.ends_with('c')
}

/// Does this shell command run a program named `wend` in any segment?
pub(crate) fn runs_wend(command: &str) -> bool {
    segments(command)
        .iter()
        .any(|seg| segment_runs_wend(seg, 0))
}

fn segments(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        let split = match c {
            '\n' | ';' | '`' => true,
            '&' | '|' => {
                if chars.peek() == Some(&c) {
                    chars.next();
                }
                true
            }
            '$' if chars.peek() == Some(&'(') => {
                chars.next();
                true
            }
            '(' | ')' => true,
            _ => false,
        };
        if split {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    out.push(cur);
    out
}

fn segment_runs_wend(segment: &str, depth: usize) -> bool {
    let tokens: Vec<&str> = segment.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i].trim_matches(|c| c == '"' || c == '\'');
        let is_assignment = t.contains('=') && !t.starts_with('=') && !t.contains('/');
        match t {
            "env" | "sudo" | "time" | "nohup" | "exec" | "command" => i += 1,
            "xargs" => {
                i += 1;
                while i < tokens.len() && tokens[i].starts_with('-') {
                    i += 1;
                }
            }
            _ if is_shell(t) && tokens.get(i + 1).is_some_and(|f| is_c_flag(f)) && depth < 2 => {
                let rest = tokens[i + 2..].join(" ");
                let rest = rest.trim_matches(|c| c == '"' || c == '\'');
                return segments(rest)
                    .iter()
                    .any(|s| segment_runs_wend(s, depth + 1));
            }
            _ if is_assignment => i += 1,
            _ => {
                let base = t.rsplit('/').next().unwrap_or(t);
                return base == "wend";
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_paths_and_commands() {
        for s in [
            "/repo/.env",
            "cat .env.local",
            "/home/u/.ssh/id_ed25519",
            "server.pem",
            "~/.aws/credentials",
            "terraform.tfvars",
            "printenv",
            "env | sort",
            "gh auth token",
            "kubectl get secret db -o yaml",
            "cat ~/.config/gh/hosts.yml",
            "docker compose config",
        ] {
            assert!(is_sensitive(s), "not sensitive: {s}");
        }
        for s in [
            "src/environment.rs",
            "cargo test",
            "env FOO=1 cargo run",
            "src/id_mapping.rs",
            "monkey.rs",
            "README.md",
        ] {
            assert!(!is_sensitive(s), "falsely sensitive: {s}");
        }
    }

    #[test]
    fn wend_as_program_only() {
        for c in [
            "wend search foo",
            "./target/release/wend sync",
            "cd x && wend show abc",
            "FOO=1 wend search x",
            "echo $(wend search x)",
            "ls | xargs -0 wend show",
            "bash -c 'wend search x'",
            "/bin/zsh -lc 'wend search x'",
            "git status\nwend search y",
        ] {
            assert!(runs_wend(c), "missed: {c}");
        }
        for c in [
            "cd ~/coding/wend && cargo test",
            "grep -r wend src",
            "ls /Users/us/coding/wend",
            "cargo build -p wend-cli",
        ] {
            assert!(!runs_wend(c), "false match: {c}");
        }
    }
}
