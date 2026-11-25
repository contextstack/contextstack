//! Normalisation for two hard lines: writes to a Git remote, and credential stores.
//!
//! A hard line sends an action to the owner without asking the reviewer model. The Git check looks
//! at command positions in argv and inline shell code, including commands passed to wrappers.
//!
//! Remote writes use a closed list: a `git` or `gh` invocation skips the owner only when it is a form
//! known not to write to a remote. Anything else, including subcommands added by future releases, an
//! alias that does not resolve to a listed form, and `-c` settings that can run commands or redirect
//! remotes, waits on the owner. A deny list would have to name every write path git and gh offer
//! (`push`, `send-pack`, `http-push`, `remote-*` helpers, `gh pr merge`, `gh api -X POST`, …) and
//! would silently fail open on the next one.
//!
//! Credential stores are matched on normalised absolute paths: `~`, `~user`, `$HOME` and `${HOME}` are
//! expanded, relative paths are taken from the workspace (the action's working directory), `..` is
//! removed lexically, and paths are compared case-insensitively (macOS volumes usually are). Symlinks
//! are resolved twice: the node sends `resolved_operands` it resolved on its own filesystem, and the
//! Director also resolves each path under the home when it exists on the Director's host. Containing
//! directories are protected except for shallow listings. Dynamic paths are always checked conservatively.

use std::path::{Component, Path, PathBuf};

/// Home-relative credential stores. Mirrors `PROTECTED_READS` and `HOSTED_READS` in
/// `sandbox/src/cli_executor.rs` (the paths every agent profile already denies), plus stores the
/// sandbox list does not name.
const CREDENTIAL_STORES: &[&str] = &[
    // Agent sandbox denies.
    ".contextstack/credentials",
    ".contextstack/state/director",
    ".contextstack/dev-state/director",
    ".contextstack/gateway",
    ".contextstack/config/gateway/owner-token",
    ".contextstack/config/node.env",
    ".contextstack/config/.cs-init-node-token",
    ".ssh",
    ".aws",
    ".gnupg",
    ".config/gh",
    ".netrc",
    ".zshrc.secrets",
    "Library/Keychains",
    ".claude/.credentials.json",
    ".codex/auth.json",
    ".gemini/settings.json",
    ".gemini/oauth_creds.json",
    ".gemini/google_accounts.json",
    ".gemini/gemini-credentials.json",
    ".gemini/installation_id",
    ".pi/agent/auth.json",
    ".pi/agent/settings.json",
    ".pi/agent/models.json",
    ".config/goose/config.yaml",
    ".config/goose/chatgpt_codex",
    ".config/goose/gemini_oauth",
    ".config/goose/xai_oauth",
    ".grok/auth.json",
    ".contextstack/director-db-backup",
    ".contextstack/browser",
    ".contextstack/state/browser-state",
    ".contextstack/config/lead-credential",
    ".contextstack/config/hosted-lead.json",
    ".contextstack/state/lead-sessions",
    ".contextstack/dev-state/lead-sessions",
    // OpenCode's login store (`cs/ops/bin/_cli_adapter.py`).
    ".local/share/opencode/opencode.db",
    // Other tools' credential stores.
    ".docker/config.json",
    ".kube",
    ".config/gcloud",
    ".git-credentials",
    ".config/git/credentials",
    "Library/Group Containers/2BUA8C4S2C.com.1password",
    "Library/Containers/com.maxgoedjen.Secretive.SecretAgent",
];

/// Executable helpers and push indicators for dynamically named commands.
const PUSH_WORDS: &[&str] = &[
    "push",
    "--force-push",
    "--force-with-lease",
    "send-pack",
    "http-push",
    "receive-pack",
];

/// Git subcommands that do not write to a remote and do not run a caller-chosen command (the options
/// that would are refused in `git_options`).
const GIT_SUBCOMMANDS: &[&str] = &[
    "add",
    "annotate",
    "apply",
    "archive",
    "bisect",
    "blame",
    "branch",
    "bundle",
    "cat-file",
    "check-attr",
    "check-ignore",
    "check-mailmap",
    "check-ref-format",
    "checkout",
    "cherry",
    "cherry-pick",
    "clean",
    "clone",
    "commit",
    "commit-tree",
    "config",
    "count-objects",
    "describe",
    "diff",
    "diff-files",
    "diff-index",
    "diff-tree",
    "fetch",
    "for-each-ref",
    "format-patch",
    "fsck",
    "gc",
    "grep",
    "hash-object",
    "help",
    "init",
    "log",
    "ls-files",
    "ls-remote",
    "ls-tree",
    "merge",
    "merge-base",
    "mv",
    "name-rev",
    "notes",
    "pull",
    "range-diff",
    "read-tree",
    "rebase",
    "reflog",
    "remote",
    "reset",
    "restore",
    "rev-list",
    "rev-parse",
    "revert",
    "rm",
    "shortlog",
    "show",
    "show-branch",
    "show-ref",
    "sparse-checkout",
    "stash",
    "status",
    "submodule",
    "switch",
    "symbolic-ref",
    "tag",
    "update-index",
    "update-ref",
    "var",
    "verify-commit",
    "verify-tag",
    "version",
    "whatchanged",
    "worktree",
    "write-tree",
];

/// `git -c` keys that neither run commands nor redirect remotes. `alias.*` is resolved separately.
fn safe_git_config(key: &str, value: Option<&str>) -> bool {
    let key = key.to_ascii_lowercase();
    ["color.", "advice.", "column."]
        .iter()
        .any(|prefix| key.starts_with(prefix))
        || matches!(
            key.as_str(),
            "core.quotepath" | "user.name" | "user.email" | "init.defaultbranch" | "safe.directory"
        )
        || (key == "core.fsmonitor"
            && value.is_some_and(|value| {
                matches!(
                    value.to_ascii_lowercase().as_str(),
                    "false" | "no" | "off" | "0"
                )
            }))
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Word(String),
    /// A shell command boundary (`;`, `&`, `|`, `(`, `)`, a backtick, a redirect or a newline).
    Break,
    /// A shell redirection, followed by its target word.
    Redirect,
}

const SHELL_TEXT: &[char] = &[
    ' ', '\t', '\n', '\r', '\'', '"', '\\', ';', '&', '|', '(', ')', '`', '<', '>', '[', ']', ',',
    '{', '}',
];

/// Splits shell-like text into words, removing quotes and backslashes. With `code`, `[`, `]`, `,`,
/// `{` and `}` separate words too, so `['git','push']` in inline Python or `{push,}` brace expansion
/// are seen; without it they stay in the word, where a path check reads them as glob characters.
fn split(text: &str, code: bool) -> Vec<Token> {
    tokenize(text, code, false, &mut Vec::new())
}

fn tokenize(text: &str, code: bool, shell: bool, substitutions: &mut Vec<String>) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut chars = text.chars().peekable();
    let flush = |tokens: &mut Vec<Token>, word: &mut String, started: &mut bool| {
        if *started {
            tokens.push(Token::Word(std::mem::take(word)));
        }
        *started = false;
    };
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                started = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    word.push(c);
                }
            }
            '"' => {
                started = true;
                double_quoted(&mut chars, &mut word, shell, substitutions);
            }
            '\\' => {
                started = true;
                if let Some(next) = chars.next().filter(|next| *next != '\n') {
                    word.push(next);
                }
            }
            '$' if chars.peek() == Some(&'{') => {
                started = true;
                word.push('$');
                for c in chars.by_ref() {
                    word.push(c);
                    if c == '}' {
                        break;
                    }
                }
            }
            '$' | '<' | '>' if shell && chars.peek() == Some(&'(') => {
                chars.next();
                substitutions.push(substitution(&mut chars, ')'));
                started = true;
                word.push_str("$()");
            }
            '`' if shell => {
                substitutions.push(substitution(&mut chars, '`'));
                started = true;
                word.push_str("$()");
            }
            '<' | '>' if shell => {
                if started && word.chars().all(|c| c.is_ascii_digit()) {
                    word.clear();
                    started = false;
                }
                flush(&mut tokens, &mut word, &mut started);
                while chars
                    .next_if(|next| matches!(next, '<' | '>' | '&' | '|' | '-'))
                    .is_some()
                {}
                tokens.push(Token::Redirect);
            }
            '#' if shell && !started => {
                if chars.by_ref().any(|c| c == '\n') {
                    tokens.push(Token::Break);
                }
            }
            ';' | '&' | '|' | '(' | ')' | '`' | '<' | '>' | '\n' => {
                flush(&mut tokens, &mut word, &mut started);
                tokens.push(Token::Break);
            }
            c if c.is_whitespace() || (code && matches!(c, '[' | ']' | ',' | '{' | '}')) => {
                flush(&mut tokens, &mut word, &mut started);
            }
            c => {
                started = true;
                word.push(c);
            }
        }
    }
    flush(&mut tokens, &mut word, &mut started);
    tokens
}

fn double_quoted(
    chars: &mut std::iter::Peekable<impl Iterator<Item = char>>,
    word: &mut String,
    shell: bool,
    substitutions: &mut Vec<String>,
) {
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => {
                if let Some(next) = chars.next() {
                    if !matches!(next, '"' | '\\' | '$' | '`' | '\n') {
                        word.push('\\');
                    }
                    if next != '\n' {
                        word.push(next);
                    }
                }
            }
            '$' if shell && chars.peek() == Some(&'(') => {
                chars.next();
                substitutions.push(substitution(chars, ')'));
                word.push_str("$()");
            }
            '`' if shell => substitutions.push(substitution(chars, '`')),
            c => word.push(c),
        }
    }
}

/// Every token stream the Director can see: the argv as executed, then each word that holds shell
/// text re-read as a command, recursively (to a fixed depth) for strings quoted inside it.
fn streams(argv: &[&str], code: bool) -> Vec<Vec<Token>> {
    let mut out = vec![
        argv.iter()
            .map(|word| Token::Word((*word).to_owned()))
            .collect::<Vec<_>>(),
    ];
    let mut next = 0;
    for _depth in 0..6 {
        let end = out.len();
        for index in next..end {
            let words = out[index]
                .iter()
                .filter_map(|token| match token {
                    Token::Word(word) if word.contains(SHELL_TEXT) => Some(word.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            out.extend(words.iter().map(|word| split(word, code)));
        }
        next = end;
    }
    out
}

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// Collects inline command substitutions embedded in double-quoted shell words.
fn substitution(chars: &mut impl Iterator<Item = char>, end: char) -> String {
    let mut text = String::new();
    let mut depth = 1;
    let mut quote = None;
    while let Some(c) = chars.next() {
        if c == '\\' {
            text.push(c);
            text.extend(chars.next());
            continue;
        }
        if quote.is_none() {
            if c == end {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            } else if end == ')' && c == '(' {
                depth += 1;
            } else if matches!(c, '\'' | '"') {
                quote = Some(c);
            }
        } else if quote == Some(c) {
            quote = None;
        }
        text.push(c);
    }
    text
}

fn refused(form: &str) -> String {
    let form: String = form.chars().take(120).collect();
    format!(
        "Git push requires an owner decision: `{form}` can write to a Git remote or run another \
         command; only read-only git and gh forms skip the owner."
    )
}

/// None when every visible git and gh invocation is on the permitted list.
pub(super) fn remote_write(words: &[&str], resolved_executable: Option<&str>) -> Option<String> {
    let (command, operands) = words.split_first()?;
    let args = operands
        .iter()
        .map(|word| (*word).to_owned())
        .collect::<Vec<_>>();
    resolved_executable
        .and_then(|executable| invocation(executable, &args, 0))
        .or_else(|| invocation(command, &args, 0))
}

fn invocation(command: &str, args: &[String], depth: usize) -> Option<String> {
    if depth > 16 {
        return Some(refused("nested command"));
    }
    let name = basename(command);
    match name {
        "git" => git(args, &mut Vec::new(), 0),
        "gh" => gh(args),
        "send-pack" | "http-push" | "receive-pack" => Some(refused(command)),
        name if name.strip_prefix("git-").is_some_and(|helper| {
            PUSH_WORDS.contains(&helper) || helper.starts_with("remote-")
        }) =>
        {
            Some(refused(command))
        }
        "sh" | "bash" | "dash" | "zsh" | "ksh" | "ash" | "fish" => {
            let mut options = args.iter();
            while let Some(option) = options.next() {
                match option.as_str() {
                    "-o" | "+o" | "-O" | "+O" | "--rcfile" | "--init-file" => {
                        options.next();
                    }
                    "--" => break,
                    _ if option.starts_with("--") => {}
                    _ if option.starts_with('-') && option[1..].contains('c') => {
                        return shell_write(options.next()?, depth + 1);
                    }
                    _ if option.starts_with(['-', '+']) => {}
                    _ => break,
                }
            }
            None
        }
        "eval" => shell_write(&args.join(" "), depth + 1),
        "find" => args.iter().enumerate().find_map(|(index, arg)| {
            if matches!(arg.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir") {
                let rest = &args[index + 1..];
                let end = rest
                    .iter()
                    .position(|arg| arg == ";" || arg == "+")
                    .unwrap_or(rest.len());
                command_words(&rest[..end], depth + 1)
            } else {
                None
            }
        }),
        "command" | "exec" | "builtin" | "nohup" | "time" => {
            let mut args = args.iter();
            while let Some(arg) = args.next() {
                match (name, arg.as_str()) {
                    ("command", "-v" | "-V" | "-pv" | "-pV") => return None,
                    ("exec", "-a") => {
                        args.next();
                    }
                    (_, "--") => return command_words(args.as_slice(), depth + 1),
                    (_, flag) if flag.starts_with('-') => {}
                    _ => return invocation(arg, args.as_slice(), depth + 1),
                }
            }
            None
        }
        "env" => env_write(args, depth + 1),
        "sudo" | "doas" | "xargs" | "timeout" | "nice" | "setsid" | "chronic" | "stdbuf"
        | "watch" | "ssh" | "su" | "runuser" | "flock" | "busybox" => {
            args.iter().enumerate().find_map(|(index, arg)| {
                invocation(arg, &args[index + 1..], depth + 1).or_else(|| {
                    arg.contains(SHELL_TEXT)
                        .then(|| shell_write(arg, depth + 1))
                        .flatten()
                })
            })
        }
        name if matches!(
            name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.'),
            "python" | "perl" | "ruby" | "node" | "php" | "lua" | "awk" | "gawk" | "osascript"
        ) =>
        {
            // Inline programs may hand argv arrays or strings to process APIs.
            let words = args.iter().map(String::as_str).collect::<Vec<_>>();
            streams(&words, true).iter().find_map(|stream| {
                stream
                    .split(|token| *token == Token::Break)
                    .find_map(|segment| {
                        let words = segment_words(segment);
                        if words.iter().any(|word| PUSH_WORDS.contains(&word.as_str())) {
                            return Some(push_reason());
                        }
                        words.iter().enumerate().find_map(|(index, word)| {
                            invocation(word, &words[index + 1..], depth + 1)
                        })
                    })
            })
        }
        _ if command.contains(['$', '`', '*', '?', '[', '{'])
            && args.iter().any(|arg| PUSH_WORDS.contains(&arg.as_str())) =>
        {
            Some(push_reason())
        }
        _ => None,
    }
}

fn env_write(args: &[String], depth: usize) -> Option<String> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--" => return command_words(args.as_slice(), depth),
            "-u" | "--unset" | "-C" | "--chdir" | "-a" | "--argv0" => {
                args.next();
            }
            "-S" | "--split-string" => {
                let mut command = segment_words(&split(args.next()?, false));
                command.extend_from_slice(args.as_slice());
                return command_words(&command, depth);
            }
            _ if arg.starts_with("-S") || arg.starts_with("--split-string=") => {
                let text = arg
                    .strip_prefix("-S")
                    .or_else(|| arg.strip_prefix("--split-string="))?;
                let mut command = segment_words(&split(text, false));
                command.extend_from_slice(args.as_slice());
                return command_words(&command, depth);
            }
            _ if arg.starts_with('-') || arg.contains('=') => {}
            _ => return invocation(arg, args.as_slice(), depth),
        }
    }
    None
}

fn push_reason() -> String {
    "Git push requires an owner decision.".to_owned()
}

fn segment_words(segment: &[Token]) -> Vec<String> {
    let mut words = Vec::new();
    let mut redirect = false;
    for token in segment {
        match token {
            Token::Redirect => redirect = true,
            Token::Word(_) if redirect => redirect = false,
            Token::Word(word) => words.push(word.clone()),
            Token::Break => {}
        }
    }
    words
}

fn command_words(words: &[String], depth: usize) -> Option<String> {
    let index = words.iter().position(|word| {
        !(word.split_once('=').is_some_and(|(key, _)| {
            !key.is_empty()
                && key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                && !key.as_bytes()[0].is_ascii_digit()
        }) || matches!(
            word.as_str(),
            "if" | "then" | "elif" | "else" | "while" | "until" | "do" | "!" | "{" | "coproc"
        ))
    })?;
    invocation(&words[index], &words[index + 1..], depth)
}

fn shell_write(text: &str, depth: usize) -> Option<String> {
    if depth > 16 {
        return Some(refused("nested shell command"));
    }
    let mut substitutions = Vec::new();
    let tokens = tokenize(text, false, true, &mut substitutions);
    tokens
        .split(|token| *token == Token::Break)
        .find_map(|segment| command_words(&segment_words(segment), depth))
        .or_else(|| {
            substitutions
                .iter()
                .find_map(|text| shell_write(text, depth + 1))
        })
}

/// Classifies `git <args>`; `aliases` holds `-c alias.*` definitions seen so far.
fn git(args: &[String], aliases: &mut Vec<(String, String)>, depth: usize) -> Option<String> {
    if depth > 8 {
        return Some(refused("git alias that expands too deeply"));
    }
    let mut index = 0;
    let subcommand = loop {
        let arg = args.get(index)?;
        if !arg.starts_with('-') {
            break arg;
        }
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if arg.starts_with("--") => (flag, Some(value)),
            _ => (arg.as_str(), None),
        };
        let mut value = || {
            inline.map(str::to_owned).or_else(|| {
                index += 1;
                args.get(index).cloned()
            })
        };
        match flag {
            "-C" | "--git-dir" | "--work-tree" | "--namespace" | "--list-cmds" => {
                value();
            }
            "-c" => {
                let setting = value().unwrap_or_default();
                let (key, value) = match setting.split_once('=') {
                    Some((key, value)) => (key, Some(value)),
                    None => (setting.as_str(), None),
                };
                if let Some(name) = key.to_ascii_lowercase().strip_prefix("alias.") {
                    let value = value.unwrap_or_default();
                    if value.trim_start().starts_with('!') {
                        return Some(refused(&format!("git -c alias.{name}={value}")));
                    }
                    aliases.push((name.to_owned(), value.to_owned()));
                } else if !safe_git_config(key, value) {
                    return Some(refused(&format!("git -c {key}")));
                }
            }
            "--config-env" => {
                // The value comes from an environment variable the Director cannot see.
                let setting = value().unwrap_or_default();
                return Some(refused(&format!("git --config-env={setting}")));
            }
            "--exec-path" if inline.is_some() => {
                return Some(refused(arg));
            }
            "-p"
            | "--paginate"
            | "-P"
            | "--no-pager"
            | "--bare"
            | "--no-replace-objects"
            | "--literal-pathspecs"
            | "--glob-pathspecs"
            | "--noglob-pathspecs"
            | "--icase-pathspecs"
            | "--no-optional-locks"
            | "--no-lazy-fetch"
            | "--no-advice"
            | "--exec-path"
            | "--html-path"
            | "--man-path"
            | "--info-path"
            | "--version"
            | "-v"
            | "--help"
            | "-h" => {}
            _ => return Some(refused(&format!("git {arg}"))),
        }
        index += 1;
    };
    let rest = &args[index + 1..];
    if subcommand == "push" {
        return Some(push_reason());
    }
    // Git runs a builtin before an alias of the same name; aliases are matched case-insensitively.
    if GIT_SUBCOMMANDS.contains(&subcommand.as_str()) {
        return git_options(subcommand, rest);
    }
    let lowered = subcommand.to_ascii_lowercase();
    let Some((_, expansion)) = aliases.iter().rev().find(|(name, _)| *name == lowered) else {
        return Some(refused(&format!("git {subcommand}")));
    };
    let mut expanded = split(expansion, false)
        .into_iter()
        .filter_map(|token| match token {
            Token::Word(word) => Some(word),
            Token::Break | Token::Redirect => None,
        })
        .collect::<Vec<_>>();
    expanded.extend_from_slice(rest);
    let mut aliases = aliases.clone();
    git(&expanded, &mut aliases, depth + 1)
}

/// Options of listed subcommands that would run a caller-chosen command or change config.
fn git_options(subcommand: &str, args: &[String]) -> Option<String> {
    let has = |prefixes: &[&str]| {
        args.iter()
            .find(|arg| prefixes.iter().any(|prefix| arg.starts_with(prefix)))
    };
    let positional = || args.iter().find(|arg| !arg.starts_with('-'));
    let found = match subcommand {
        "clone" => has(&["--upload-pack", "-u", "--config", "-c", "--template"]),
        "fetch" | "pull" => has(&["--upload-pack"]),
        "ls-remote" => has(&["--upload-pack", "-u"]),
        "archive" => has(&["--exec"]),
        "grep" => has(&["-O", "--open-files-in-pager"]),
        "rebase" => has(&["--exec"]).or_else(|| {
            args.iter()
                .find(|arg| !arg.starts_with("--") && arg.starts_with('-') && arg.contains('x'))
        }),
        "bisect" => positional().filter(|arg| *arg == "run"),
        "submodule" => positional().filter(|arg| {
            !matches!(
                arg.as_str(),
                "status" | "summary" | "init" | "update" | "sync" | "absorbgitdirs"
            )
        }),
        "config" => {
            let reads = [
                "--get",
                "--get-all",
                "--get-regexp",
                "--get-urlmatch",
                "-l",
                "--list",
            ];
            let read = args.iter().any(|arg| reads.contains(&arg.as_str()))
                || positional().is_some_and(|arg| arg == "get" || arg == "list");
            let writes = [
                "--add",
                "--unset",
                "--unset-all",
                "--replace-all",
                "--rename-section",
                "--remove-section",
                "-e",
                "--edit",
            ];
            match args.iter().find(|arg| writes.contains(&arg.as_str())) {
                Some(arg) => Some(arg),
                None if args
                    .iter()
                    .any(|arg| arg.to_ascii_lowercase().starts_with("credential.")) =>
                {
                    return Some(refused("git config credential settings"));
                }
                None if read => None,
                None if matches!(args, [key, _] if matches!(key.to_ascii_lowercase().as_str(), "user.name" | "user.email")) => {
                    None
                }
                None => return Some(refused("git config (writing settings)")),
            }
        }
        _ => None,
    };
    found.map(|arg| refused(&format!("git {subcommand} {arg}")))
}

/// Classifies `gh <args>` against the read-only list.
fn gh(args: &[String]) -> Option<String> {
    let group = args.first()?;
    let command = args.get(1).map_or("", String::as_str);
    let allowed = match group.as_str() {
        "--version" | "version" | "--help" | "-h" | "help" | "status" => true,
        "api" => return gh_api(&args[1..]),
        "search" => !command.is_empty(),
        "pr" => matches!(command, "view" | "list" | "diff" | "checks" | "status"),
        "issue" => matches!(command, "view" | "list" | "status"),
        "repo" => {
            matches!(command, "view" | "list")
                || (command == "clone" && !args.iter().any(|arg| arg == "--"))
        }
        "run" => matches!(command, "view" | "list" | "watch" | "download"),
        "workflow" => matches!(command, "view" | "list"),
        "release" => matches!(command, "view" | "list" | "download"),
        "label" | "cache" => command == "list",
        _ => false,
    };
    (!allowed).then(|| refused(&format!("gh {group} {command}")))
}

/// `gh api` is read-only only as a GET without request fields; GraphQL may hold mutations.
fn gh_api(args: &[String]) -> Option<String> {
    let mut endpoint = None;
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        let method = if arg == "-X" || arg == "--method" {
            index += 1;
            Some(args.get(index).map_or("", String::as_str))
        } else {
            arg.strip_prefix("--method=").or_else(|| {
                arg.strip_prefix("-X")
                    .map(|method| method.trim_start_matches('='))
            })
        };
        if let Some(method) = method {
            if !method.eq_ignore_ascii_case("GET") {
                return Some(refused(&format!("gh api --method {method}")));
            }
        } else if ["--field", "--raw-field", "--input"]
            .iter()
            .any(|flag| arg == flag || arg.starts_with(&format!("{flag}=")))
            || (arg.starts_with('-')
                && !arg.starts_with("--")
                && arg[1..].contains(['f', 'F', 'X']))
        {
            return Some(refused(&format!("gh api {arg}")));
        } else if !arg.starts_with('-') && endpoint.is_none() {
            endpoint = Some(arg.as_str());
        }
        index += 1;
    }
    endpoint
        .filter(|endpoint| {
            endpoint
                .trim_start_matches('/')
                .eq_ignore_ascii_case("graphql")
        })
        .map(|_| refused("gh api graphql"))
}

/// Removes `.` and `..` without touching the filesystem (`..` at the root stays at the root).
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            part => out.push(part),
        }
    }
    out
}

/// The absolute path a word names when used as an operand, or none for flags and URLs. A word
/// with a part only known at run time (`$name`, `$'…'`, a glob or brace) yields the directory
/// before that part, marked dynamic.
fn absolute(word: &str, home: &Path, workspace: &Path) -> Option<(PathBuf, bool)> {
    if word.is_empty() || word.starts_with('-') || word.contains("://") {
        return None;
    }
    let home_text = home.to_string_lossy();
    let expanded = if let Some(rest) = ["$HOME", "${HOME}"]
        .iter()
        .find_map(|prefix| word.strip_prefix(prefix))
        .filter(|rest| rest.is_empty() || rest.starts_with('/'))
    {
        format!("{home_text}{rest}")
    } else if let Some(rest) = word.strip_prefix('~') {
        let (user, rest) = rest
            .split_once('/')
            .map_or((rest, ""), |(user, rest)| (user, rest));
        if user.is_empty() || Some(std::ffi::OsStr::new(user)) == home.file_name() {
            format!("{home_text}/{rest}")
        } else {
            let parent = home.parent()?.to_string_lossy();
            format!("{parent}/{user}/{rest}")
        }
    } else {
        word.to_owned()
    };
    let (fixed, dynamic) = match expanded.find(['$', '*', '?', '[', '{']) {
        Some(index) => {
            let fixed = &expanded[..index];
            (
                &fixed[..fixed.rfind('/').map_or(0, |slash| slash + 1)],
                true,
            )
        }
        None => (expanded.as_str(), false),
    };
    Some((lexical(&workspace.join(fixed)), dynamic))
}

/// The path with symlinks resolved on this host, for the longest prefix that exists.
fn canonical(path: &Path) -> Option<PathBuf> {
    let mut base = path;
    let mut rest = Vec::new();
    loop {
        if let Ok(mut real) = std::fs::canonicalize(base) {
            real.extend(rest.iter().rev());
            return Some(lexical(&real));
        }
        rest.push(base.file_name()?);
        base = base.parent()?;
    }
}

fn lower(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().to_lowercase())
}

fn stores(home: &Path) -> impl Iterator<Item = PathBuf> {
    let home = lower(home);
    CREDENTIAL_STORES
        .iter()
        .map(move |relative| home.join(relative.to_lowercase()))
}

/// The path is a credential store or lies inside one; or it lies in the home (or, when part of it
/// is only known at run time, above the home) and contains one.
fn store(path: &Path, home: &Path, dynamic: bool, listing: bool) -> bool {
    let path = lower(path);
    let contains = path.starts_with(lower(home)) || (dynamic && lower(home).starts_with(&path));
    stores(home).any(|store| {
        path.starts_with(&store) || (contains && (!listing || dynamic) && store.starts_with(&path))
    })
}

/// Recognizes direct listings whose options neither recurse nor open directory entries.
pub(super) fn shallow_listing(words: &[&str], resolved_executable: Option<&str>) -> bool {
    let Some((command, args)) = words.split_first() else {
        return false;
    };
    let name = basename(command);
    let system_listing = |path: &str| {
        basename(path) == name
            && matches!(
                Path::new(path).parent().and_then(Path::to_str),
                Some("/bin" | "/usr/bin")
            )
    };
    if (command.contains('/') && !system_listing(command))
        || resolved_executable.is_some_and(|path| !system_listing(path))
    {
        return false;
    }
    match name {
        "ls" => args.iter().take_while(|arg| **arg != "--").all(|arg| {
            arg.strip_prefix('-')
                .is_none_or(|flags| flags.chars().all(|flag| "aAld1h".contains(flag)))
        }),
        "find" => matches!(
            args,
            [path, "-maxdepth", "0" | "1"]
            | [path, "-maxdepth", "0" | "1", "-name" | "-iname", _]
            if !path.starts_with(['-', '!', '('])
        ),
        _ => false,
    }
}

/// True when any word, write path, resolved operand or the executable names a credential store.
/// `roots` are the workspace (the action's working directory) and the job tmp directory; paths
/// inside a root that itself contains no store are the lane's own files.
pub(super) fn credential_store<'a>(
    words: impl Iterator<Item = &'a str>,
    home: &Path,
    roots: &[&Path],
    listing: bool,
) -> bool {
    let words = words.collect::<Vec<_>>();
    let Some(workspace) = roots.first() else {
        return true;
    };
    let real_home = canonical(home);
    let own = |path: &Path, home: &Path| {
        roots.iter().any(|root| {
            lower(path).starts_with(lower(root))
                && !stores(home).any(|store| store.starts_with(lower(root)))
        })
    };
    let flagged = |path: &Path, dynamic: bool, home: &Path| {
        !own(path, home) && store(path, home, dynamic, listing)
    };
    let mut candidates = Vec::new();
    for code in [false, true] {
        for token in streams(&words, code).into_iter().flatten() {
            if let Token::Word(word) = token {
                candidates.extend(
                    [
                        Some(word.as_str()),
                        word.split_once('=').map(|(_, value)| value),
                    ]
                    .into_iter()
                    .flatten()
                    .filter_map(|word| absolute(word, home, workspace)),
                );
            }
        }
    }
    candidates.into_iter().any(|(path, dynamic)| {
        flagged(&path, dynamic, home)
            || (path.starts_with(home)
                && canonical(&path).is_some_and(|real| {
                    flagged(&real, dynamic, home)
                        || real_home
                            .as_deref()
                            .is_some_and(|home| flagged(&real, dynamic, home))
                }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        split(text, true)
            .into_iter()
            .map(|token| match token {
                Token::Word(word) => word,
                Token::Break | Token::Redirect => ";".into(),
            })
            .collect()
    }

    #[test]
    fn split_follows_shell_quoting() {
        assert_eq!(words("git pu''sh"), ["git", "push"]);
        assert_eq!(words(r#"a "b c" 'd;e' f\ g"#), ["a", "b c", "d;e", "f g"]);
        assert_eq!(words("x && y|z"), ["x", ";", ";", "y", ";", "z"]);
        assert_eq!(words("cat ${HOME}/x"), ["cat", "${HOME}/x"]);
        assert_eq!(
            words("run(['git','push'])"),
            ["run", ";", "git", "push", ";"]
        );
    }

    #[test]
    fn nested_quotes_are_reread() {
        let seen = streams(&["sh", "-c", "bash -c 'git send-pack origin'"], true);
        assert!(seen.iter().any(|stream| {
            stream.first() == Some(&Token::Word("git".into()))
                && stream.get(1) == Some(&Token::Word("send-pack".into()))
        }));
    }

    #[test]
    fn home_forms_expand() {
        let (home, workspace) = (Path::new("/u/me"), Path::new("/u/me/lane"));
        for (word, path) in [
            ("~", "/u/me"),
            ("~/.kube", "/u/me/.kube"),
            ("~me/.kube", "/u/me/.kube"),
            ("$HOME/.kube", "/u/me/.kube"),
            ("${HOME}", "/u/me"),
            ("x/../../.kube", "/u/me/.kube"),
        ] {
            assert_eq!(
                absolute(word, home, workspace),
                Some((PathBuf::from(path), false))
            );
        }
        for (word, path) in [
            ("~/.config/$d/hosts.yml", "/u/me/.config"),
            ("~/.c*/gh", "/u/me"),
            ("/u/$USER/.kube", "/u"),
            ("$'\\x7e'/.kube", "/u/me/lane"),
        ] {
            assert_eq!(
                absolute(word, home, workspace),
                Some((PathBuf::from(path), true))
            );
        }
        assert!(store(Path::new("/U/ME/.KUBE/config"), home, false, false));
        assert!(store(Path::new("/u/me/.config"), home, false, false));
        assert!(store(Path::new("/u"), home, true, false));
        assert!(!store(Path::new("/u"), home, false, false));
        assert!(!store(Path::new("/u/me/lane/.config"), home, true, false));
    }
}
