//! Tests for `KnownHostsStore`: the New/Matches/Mismatch verdict logic and
//! on-disk persistence that back SSH host key verification.
//!
//! These tests exercise the store directly with synthetic ED25519 keys
//! (no real network/SSH server needed) so they run in CI without any
//! external dependency.

use hyperterm::ssh_engine::known_hosts::{KnownHostsStore, Verdict};
use russh::keys::key::{KeyPair, PublicKey};

fn temp_path(name: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("hyperterm-knownhosts-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(format!("{name}.known_hosts"))
}

fn synthetic_key(seed: u8) -> PublicKey {
    // Deterministic ED25519 key derived from a seed byte, purely for test
    // fixtures -- never used for real authentication.
    let mut bytes = [0u8; 32];
    bytes[0] = seed;
    let signing = ed25519_dalek::SigningKey::from_bytes(&bytes);
    let key_pair = KeyPair::Ed25519(signing);
    key_pair
        .clone_public_key()
        .expect("derive public key from synthetic keypair")
}

#[test]
fn unknown_host_is_new() {
    let path = temp_path("new-host");
    let _ = std::fs::remove_file(&path);
    let store = KnownHostsStore::load(&path).unwrap();
    let key = synthetic_key(1);
    assert!(matches!(store.check("example.com:22", &key), Verdict::New));
}

#[test]
fn trusted_host_matches_on_next_check() {
    let path = temp_path("trust-then-match");
    let _ = std::fs::remove_file(&path);
    let mut store = KnownHostsStore::load(&path).unwrap();
    let key = synthetic_key(2);

    store.trust("example.com:22", &key).unwrap();

    assert!(matches!(
        store.check("example.com:22", &key),
        Verdict::Matches
    ));
}

#[test]
fn different_host_key_is_mismatch_not_new() {
    let path = temp_path("mismatch");
    let _ = std::fs::remove_file(&path);
    let mut store = KnownHostsStore::load(&path).unwrap();
    let original = synthetic_key(3);
    let attacker_key = synthetic_key(4);

    store.trust("example.com:22", &original).unwrap();

    match store.check("example.com:22", &attacker_key) {
        Verdict::Mismatch { .. } => {}
        Verdict::New => panic!("expected Mismatch, got New"),
        Verdict::Matches => panic!("expected Mismatch, got Matches"),
    }
}

#[test]
fn persists_across_reload() {
    let path = temp_path("persist-reload");
    let _ = std::fs::remove_file(&path);
    let key = synthetic_key(5);
    {
        let mut store = KnownHostsStore::load(&path).unwrap();
        store.trust("persisted.example:2222", &key).unwrap();
    }
    // Reopen fresh, as HyperTerm would on the next launch.
    let reopened = KnownHostsStore::load(&path).unwrap();
    assert!(matches!(
        reopened.check("persisted.example:2222", &key),
        Verdict::Matches
    ));
}

#[test]
fn different_hosts_are_independent() {
    let path = temp_path("independent-hosts");
    let _ = std::fs::remove_file(&path);
    let mut store = KnownHostsStore::load(&path).unwrap();
    let key_a = synthetic_key(6);
    let key_b = synthetic_key(7);

    store.trust("host-a.example:22", &key_a).unwrap();

    // host-b was never trusted, so even though *a* key exists for host-a,
    // host-b should still be reported as New, not accidentally matched.
    assert!(matches!(
        store.check("host-b.example:22", &key_b),
        Verdict::New
    ));
    assert!(matches!(
        store.check("host-a.example:22", &key_a),
        Verdict::Matches
    ));
}

#[test]
fn forget_removes_only_the_targeted_host() {
    let path = temp_path("forget");
    let _ = std::fs::remove_file(&path);
    let mut store = KnownHostsStore::load(&path).unwrap();
    let key_a = synthetic_key(8);
    let key_b = synthetic_key(9);
    store.trust("keep-me.example:22", &key_a).unwrap();
    store.trust("forget-me.example:22", &key_b).unwrap();

    let removed = store.forget("forget-me.example:22").unwrap();
    assert!(removed);

    assert!(matches!(
        store.check("forget-me.example:22", &key_b),
        Verdict::New
    ));
    assert!(matches!(
        store.check("keep-me.example:22", &key_a),
        Verdict::Matches
    ));

    // Survives reload -- forget() must persist, not just mutate in-memory.
    let reopened = KnownHostsStore::load(&path).unwrap();
    assert!(matches!(
        reopened.check("forget-me.example:22", &key_b),
        Verdict::New
    ));
    assert!(matches!(
        reopened.check("keep-me.example:22", &key_a),
        Verdict::Matches
    ));
}

#[test]
fn forget_unknown_host_returns_false() {
    let path = temp_path("forget-unknown");
    let _ = std::fs::remove_file(&path);
    let mut store = KnownHostsStore::load(&path).unwrap();
    let removed = store.forget("never-trusted.example:22").unwrap();
    assert!(!removed);
}
