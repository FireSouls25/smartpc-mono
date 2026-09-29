//! Maps pi's model catalog to our `ProviderInfo` shape.
//!
//! pi owns the catalog (built-ins + user config + our extension's local
//! rows); we own availability semantics, which stay exactly as today:
//! loopback providers are live-probed, everything else is `available` when
//! listed. `needs_key` is false for loopback and for providers pi already
//! authenticates (its own `auth.json`); true otherwise, which routes the UI
//! to our key modal for the one provider we manage (opencode).
use serde_json::Value;

fn home_config_dir() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE").ok().map(std::path::PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var("HOME").ok().map(std::path::PathBuf::from)
    }
}

/// Does the user's own pi config already authenticate this provider?
/// Best-effort presence check (key names only, values never read).
/// `PI_AUTH_FILE` overrides the path (test hook: e2e isolates ambient
/// developer auth so the no-key modal path is deterministic).
pub fn pi_auth_has(provider: &str) -> bool {
    let path = match std::env::var("PI_AUTH_FILE")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(std::path::PathBuf::from)
    {
        Some(p) => p,
        None => match home_config_dir() {
            Some(h) => h.join(".pi").join("agent").join("auth.json"),
            None => return false,
        },
    };
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return false,
    };
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.get(provider).cloned())
        .is_some_and(|v| !(v.is_null() || v == Value::String(String::new())))
}

/// Loopback providers never need keys through us.
pub(crate) fn is_local_provider(id: &str) -> bool {
    matches!(id, "ollama" | "llamacpp" | "llama.cpp" | "llama-cpp")
}

pub fn is_loopback_url(base_url: Option<&str>) -> bool {
    base_url.is_some_and(|u| u.contains("127.0.0.1") || u.contains("localhost"))
}

/// Group pi model objects by their provider id.
pub fn group_by_provider<'a>(models: &'a [Value]) -> Vec<(String, Vec<&'a Value>)> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<String, Vec<&'a Value>> =
        std::collections::HashMap::new();
    for m in models {
        let pid = m
            .get("provider")
            .and_then(|p| p.as_str())
            .unwrap_or("unknown")
            .to_string();
        if !groups.contains_key(&pid) {
            order.push(pid.clone());
        }
        groups.entry(pid).or_default().push(m);
    }
    order
        .into_iter()
        .filter_map(|id| groups.remove(&id).map(|ms| (id, ms)))
        .collect()
}

pub fn model_ids(models: &[&Value]) -> Vec<String> {
    let mut ids: Vec<String> = models
        .iter()
        .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(str::to_string))
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

pub fn max_context_window(models: &[&Value]) -> Option<u32> {
    models
        .iter()
        .filter_map(|m| m.get("contextWindow").and_then(|w| w.as_u64()))
        .map(|w| w as u32)
        .max()
}

/// Full providers list in the current UI shape, sourced from pi's catalog
/// with local-first availability preserved:
/// - ollama / llama.cpp: today's live probe output verbatim (fresh models,
///   defaults, windows, startable/installed) — pi groups under these ids
///   are skipped to avoid double entries.
/// - everything else pi lists: models + context windows from pi,
///   `available: true` (configured), `needs_key` unless loopback or pi
///   already authenticates it (its own auth.json).
/// On pi failure: degraded to the three local probes + diagnostics (the
/// endpoint never hard-fails; the UI degrades to offline chips).
pub async fn catalog(state: &crate::api::AppState) -> Vec<Value> {
    let sys = match state.pi.child("system").await {
        Ok(c) => c,
        Err(e) => {
            crate::diagnostics::push(format!("pi providers: {e:?}, local fallback"));
            return fallback().await;
        }
    };
    let models = match sys.models(&state.pi).await {
        Ok(m) => m,
        Err(e) => {
            crate::diagnostics::push(format!("pi providers: {e:?}, local fallback"));
            return fallback().await;
        }
    };
    let mut out = Vec::new();
    // Local-first behavior bit-identical to the native path.
    let (ollama, llamacpp) =
        tokio::join!(crate::ai::routes::probe("ollama"), crate::ai::routes::probe("llama.cpp"));
    out.push(ollama);
    out.push(llamacpp);
    for (id, group) in group_by_provider(&models) {
        if is_local_provider(&id) {
            continue;
        }
        let base = group
            .first()
            .and_then(|m| m.get("baseUrl"))
            .and_then(|u| u.as_str());
        let loopback = is_loopback_url(base);
        out.push(serde_json::json!({
            "id": id,
            "name": id,
            "available": true,
            "models": model_ids(&group),
            "default_model": "",
            "needs_key": !loopback && !pi_auth_has(&id),
            "context_window": max_context_window(&group),
            "startable": false,
            "installed": null,
        }));
    }
    out
}

async fn fallback() -> Vec<Value> {
    let (ollama, llamacpp, opencode) = tokio::join!(
        crate::ai::routes::probe("ollama"),
        crate::ai::routes::probe("llama.cpp"),
        crate::ai::routes::probe("opencode"),
    );
    vec![ollama, llamacpp, opencode]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model(provider: &str, id: &str, window: Option<u32>) -> Value {
        let mut m = json!({ "id": id, "provider": provider });
        if let Some(w) = window {
            m["contextWindow"] = json!(w);
        }
        m
    }

    #[test]
    fn groups_preserve_first_seen_order() {
        let models = vec![
            model("opencode", "a", None),
            model("ollama", "b", None),
            model("opencode", "c", None),
        ];
        let groups = group_by_provider(&models);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, "opencode");
        assert_eq!(model_ids(&groups[0].1), vec!["a", "c"]);
        assert_eq!(groups[1].0, "ollama");
    }

    #[test]
    fn local_ids_recognized() {
        assert!(is_local_provider("ollama"));
        assert!(is_local_provider("llamacpp"));
        assert!(is_local_provider("llama.cpp"));
        assert!(!is_local_provider("opencode"));
        assert!(!is_local_provider("anthropic"));
        assert!(is_loopback_url(Some("http://127.0.0.1:11434/v1")));
        assert!(is_loopback_url(Some("http://localhost:8080/v1")));
        assert!(!is_loopback_url(Some("https://opencode.ai/zen")));
        assert!(!is_loopback_url(None));
    }

    #[test]
    fn context_takes_max() {
        let models = vec![
            model("x", "a", Some(8000)),
            model("x", "b", Some(200000)),
            model("x", "c", None),
        ];
        let refs: Vec<&Value> = models.iter().collect();
        assert_eq!(max_context_window(&refs), Some(200000));
        let lone = vec![model("x", "c", None)];
        let empty: Vec<&Value> = lone.iter().collect();
        assert_eq!(max_context_window(&empty), None);
    }

    #[test]
    fn missing_auth_file_means_no_key() {
        let prev = std::env::var("HOME").ok();
        std::env::set_var("HOME", "/tmp/opencode-definitely-no-home-xyz");
        assert!(!pi_auth_has("opencode"));
        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn auth_file_override_is_honored() {
        let prev = std::env::var("PI_AUTH_FILE").ok();
        // Unreadable path: no key, regardless of the real home config.
        std::env::set_var("PI_AUTH_FILE", "/tmp/opencode-definitely-no-auth-xyz.json");
        assert!(!pi_auth_has("opencode"));
        // A file naming the provider authenticates it (values never read).
        let dir = std::env::temp_dir().join(format!(
            "smartpc-authtest-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("auth.json");
        std::fs::write(&file, r#"{"opencode": {"type": "api_key"}}"#).unwrap();
        std::env::set_var("PI_AUTH_FILE", &file);
        assert!(pi_auth_has("opencode"));
        assert!(!pi_auth_has("anthropic"));
        match prev {
            Some(v) => std::env::set_var("PI_AUTH_FILE", v),
            None => std::env::remove_var("PI_AUTH_FILE"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
