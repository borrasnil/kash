//! Session ID generation: animal names, easy to type.
//!
//! Shared by `listen` (reverse shell) and `ssh` (outbound connection).
//! IDs must be unique among currently-active sessions (same scan as `ps`).

use std::sync::LazyLock;

use rand::seq::SliceRandom;

use crate::agent;

/// Popular, easy-to-type animal names used for auto-generated session IDs.
static ANIMAL_NAMES: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    vec![
        "monkey", "tiger", "lion", "bear", "wolf", "fox", "deer", "eagle", "hawk", "crow", "owl",
        "snake", "zebra", "horse", "goat", "sheep", "camel", "moose", "elk", "bison", "rat",
        "rabbit", "otter", "beaver", "badger", "whale", "shark", "dolphin", "turtle", "frog",
        "lizard", "hyena", "leopard", "panda", "koala", "giraffe", "hippo", "crab", "squid",
        "penguin",
    ]
});

/// True if `id` is already assigned to a currently-active session (same scan as `ps`).
pub fn session_id_in_use(id: &str) -> bool {
    agent::list_sessions().iter().any(|s| s.id == id)
}

/// Generate an unused animal-name session ID, with numeric-suffix fallback.
pub fn generate_session_id() -> String {
    let mut rng = rand::thread_rng();
    let mut candidates: Vec<&str> = ANIMAL_NAMES.clone();
    candidates.shuffle(&mut rng);
    if let Some(name) = candidates.into_iter().find(|n| !session_id_in_use(n)) {
        return name.to_string();
    }
    // All animal names are in use — append a numeric suffix.
    let base = ANIMAL_NAMES.first().copied().unwrap_or("session");
    (2..)
        .map(|i| format!("{base}{i}"))
        .find(|candidate| !session_id_in_use(candidate))
        .unwrap_or_else(|| format!("{base}-{}", crate::util::generate_nonce()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_is_an_animal() {
        let id = generate_session_id();
        let base = id.trim_end_matches(|c: char| c.is_ascii_digit());
        assert!(ANIMAL_NAMES.contains(&base), "unexpected session id: {id}");
        assert!(!session_id_in_use(&id));
    }

    #[test]
    fn session_id_in_use_detects_active_session() {
        assert!(!session_id_in_use("__test_in_use__"));
        std::fs::write(agent::socket_path("__test_in_use__"), b"").unwrap();
        assert!(session_id_in_use("__test_in_use__"));
        let _ = std::fs::remove_file(agent::socket_path("__test_in_use__"));
    }
}
