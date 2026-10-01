//! Maps pi's model catalog to our `ProviderInfo` shape.
//!
//! pi owns the catalog (built-ins + user config + our extension's local
//! rows); we own availability semantics, which stay exactly as today:
//! loopback providers are live-probed, everything else is `available` when
//! listed. `needs_key` is false for loopback and for providers pi already
//! authenticates (its own `auth.json`); true otherwise, which routes the UI
//! to our key modal for the one provider we manage (opencode).
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::OnceLock;

/// pi's built-in model registry, snapshotted into the repo (see
/// scripts/regen-pi-models.mjs + src/native/assets/pi-models.json). The live
/// `get_available_models` RPC only reports authenticated + local providers,
/// so without this the UI could never list — let alone validate — a provider
/// the user hasn't authed yet. The live RPC always wins at runtime; the
/// snapshot only fills the gaps. Regenerate on pi bumps.
static SNAPSHOT: OnceLock<BTreeMap<String, Vec<String>>> = OnceLock::new();

fn snapshot() -> &'static BTreeMap<String, Vec<String>> {
    SNAPSHOT.get_or_init(|| {
        serde_json::from_str::<serde_json::Value>(include_str!("../../assets/pi-models.json"))
            .ok()
            .and_then(|v| v.get("providers").cloned())
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    })
}

/// Model ids pi ships for one provider, from the repo snapshot. Used for
/// listing and select-time validation when the live RPC has nothing
/// (unauthenticated provider); live data wins whenever present.
pub fn known_models(id: &str) -> Option<Vec<String>> {
    snapshot().get(id).cloned()
}

/// Every provider id pi's registry knows, live or not.
pub fn known_ids() -> Vec<String> {
    snapshot().keys().cloned().collect()
}

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

/// Model ids pi lists for one provider: live RPC first, repo snapshot as
/// fallback, None when neither knows the id. Select-time validation for
/// pi-managed ids (turn hot paths never call this — membership was validated
/// at select, and pi fails honestly).
pub async fn models_for(state: &crate::api::AppState, id: &str) -> Option<Vec<String>> {
    if let Ok(sys) = state.pi.child("system").await {
        if let Ok(models) = sys.models(&state.pi).await {
            if let Some(group) = group_by_provider(&models)
                .into_iter()
                .find(|(gid, _)| gid == id)
            {
                return Some(model_ids(&group.1));
            }
        }
    }
    known_models(id)
}

/// Full providers list in the current UI shape, sourced from pi's catalog
/// with local-first availability preserved:
/// - ollama / llama.cpp: today's live probe output verbatim (fresh models,
///   defaults, windows, startable/installed) — pi groups under these ids
///   are skipped to avoid double entries.
/// - everything else pi lists: models + context windows from pi,
///   `available: true` (configured), `needs_key` unless loopback or pi
///   already authenticates it (its own auth.json).
/// - registry ids pi's live RPC omits (it only reports authenticated +
///   local providers): models from the repo snapshot so the UI can list,
///   validate, and take keys for the whole catalog, not just what's authed.
/// On pi failure: degraded to the local probes + snapshot + diagnostics
/// (the endpoint never hard-fails; the UI degrades to offline chips).
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
    let mut live_ids: Vec<String> = Vec::new();
    for (id, group) in group_by_provider(&models) {
        if is_local_provider(&id) {
            continue;
        }
        let base = group
            .first()
            .and_then(|m| m.get("baseUrl"))
            .and_then(|u| u.as_str());
        let loopback = is_loopback_url(base);
        live_ids.push(id.clone());
        out.push(serde_json::json!({
            "id": id,
            "name": id,
            "available": true,
            "models": model_ids(&group),
            "default_model": "",
            "needs_key": !loopback && !pi_auth_has(&id),
            // Paste is about OUR modal: loopback takes no keys at all, and
            // OAuth/subscription ids refuse with a `pi auth` pointer.
            "key_paste": !loopback && !crate::secrets::pi_managed_only(&id),
            "context_window": max_context_window(&group),
            "startable": false,
            "installed": null,
        }));
    }
    append_snapshot_entries(&mut out, &live_ids);
    out
}

/// Registry ids missing from the live output (unauthenticated providers pi
/// doesn't report): same shape, models from the snapshot, no context
/// window (unknown until authed). `needs_key` still honors pi's own auth.
fn append_snapshot_entries(out: &mut Vec<Value>, live_ids: &[String]) {
    for id in known_ids() {
        if live_ids.iter().any(|l| l == &id) || is_local_provider(&id) {
            continue;
        }
        let models = known_models(&id).unwrap_or_default();
        out.push(serde_json::json!({
            "id": id,
            "name": id,
            "available": true,
            "models": models,
            "default_model": "",
            "needs_key": !pi_auth_has(&id),
            "key_paste": !crate::secrets::pi_managed_only(&id),
            "context_window": null,
            "startable": false,
            "installed": null,
        }));
    }
}

async fn fallback() -> Vec<Value> {
    let (ollama, llamacpp, opencode) = tokio::join!(
        crate::ai::routes::probe("ollama"),
        crate::ai::routes::probe("llama.cpp"),
        crate::ai::routes::probe("opencode"),
    );
    let mut out = vec![ollama, llamacpp, opencode];
    let live_ids = ["ollama", "llama.cpp", "opencode"]
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    append_snapshot_entries(&mut out, &live_ids);
    out
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
    fn snapshot_covers_the_registry() {
        let ids = known_ids();
        assert!(ids.len() >= 30, "snapshot shrank: {}", ids.len());
        assert!(ids.contains(&"anthropic".to_string()));
        assert!(ids.contains(&"opencode".to_string()));
        let sorted = {
            let mut s = ids.clone();
            s.sort();
            s
        };
        assert_eq!(ids, sorted, "known_ids must be deterministic");
        let anthropic = known_models("anthropic").expect("snapshot has anthropic");
        assert!(!anthropic.is_empty());
        assert!(known_models("definitely-not-a-provider-xyz").is_none());
    }

    #[test]
    fn snapshot_entries_skip_live_and_local() {
        let mut out = Vec::new();
        append_snapshot_entries(&mut out, &["anthropic".to_string()]);
        let ids: Vec<String> = out
            .iter()
            .filter_map(|v| v.get("id")?.as_str().map(str::to_string))
            .collect();
        // Live-listed anthropic is not duplicated; loopback never appears.
        assert!(!ids.contains(&"anthropic".to_string()));
        assert!(!ids.iter().any(|id| is_local_provider(id)));
        assert!(ids.contains(&"openai".to_string()));
        // Shape matches the live entries the UI already renders.
        for v in &out {
            assert!(v.get("needs_key").and_then(|b| b.as_bool()).is_some());
            assert!(v.get("key_paste").and_then(|b| b.as_bool()).is_some());
            assert!(v.get("models").and_then(|m| m.as_array()).is_some());
            assert_eq!(v.get("available"), Some(&serde_json::Value::Bool(true)));
        }
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
