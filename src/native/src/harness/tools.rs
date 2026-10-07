//! Tool catalog: the ONLY actions the model may request.
//! Each tool carries its JSON schema (sent to tool-capable models) and a
//! [`Risk`] the executor enforces. Adding a tool = one entry here + one
//! match arm in exec — the model can never reach past this list.
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    /// Pure reads: always allowed.
    ReadOnly,
    /// Reversible, everyday actions (open an app, media keys).
    Low,
    /// Visible side effects (key presses outside the allowlist scope).
    Medium,
    /// Hard to undo or sensitive (typing text): policy-gated.
    High,
}

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
    pub risk: Risk,
    /// Whether a successful call becomes an Action row (left pane).
    /// Read-only tools stay in the run trace only.
    pub records_action: bool,
}

fn schema(props: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": props,
        "required": required,
        "additionalProperties": false,
    })
}

pub fn catalog() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "get_system_context",
            description: "Re-read the machine context: OS and version, CPU, memory, session, focused app, input capabilities. Use for hardware and system questions (processor, memory, OS, focused app) and when the situation may have changed since the run started.",
            parameters: schema(
                json!({ "refresh": { "type": "boolean", "description": "Force a full re-probe (default false: 5 s cache)" } }),
                &[],
            ),
            risk: Risk::ReadOnly,
            records_action: false,
        },
        ToolDef {
            name: "list_processes",
            description: "List running processes (pid, name, executable) to find an app or check state. Read-only.",
            parameters: schema(
                json!({ "limit": { "type": "integer", "minimum": 1, "maximum": 50, "description": "Max entries (default 30)" } }),
                &[],
            ),
            risk: Risk::ReadOnly,
            records_action: false,
        },
        ToolDef {
            name: "open_app",
            description: "Launch an application by name (e.g. firefox, code, Calculator). No arguments, no URLs, no shell — just the app name. Friendly names (terminal, browser, editor, files, calculator) resolve automatically; the result names the exact binary started.",
            parameters: schema(
                json!({ "name": { "type": "string", "description": "Application name or binary (no paths, no flags)" } }),
                &["name"],
            ),
            risk: Risk::Low,
            records_action: true,
        },
        ToolDef {
            name: "press_key",
            description: "Press one safe key (media controls, navigation, F5/Escape/Tab/Enter/Space). For anything else, explain why and stop.",
            parameters: schema(
                json!({ "key": { "type": "string", "enum": [
                    "play_pause", "next", "prev", "mute",
                    "volume_up", "volume_down",
                    "escape", "tab", "enter", "space",
                    "left", "right", "up", "down", "f5",
                ] } }),
                &["key"],
            ),
            risk: Risk::Medium,
            records_action: true,
        },
        ToolDef {
            name: "close_app",
            description: "Close a running application by name: matches process or executable names, friendly kinds like browser or terminal work too. May lose unsaved work — say so. Refuses its own backend process. Use list_processes first if unsure of the exact name.",
            parameters: schema(
                json!({
                    "name": { "type": "string", "description": "App/process name (no paths, no flags)" },
                    "force": { "type": "boolean", "description": "SIGKILL instead of graceful terminate when it won't die (default false)" },
                }),
                &["name"],
            ),
            risk: Risk::Medium,
            records_action: true,
        },
        ToolDef {
            name: "open_url",
            description: "Open a public web URL in the default browser (or an allowlisted browser: firefox, google-chrome, chromium, microsoft-edge, safari, librewolf). Only absolute http(s) URLs with public hosts: loopback/private/link-local/*.local/metadata hosts, credentials in the URL, and non-http(s) schemes are refused. http: links always need confirmation and render as not secure. The query string never appears in logs, titles or exports (scheme+host+path only).",
            parameters: schema(
                json!({
                    "url": { "type": "string", "maxLength": 2048, "description": "Absolute http(s) URL to open (max 2048 chars)" },
                    "browser": { "type": "string", "description": "Optional allowlisted browser (default: system browser)" },
                }),
                &["url"],
            ),
            risk: Risk::Medium,
            records_action: true,
        },
        ToolDef {
            name: "type_text",
            description: "Type short text into the focused app (max 200 chars per call, max 5 calls and 1000 chars per turn). Avoid password, login or secret fields: the backend may refuse when the focused window suggests secrets. Often disabled by policy — honor the error and tell the user.",
            parameters: schema(
                json!({ "text": { "type": "string", "maxLength": 200 } }),
                &["text"],
            ),
            risk: Risk::High,
            records_action: true,
        },
        ToolDef {
            name: "mouse_move",
            description: "Move the pointer to physical-pixel coordinates from get_display_info. Refused without a fresh screenshot this turn (run get_display_info and capture_screen first); out-of-range coordinates are refused, never clamped. Max 10 moves per turn.",
            parameters: schema(
                json!({
                    "x": { "type": "integer", "minimum": 0, "description": "Physical-pixel x (see get_display_info geometry)" },
                    "y": { "type": "integer", "minimum": 0, "description": "Physical-pixel y (see get_display_info geometry)" },
                }),
                &["x", "y"],
            ),
            risk: Risk::Medium,
            records_action: true,
        },
        ToolDef {
            name: "mouse_click",
            description: "Click a mouse button at the current pointer position (move with mouse_move first). High risk: needs risky consent. Refused without a fresh screenshot this turn.",
            parameters: schema(
                json!({
                    "button": { "type": "string", "enum": ["left", "right", "middle"], "description": "Button (default left)" },
                    "count": { "type": "integer", "minimum": 1, "maximum": 2, "description": "Clicks (default 1, max 2 for double-click)" },
                }),
                &[],
            ),
            risk: Risk::High,
            records_action: true,
        },
        ToolDef {
            name: "mouse_scroll",
            description: "Scroll the wheel under the pointer: positive delta scrolls up, negative down (-10..10). Refused without a fresh screenshot this turn.",
            parameters: schema(
                json!({
                    "delta": { "type": "integer", "minimum": -10, "maximum": 10, "description": "Wheel steps, -10..10 (default 3)" },
                }),
                &[],
            ),
            risk: Risk::Medium,
            records_action: true,
        },
        ToolDef {
            name: "key_combo",
            description: "Press one editing shortcut: copy, paste, cut, undo, redo (= Ctrl+Shift+Z), save, select_all or find. Control on Linux/Windows, Command on macOS. Careful: select_all followed by cut or type_text wipes the selection — confirm destructive compositions with the user first. Needs confirmation until the renderer surface lands.",
            parameters: schema(
                json!({ "combo": { "type": "string", "enum": ["copy", "paste", "cut", "undo", "redo", "save", "select_all", "find"] } }),
                &["combo"],
            ),
            risk: Risk::Medium,
            records_action: true,
        },
        ToolDef {
            name: "get_display_info",
            description: "Report connected displays: id, name, geometry in physical pixels, scale, refresh rate, primary flag, and the focused display. Empty list with a note when headless. Read-only.",
            parameters: schema(
                json!({ "refresh": { "type": "boolean", "description": "Re-probe displays (default false)" } }),
                &[],
            ),
            risk: Risk::ReadOnly,
            records_action: false,
        },
        ToolDef {
            name: "capture_screen",
            description: "Captura de pantalla: capture the pixels visible on screen (may include secrets) and send them to the model. Requires risky consent (HARNESS_ALLOW_RISKY=1). Refused when the screen is locked, the lock state is unknown, or the focused window title suggests secrets. Reference + metadata only: bytes never enter tool results, logs or exports; shots live in memory only (cap 5, 60 s TTL, consume-once).",
            parameters: schema(
                json!({
                    "display": { "type": "integer", "minimum": 0, "description": "Display id (default: primary)" },
                    "region": {
                        "type": "object",
                        "properties": {
                            "x": { "type": "integer", "minimum": 0 },
                            "y": { "type": "integer", "minimum": 0 },
                            "width": { "type": "integer", "minimum": 1 },
                            "height": { "type": "integer", "minimum": 1 },
                        },
                        "required": ["x", "y", "width", "height"],
                        "additionalProperties": false,
                    },
                    "max_width": { "type": "integer", "minimum": 320, "maximum": 1280, "description": "Downscale cap in px, never upscaled (default 960)" },
                    "format": { "type": "string", "enum": ["png"] },
                    "refresh": { "type": "boolean", "description": "Re-probe displays before capture (default false)" },
                }),
                &[],
            ),
            risk: Risk::High,
            records_action: true,
        },
    ]
}

/// OpenAI function-tool array, sent verbatim to tool-capable providers.
pub fn openai_schemas() -> Vec<Value> {
    catalog()
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            })
        })
        .collect()
}

/// One executed tool call, persisted as a `tool` message and echoed to the
/// UI trace. Produced by the pi turn mapper (tools execute in Rust via the
/// bridge; pi only reasons).
#[derive(Debug, Clone, serde::Serialize)]
pub struct TraceStep {
    pub tool: String,
    pub args: serde_json::Value,
    pub action_id: Option<String>,
    pub ok: bool,
    pub output_preview: String,
}

/// Fixed browser allowlist for open_url (T2 §2): `None` means the system
/// default; anything else must match this list (case-insensitive) before
/// any PATH/OS resolution runs.
pub(crate) const URL_BROWSERS: &[&str] = &[
    "firefox",
    "google-chrome",
    "chromium",
    "microsoft-edge",
    "safari",
    "librewolf",
];

/// A validated public http(s) URL. Query, fragment and userinfo are
/// stripped at parse time, so they can never leak into logs, titles or
/// exports (T2 redaction contract: scheme+host+path ONLY).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedUrl {
    pub scheme: String,
    pub host: String,
    pub port: Option<u16>,
    pub path: String,
}

impl ParsedUrl {
    fn host_display(&self) -> String {
        match self.port {
            Some(p) => format!("{}:{p}", self.host),
            None => self.host.clone(),
        }
    }
}

/// Redacted URL form: scheme+host+path ONLY. The query string is dropped
/// everywhere (logs, titles, exports, summaries) — never truncated.
pub(crate) fn redact_url(u: &ParsedUrl) -> String {
    format!("{}://{}{}", u.scheme, u.host_display(), u.path)
}

fn parse_port(raw: &str) -> Result<u16, String> {
    raw.parse::<u16>().map_err(|_| "URL port is invalid".to_string())
}

fn parse_ipv4(host: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut n = 0usize;
    for part in host.split('.') {
        if n == 4 || part.is_empty() {
            return None;
        }
        out[n] = part.parse::<u8>().ok()?;
        // Reject leading-zero octets ("010" may mean octal elsewhere).
        if part.len() > 1 && part.starts_with('0') {
            return None;
        }
        n += 1;
    }
    if n == 4 { Some(out) } else { None }
}

/// Loopback, RFC1918, link-local (incl. the cloud metadata IP
/// 169.254.169.254), this-network and broadcast.
fn ipv4_non_global(b: [u8; 4]) -> bool {
    b[0] == 0
        || b[0] == 10
        || (b[0] == 172 && (16..32).contains(&b[1]))
        || (b[0] == 192 && b[1] == 168)
        || b[0] == 127
        || (b[0] == 169 && b[1] == 254)
        || b == [255, 255, 255, 255]
}

/// v6 loopback/unspecified, unique-local (fc00::/7, the v6 RFC1918) and
/// link-local (fe80::/10); ::ffff:0:0/96 maps onto the v4 judgement.
/// Any other literal is treated as global (pass).
fn ipv6_non_global(h: &str) -> bool {
    if !h.contains(':') {
        return false;
    }
    let l = h.to_lowercase();
    if l == "::" || l == "::1" {
        return true;
    }
    if let Some(mapped) = l.strip_prefix("::ffff:") {
        return match parse_ipv4(mapped) {
            Some(b) => ipv4_non_global(b),
            None => true,
        };
    }
    match l
        .split(':')
        .next()
        .unwrap_or("")
        .parse::<u16>()
    {
        // from_str_radix(16) would accept the head; decimal parse fails
        // on a-f heads, so hex heads fall through to global (pass).
        Ok(_) => false,
        Err(_) => {
            let head = l.split(':').next().unwrap_or("");
            match u16::from_str_radix(head, 16) {
                Ok(g) => g & 0xffc0 == 0xfe80 || g & 0xfe00 == 0xfc00,
                // Unparseable head inside brackets: fail closed.
                Err(_) => true,
            }
        }
    }
}

fn host_is_non_global(host: &str) -> bool {
    if host == "localhost"
        || host.ends_with(".localhost")
        || host == "local"
        || host.ends_with(".local")
    {
        return true;
    }
    if let Some(b) = parse_ipv4(host) {
        return ipv4_non_global(b);
    }
    // Hex IP spellings (0x7f.0.0.1, 0x7f000001) are never valid DNS names.
    if host.contains("0x") {
        return true;
    }
    // All-numeric dotted forms that are NOT valid IPv4 (leading-zero
    // octets, out-of-range parts: classic octal/overflow tricks) can never
    // be public DNS names (numeric TLDs don't exist) — fail closed.
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() > 1
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    {
        return true;
    }
    // Decimal integer form (e.g. http://2130706433/ == 127.0.0.1): decode
    // and judge the octets. (Hex/octal spellings stay refused as odd hosts
    // only when they hit the rules above; residual bypass noted in tests.)
    if !host.is_empty() && host.chars().all(|c| c.is_ascii_digit()) {
        if let Ok(n) = host.parse::<u32>() {
            return ipv4_non_global(n.to_be_bytes());
        }
    }
    ipv6_non_global(host)
}

/// Strict public-URL validation (T2 §2). Order: trim → ≤2048 → absolute
/// URI with authority (no bare words) → scheme exactly http/https
/// (scheme-specific refusals) → no controls/whitespace/backslashes →
/// userinfo and non-global-host refusal. Never auto-prepends a scheme.
pub(crate) fn parse_public_url(raw: &str) -> Result<ParsedUrl, String> {
    let url = raw.trim();
    if url.is_empty() {
        return Err("URL is required (absolute http(s) URL with a host)".to_string());
    }
    if url.len() > 2048 {
        return Err("URL too long (max 2048 chars)".to_string());
    }
    if url.chars().any(|c| c.is_control() || c.is_whitespace() || c == '\\') {
        return Err("URL contains whitespace, controls or backslashes — refused".to_string());
    }
    let colon = url
        .find(':')
        .ok_or_else(|| "URL must be an absolute http(s) URL with a host (bare words refused)".to_string())?;
    let scheme = url[..colon].to_lowercase();
    if scheme != "http" && scheme != "https" {
        // Scheme-specific refusal (contract: javascript:alert(1) →
        // ok:false containing "refused javascript:").
        return Err(format!("refused {scheme}: only absolute http(s) URLs can be opened"));
    }
    let rest = &url[colon + 1..];
    if !rest.starts_with("//") {
        return Err("URL must be an absolute http(s) URL with a host (bare words refused)".to_string());
    }
    let after = &rest[2..];
    let auth_end = after
        .find(|c| c == '/' || c == '?' || c == '#')
        .unwrap_or(after.len());
    let authority = &after[..auth_end];
    let path_part = &after[auth_end..];
    if authority.is_empty() {
        return Err("URL must be an absolute http(s) URL with a host".to_string());
    }
    if authority.contains('@') {
        return Err("refused local/private URL (credentials in URLs are never allowed)".to_string());
    }
    let (host_raw, port) = if authority.starts_with('[') {
        let close = authority
            .find(']')
            .ok_or_else(|| "URL must be an absolute http(s) URL with a host".to_string())?;
        let rest = &authority[close + 1..];
        let port = match rest.strip_prefix(':') {
            Some(p) => Some(parse_port(p)?),
            None if rest.is_empty() => None,
            _ => return Err("URL must be an absolute http(s) URL with a host".to_string()),
        };
        (authority[1..close].to_string(), port)
    } else {
        match authority.rfind(':') {
            Some(i) if !authority[..i].contains(':') => {
                let port = parse_port(&authority[i + 1..])?;
                (authority[..i].to_string(), Some(port))
            }
            Some(_) => {
                return Err("IPv6 literals must be bracketed (e.g. http://[::1]/) — refused".to_string());
            }
            None => (authority.to_string(), None),
        }
    };
    let host = host_raw.trim_end_matches('.').to_lowercase();
    if host.is_empty() {
        return Err("URL must be an absolute http(s) URL with a host".to_string());
    }
    if host_is_non_global(&host) {
        return Err("refused local/private URL (loopback, private, link-local, .local, metadata or localhost)".to_string());
    }
    // Path up to ? or #: query/fragment dropped = the redaction contract.
    let path_end = path_part
        .find(|c| c == '?' || c == '#')
        .unwrap_or(path_part.len());
    let path = path_part[..path_end].to_string();
    Ok(ParsedUrl {
        scheme,
        host,
        port,
        path,
    })
}

/// Chat-visible title: redacted host+path (≤60 chars, query stripped);
/// http: carries the not-secure marker (T2 P1).
pub(crate) fn short_link(args: &Value) -> String {
    let raw = args.get("url").and_then(|v| v.as_str()).unwrap_or("");
    match parse_public_url(raw) {
        Ok(p) => {
            let shown: String = format!("{}{}", p.host_display(), p.path)
                .chars()
                .take(60)
                .collect();
            if p.scheme == "http" {
                format!("Abrir enlace no seguro {shown}")
            } else {
                format!("Abrir enlace {shown}")
            }
        }
        Err(_) => "Abrir enlace".to_string(),
    }
}

/// The 8 editing shortcuts `key_combo` accepts (T4 §4). `redo` is
/// Ctrl+Shift+Z everywhere (Command+Shift+Z on macOS via the Meta swap).
pub(crate) const KEY_COMBOS: &[&str] = &[
    "copy", "paste", "cut", "undo", "redo", "save", "select_all", "find",
];

/// Dry-run (G3) preview title: what the call WOULD do, with no side
/// effect and no Action row. The EventsFeed renders this verbatim, so the
/// language prefix lives here (turn budget carries the turn lang).
pub(crate) fn preview_title(tool: &str, args: &serde_json::Value, lang: &str) -> String {
    let prefix = if lang.starts_with("es") {
        "Vista previa"
    } else {
        "Preview"
    };
    format!("{}: {}", prefix, title_for(tool, args))
}

/// Human title for an Action row, per tool.
pub(crate) fn title_for(tool: &str, args: &serde_json::Value) -> String {
    let arg = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("?");
    match tool {
        "open_app" => format!("Abrir {}", arg("name")),
        "mouse_move" => {
            let x = args.get("x").and_then(|v| v.as_u64()).map(|v| v.to_string()).unwrap_or("?".into());
            let y = args.get("y").and_then(|v| v.as_u64()).map(|v| v.to_string()).unwrap_or("?".into());
            format!("Mover puntero ({x}, {y})")
        }
        "mouse_click" => format!("Clic {}", arg("button")),
        "mouse_scroll" => "Desplazar".to_string(),
        "key_combo" => format!("Combinación {}", arg("combo")),
        "open_url" => short_link(args),
        "close_app" => format!("Cerrar {}", arg("name")),
        "press_key" => format!("Pulsar {}", arg("key")),
        "type_text" => "Escribir texto".to_string(),
        "get_display_info" => "Consultar pantallas".to_string(),
        // Size-only audit row: no region/display args in the title.
        "capture_screen" => "Captura de pantalla".to_string(),
        _ => tool.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_are_well_formed() {
        let schemas = openai_schemas();
        assert_eq!(schemas.len(), catalog().len());
        for s in &schemas {
            assert_eq!(s["type"], "function");
            assert!(s["function"]["name"].is_string());
            assert_eq!(s["function"]["parameters"]["type"], "object");
        }
    }

    #[test]
    fn grounding_tools_carry_their_risk() {
        // Contains-key per tool (never totals — T4 may add more).
        let cat = catalog();
        let get = |n: &str| cat.iter().find(|t| t.name == n).unwrap_or_else(|| panic!("{n} missing"));
        let info = get("get_display_info");
        assert_eq!(info.risk, Risk::ReadOnly);
        assert!(!info.records_action);
        let shot = get("capture_screen");
        assert_eq!(shot.risk, Risk::High);
        assert!(shot.records_action);
        assert!(shot.description.contains("may include secrets"));
        assert_eq!(title_for("get_display_info", &serde_json::json!({})), "Consultar pantallas");
        // Audit row is size-only: no region/display leaks into the title.
        let t = title_for("capture_screen", &serde_json::json!({"display": 0, "region": {"x": 1}}));
        assert_eq!(t, "Captura de pantalla");
    }

    #[test]
    fn t4_control_tools_carry_their_risk() {
        // Contains-key per tool (never totals).
        let cat = catalog();
        let get = |n: &str| cat.iter().find(|t| t.name == n).unwrap_or_else(|| panic!("{n} missing"));
        let mv = get("mouse_move");
        assert_eq!(mv.risk, Risk::Medium);
        assert!(mv.records_action);
        assert_eq!(mv.parameters["required"], serde_json::json!(["x", "y"]));
        let click = get("mouse_click");
        assert_eq!(click.risk, Risk::High);
        assert!(click.records_action);
        assert!(click.parameters["properties"]["button"]["enum"]
            .as_array()
            .is_some_and(|e| e.len() == 3));
        assert_eq!(click.parameters["properties"]["count"]["maximum"], 2);
        let scroll = get("mouse_scroll");
        assert_eq!(scroll.risk, Risk::Medium);
        assert!(scroll.records_action);
        assert_eq!(scroll.parameters["properties"]["delta"]["minimum"], -10);
        assert_eq!(scroll.parameters["properties"]["delta"]["maximum"], 10);
        let combo = get("key_combo");
        assert_eq!(combo.risk, Risk::Medium);
        assert!(combo.records_action);
        let kinds = combo.parameters["properties"]["combo"]["enum"].clone();
        for k in KEY_COMBOS {
            assert!(kinds.as_array().unwrap().iter().any(|v| v == k), "{k} missing");
        }
        assert!(combo.description.contains("wipes"));
        // type_text is honestly budgeted: 200/call, no NEVER claim.
        let tt = get("type_text");
        assert_eq!(tt.parameters["properties"]["text"]["maxLength"], 200);
        assert!(!tt.description.contains("NEVER"));
        // Titles + preview prefixes.
        assert_eq!(
            title_for("mouse_move", &serde_json::json!({"x": 10, "y": 20})),
            "Mover puntero (10, 20)"
        );
        assert_eq!(
            title_for("key_combo", &serde_json::json!({"combo": "copy"})),
            "Combinación copy"
        );
        assert!(preview_title("mouse_click", &serde_json::json!({}), "es")
            .starts_with("Vista previa:"));
        assert!(preview_title("mouse_click", &serde_json::json!({}), "en")
            .starts_with("Preview:"));
    }

    #[test]
    fn open_url_is_medium_and_gated() {
        let cat = catalog();
        let get = |n: &str| cat.iter().find(|t| t.name == n).unwrap_or_else(|| panic!("{n} missing"));
        let link = get("open_url");
        // Medium, NOT Low (safety P0-a): routed through requires_confirmation.
        assert_eq!(link.risk, Risk::Medium);
        assert!(link.records_action);
        assert_eq!(link.parameters["required"], serde_json::json!(["url"]));
        assert_eq!(link.parameters["additionalProperties"], false);
        assert_eq!(link.parameters["properties"]["url"]["maxLength"], 2048);
        assert!(link.parameters["properties"].get("browser").is_some());
    }

    #[test]
    fn url_redaction_drops_query_everywhere() {
        let p = parse_public_url("https://example.com/a/b?token=secret#frag").unwrap();
        assert_eq!(p.scheme, "https");
        assert_eq!(redact_url(&p), "https://example.com/a/b");
        // Title shows host+path only: no query, no fragment.
        let t = title_for(
            "open_url",
            &serde_json::json!({"url": "https://example.com/a/b?token=secret#frag"}),
        );
        assert_eq!(t, "Abrir enlace example.com/a/b");
        assert!(!t.contains("token"));
        // http: always-confirm + not-secure marker in the title.
        let h = title_for("open_url", &serde_json::json!({"url": "http://example.com/"}));
        assert!(h.contains("no seguro"), "http title must warn: {h}");
        assert!(!h.contains('?'));
        // Long host+path caps at 60 chars.
        let long = format!("https://example.com/{}", "p".repeat(100));
        let t = title_for("open_url", &serde_json::json!({"url": long}));
        let shown = t.strip_prefix("Abrir enlace ").unwrap();
        assert_eq!(shown.chars().count(), 60);
    }

    #[test]
    fn url_parser_refuses_by_scheme_and_host() {
        // Strict scheme reject (T2 contract wording).
        let e = parse_public_url("javascript:alert(1)").unwrap_err();
        assert!(e.contains("refused javascript:"), "{e}");
        for bad in ["data:text/html,hi", "file:///etc/passwd", "ftp://example.com/x"] {
            assert!(parse_public_url(bad).unwrap_err().starts_with("refused "), "{bad}");
        }
        // Bare words: no auto-prepend, strict reject-and-tell.
        for bad in ["example.com", "/relative/path", "https://", "   "] {
            assert!(parse_public_url(bad).is_err(), "{bad:?} should be refused");
        }
        // Non-global hosts.
        for bad in [
            "http://localhost/",
            "https://127.0.0.1/",
            "https://10.0.0.5/",
            "https://192.168.1.1/",
            "https://172.16.9.9/",
            "https://169.254.169.254/latest/",
            "https://printer.local/",
            "https://[::1]/",
            "https://user:pass@example.com/",
        ] {
            let e = parse_public_url(bad).unwrap_err();
            assert!(e.starts_with("refused local/private URL"), "{bad}: {e}");
        }
        // Controls / whitespace / backslashes never launch.
        assert!(parse_public_url("https://example.com/a b").is_err());
        assert!(parse_public_url("https://example.com\\evil").is_err());
        // Overlong.
        assert!(parse_public_url(&format!("https://example.com/{}", "x".repeat(2048))).is_err());
        // Numeric evasions fail closed (octal tricks, out-of-range parts).
        for bad in ["https://010.0.0.1/", "https://999.1.1.1/", "https://0x7f.0.0.1/"] {
            assert!(parse_public_url(bad).is_err(), "{bad} should be refused");
        }
        // Public hosts pass (port preserved, query parsed away).
        let p = parse_public_url("https://example.com:8443/a?x=1").unwrap();
        assert_eq!(redact_url(&p), "https://example.com:8443/a");
    }
}
