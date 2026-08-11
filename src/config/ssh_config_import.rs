//! OpenSSH `~/.ssh/config` importer.
//!
//! Supports more of real-world OpenSSH config files than a naive
//! single-pass parser:
//!
//! - **Multiple patterns per `Host` line** (`Host foo bar *.internal`).
//! - **Glob wildcards** `*` and `?` in patterns, and **negation** (`!pattern`)
//!   -- e.g. `Host !bastion.example.com *.example.com` matches every
//!   `*.example.com` host except `bastion.example.com`, exactly like real
//!   `ssh`.
//! - **"First obtained value wins" resolution order**, matching OpenSSH's
//!   actual semantics: for each parameter (HostName/User/Port/IdentityFile),
//!   blocks are scanned in file order and the first matching block that
//!   sets that parameter wins -- so a specific `Host` block before a
//!   trailing `Host *` correctly overrides the wildcard defaults, and a
//!   `Host *` block placed at the top correctly acts as a default for
//!   everything below it.
//! - **`Match host <pattern>` / `Match user <pattern>` / `Match all`** are
//!   treated as additional pattern-matching blocks (same resolution rules
//!   as `Host`). `Match user` is evaluated against the `default_username`
//!   passed to [`import`] -- the username the caller intends to connect
//!   as -- not a fully sequential, mid-parse-resolved username the way
//!   real OpenSSH evaluates it; see "Honest limitation" below.
//!
//! ## Honest limitation
//! Real OpenSSH resolves `ssh_config` as a single sequential pass, so a
//! `Match user` block can react to a `User` value set by an *earlier*
//! block in the same file. This importer instead evaluates `Match user`
//! against the caller-supplied `default_username` context throughout --
//! correct for the common case (the username is decided before you pick
//! which host profile to use), but not bit-for-bit identical to OpenSSH
//! if a file relies on an earlier block changing the effective user
//! before a later `Match user` block is reached.
//!
//! Other `Match` criteria (`exec`, `canonical`, `originalhost`, ...) still
//! aren't supported and are skipped + logged, never guessed.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use super::{AuthMethod, SessionProfile};

#[derive(Debug, Clone)]
struct Pattern {
    negated: bool,
    glob: String,
}

#[derive(Debug)]
enum MatchCriteria {
    Host(Vec<Pattern>),
    /// Matched against the `default_username` passed to `import()` (the
    /// username the caller intends to connect as before any config-file
    /// `User` override) -- see module doc "Honest limitation" for exactly
    /// what this does and doesn't capture relative to real OpenSSH's fully
    /// sequential resolution.
    User(Vec<Pattern>),
    All,
    /// A `Match` criteria we don't understand (`exec`, `canonical`,
    /// `originalhost`, ...). Never matches anything, so its directives are
    /// parsed (keeping file structure sane) but never applied.
    Unsupported,
}

#[derive(Debug)]
struct Block {
    criteria: MatchCriteria,
    directives: Vec<(String, String)>, // preserves file order; first wins on lookup
}

pub fn import(path: &Path, default_username: &str) -> Result<Vec<SessionProfile>> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("reading OpenSSH config at {path:?}"))?;

    let blocks = parse_blocks(&raw);

    // The set of "real" host aliases to surface as importable profiles:
    // every literal (non-wildcard) pattern that appeared in a `Host` line.
    // Wildcard-only blocks (`Host *`, `Host *.internal`) are defaults, not
    // profiles you'd pick from a list; `Match user`/`Match all`/unsupported
    // blocks never contribute alias names.
    let mut alias_order = Vec::new();
    for block in &blocks {
        if let MatchCriteria::Host(patterns) = &block.criteria {
            for p in patterns {
                if !p.negated && !p.glob.contains(['*', '?']) && !alias_order.contains(&p.glob) {
                    alias_order.push(p.glob.clone());
                }
            }
        }
    }

    let mut profiles = Vec::with_capacity(alias_order.len());
    for alias in &alias_order {
        profiles.push(resolve_profile(alias, &blocks, default_username));
    }

    tracing::info!(
        target: "hyperterm::config::ssh_import",
        "imported {} host alias(es) from {:?} ({} total pattern block(s))",
        profiles.len(),
        path,
        blocks.len()
    );
    Ok(profiles)
}

fn parse_blocks(raw: &str) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut current: Option<Block> = None;

    for (lineno, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let key = parts.next().unwrap_or_default().to_ascii_lowercase();
        let value = parts.next().unwrap_or_default().trim();

        match key.as_str() {
            "host" => {
                if let Some(b) = current.take() {
                    blocks.push(b);
                }
                let patterns = value.split_whitespace().map(parse_pattern).collect();
                current = Some(Block { criteria: MatchCriteria::Host(patterns), directives: Vec::new() });
            }
            "match" => {
                if let Some(b) = current.take() {
                    blocks.push(b);
                }
                let criteria = parse_match_criteria(value).unwrap_or_else(|| {
                    tracing::warn!(
                        target: "hyperterm::config::ssh_import",
                        "line {}: unsupported `Match` criteria ('{}'), skipping this block entirely \
                         rather than guessing",
                        lineno + 1,
                        value
                    );
                    MatchCriteria::Unsupported
                });
                current = Some(Block { criteria, directives: Vec::new() });
            }
            "" => {}
            _ => {
                if let Some(b) = current.as_mut() {
                    b.directives.push((key, value.to_string()));
                } else {
                    tracing::debug!(
                        target: "hyperterm::config::ssh_import",
                        "line {}: directive '{}' before any Host/Match block, ignoring",
                        lineno + 1,
                        key
                    );
                }
            }
        }
    }
    if let Some(b) = current.take() {
        blocks.push(b);
    }
    blocks
}

fn parse_pattern(raw: &str) -> Pattern {
    if let Some(stripped) = raw.strip_prefix('!') {
        Pattern { negated: true, glob: stripped.to_string() }
    } else {
        Pattern { negated: false, glob: raw.to_string() }
    }
}

/// Recognizes `Match all`, `Match host <pattern-list>`, and `Match user
/// <pattern-list>`, where `<pattern-list>` is a single, comma-separated
/// token (real OpenSSH `Match` syntax -- unlike `Host` lines, which are
/// space-separated). Returns `None` for any criteria we don't support
/// (`exec`, `canonical`, `originalhost`, ...) *or* any combination of
/// multiple criteria (e.g. `Match user alice host foo`, which is exactly
/// 4 tokens here, not the 2 a single supported criteria produces) --
/// getting AND-vs-OR semantics across mixed criteria subtly wrong would be
/// worse than just not importing that block.
fn parse_match_criteria(value: &str) -> Option<MatchCriteria> {
    let tokens: Vec<&str> = value.split_whitespace().collect();
    if tokens == ["all"] {
        return Some(MatchCriteria::All);
    }
    if tokens.len() == 2 {
        let patterns: Vec<Pattern> = tokens[1].split(',').map(parse_pattern).collect();
        match tokens[0] {
            "host" => return Some(MatchCriteria::Host(patterns)),
            "user" => return Some(MatchCriteria::User(patterns)),
            _ => {}
        }
    }
    None
}

fn patterns_match(patterns: &[Pattern], candidate: &str) -> bool {
    let mut matched = false;
    for p in patterns {
        if glob_match(&p.glob, candidate) {
            if p.negated {
                return false; // an explicit negated match excludes immediately
            }
            matched = true;
        }
    }
    matched
}

fn block_matches(block: &Block, host_alias: &str, default_username: &str) -> bool {
    match &block.criteria {
        MatchCriteria::Host(patterns) => patterns_match(patterns, host_alias),
        MatchCriteria::User(patterns) => patterns_match(patterns, default_username),
        MatchCriteria::All => true,
        MatchCriteria::Unsupported => false,
    }
}

/// Minimal glob matcher supporting `*` (any run of characters, including
/// none) and `?` (exactly one character), which is the pattern language
/// OpenSSH's `ssh_config` actually uses (not full regex).
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_match_rec(&p, &t)
}

fn glob_match_rec(p: &[char], t: &[char]) -> bool {
    match p.first() {
        None => t.is_empty(),
        Some('*') => glob_match_rec(&p[1..], t) || (!t.is_empty() && glob_match_rec(p, &t[1..])),
        Some('?') => !t.is_empty() && glob_match_rec(&p[1..], &t[1..]),
        Some(c) => t.first() == Some(c) && glob_match_rec(&p[1..], &t[1..]),
    }
}

fn lookup<'a>(blocks: &'a [Block], host_alias: &str, default_username: &str, key: &str) -> Option<&'a str> {
    for block in blocks {
        if block_matches(block, host_alias, default_username) {
            if let Some((_, v)) = block.directives.iter().find(|(k, _)| k == key) {
                return Some(v.as_str());
            }
        }
    }
    None
}

fn resolve_profile(host_alias: &str, blocks: &[Block], default_username: &str) -> SessionProfile {
    let hostname = lookup(blocks, host_alias, default_username, "hostname").unwrap_or(host_alias).to_string();
    let username = lookup(blocks, host_alias, default_username, "user")
        .map(|s| s.to_string())
        .unwrap_or_else(|| default_username.to_string());
    let port: u16 = lookup(blocks, host_alias, default_username, "port").and_then(|s| s.parse().ok()).unwrap_or(22);
    let auth = match lookup(blocks, host_alias, default_username, "identityfile") {
        Some(path) => AuthMethod::PublicKey {
            private_key_path: shellexpand_home(path),
            passphrase_env_var: None,
        },
        None => AuthMethod::Agent,
    };

    SessionProfile { name: host_alias.to_string(), host: hostname, port, username, auth }
}

fn shellexpand_home(p: &str) -> std::path::PathBuf {
    if let Some(stripped) = p.strip_prefix("~/") {
        if let Some(base) = directories::BaseDirs::new() {
            return base.home_dir().join(stripped);
        }
    }
    std::path::PathBuf::from(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(contents: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("hyperterm-sshcfg-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("cfg-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn simple_host_block() {
        let path = write_temp(
            "Host myserver\n  HostName 10.0.0.5\n  User admin\n  Port 2222\n  IdentityFile ~/.ssh/id_test\n",
        );
        let profiles = import(&path, "defaultuser").unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].host, "10.0.0.5");
        assert_eq!(profiles[0].username, "admin");
        assert_eq!(profiles[0].port, 2222);
    }

    #[test]
    fn wildcard_block_provides_defaults() {
        let path =
            write_temp("Host *\n  User defaultuser\n  Port 2200\n\nHost specific\n  HostName specific.example.com\n");
        let profiles = import(&path, "defaultuser").unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "specific");
        assert_eq!(profiles[0].host, "specific.example.com");
        assert_eq!(profiles[0].username, "defaultuser");
        assert_eq!(profiles[0].port, 2200);
    }

    #[test]
    fn specific_block_overrides_earlier_wildcard() {
        // Note the order: the specific block must come BEFORE the wildcard
        // for it to win -- this matches real OpenSSH's documented (if
        // surprising) "first obtained value wins, scanned in file order"
        // semantics. A `Host *` placed at the TOP of the file would instead
        // win for every host below it, exactly like real `ssh` -- see
        // `wildcard_block_provides_defaults` above for that case, and the
        // module doc for the semantics this is intentionally matching.
        let path = write_temp("Host override\n  Port 9999\n\nHost *\n  Port 2200\n");
        let profiles = import(&path, "defaultuser").unwrap();
        assert_eq!(profiles[0].port, 9999);
    }

    #[test]
    fn negated_pattern_excludes_host() {
        let path = write_temp(
            "Host !excluded.example.com *.example.com\n  User groupuser\n\nHost excluded.example.com\n  User specificuser\n",
        );
        let profiles = import(&path, "defaultuser").unwrap();
        let excluded = profiles.iter().find(|p| p.name == "excluded.example.com").unwrap();
        assert_eq!(excluded.username, "specificuser");
    }

    #[test]
    fn wildcard_placed_first_shadows_later_specific_block() {
        // Documents the OpenSSH gotcha explicitly: since values are
        // resolved by file order and *not* by specificity, a `Host *`
        // placed before a specific block wins even though the specific
        // block looks more targeted. Real `ssh_config` behaves exactly
        // this way -- this is not a bug, it's why OpenSSH's own docs say
        // to put specific hosts first and wildcards last.
        let path = write_temp("Host *\n  Port 2200\n\nHost shadowed\n  Port 9999\n");
        let profiles = import(&path, "defaultuser").unwrap();
        assert_eq!(profiles[0].name, "shadowed");
        assert_eq!(profiles[0].port, 2200);
    }

    #[test]
    fn unsupported_match_sentinel_never_appears_as_a_fake_profile() {
        let path = write_temp("Match exec \"whoami\"\n  Port 1234\n");
        let profiles = import(&path, "defaultuser").unwrap();
        assert!(profiles.is_empty(), "unsupported Match block must not produce a phantom host profile");
    }

    #[test]
    fn unsupported_match_criteria_is_skipped_not_misapplied() {
        let path = write_temp(
            "Match exec \"some-script.sh\"\n  Port 1234\n\nHost normal\n  HostName normal.example.com\n",
        );
        let profiles = import(&path, "defaultuser").unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].port, 22);
    }

    #[test]
    fn match_all_behaves_like_wildcard_host() {
        let path = write_temp("Match all\n  User matcheduser\n\nHost concrete\n  HostName concrete.example.com\n");
        let profiles = import(&path, "defaultuser").unwrap();
        assert_eq!(profiles[0].username, "matcheduser");
    }

    #[test]
    fn match_user_applies_only_for_matching_default_username() {
        let path = write_temp(
            "Match user alice\n  Port 4444\n\nHost target\n  HostName target.example.com\n",
        );
        let as_alice = import(&path, "alice").unwrap();
        assert_eq!(as_alice[0].port, 4444);

        let as_bob = import(&path, "bob").unwrap();
        assert_eq!(as_bob[0].port, 22, "Match user alice must not apply when connecting as bob");
    }

    #[test]
    fn match_user_never_contributes_a_host_alias() {
        // A `Match user` block on its own (no `Host` line anywhere) must
        // not conjure up a phantom session profile.
        let path = write_temp("Match user alice\n  Port 4444\n");
        let profiles = import(&path, "alice").unwrap();
        assert!(profiles.is_empty());
    }

    #[test]
    fn match_with_unsupported_secondary_criteria_is_skipped() {
        // "Match user alice host foo" mixes criteria types -- deliberately
        // unsupported (see module doc) since getting AND/OR semantics
        // subtly wrong is worse than not importing it.
        let path = write_temp(
            "Match user alice host foo\n  Port 9999\n\nHost target\n  HostName target.example.com\n",
        );
        let profiles = import(&path, "alice").unwrap();
        assert_eq!(profiles[0].port, 22);
    }

    #[test]
    fn match_user_comma_separated_list() {
        // Real OpenSSH `Match` syntax uses one comma-separated token per
        // criteria, not space-separated like `Host` lines.
        let path = write_temp("Match user alice,bob\n  Port 5555\n\nHost target\n  HostName target.example.com\n");
        assert_eq!(import(&path, "alice").unwrap()[0].port, 5555);
        assert_eq!(import(&path, "bob").unwrap()[0].port, 5555);
        assert_eq!(import(&path, "carol").unwrap()[0].port, 22);
    }

    #[test]
    fn glob_star_and_question_mark() {
        assert!(glob_match("*.example.com", "foo.example.com"));
        assert!(glob_match("*.example.com", "a.b.example.com"));
        assert!(!glob_match("*.example.com", "example.com"));
        assert!(glob_match("host?", "host1"));
        assert!(!glob_match("host?", "host12"));
    }
}
