//! Детекторы цикла (OpenDev): fingerprint вызовов (tool, args).

/// Fingerprint вызова (tool, args) — детектор doom loop (OpenDev шаг 13)
pub(crate) fn fingerprint(name: &str, args: &serde_json::Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut h);
    serde_json::to_string(args).unwrap_or_default().hash(&mut h);
    h.finish()
}
