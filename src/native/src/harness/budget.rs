//! Per-turn + per-session budgets (T4 §4, G4). Checked at the top of
//! `execute()`, before the risk gate: arg rules can only refuse, never
//! escalate past this.
//!
//! The per-turn half resets every turn (`new_turn`); the session half rides
//! along in the same struct so `execute()` keeps its single-budget
//! signature — the pi bridge (`pi::routes`) holds one `TurnBudget` per pi
//! session file and calls `new_turn` when a turn starts.

use super::tools::Risk;

/// Hard per-turn caps.
pub const MAX_TOOL_CALLS_PER_TURN: u32 = 20;
pub const MAX_READONLY_PER_TURN: u32 = 30;
pub const MAX_TYPED_CHARS_PER_TURN: u32 = 1000;
pub const MAX_TYPE_TEXT_CALLS: u32 = 5;
pub const MAX_MOUSE_MOVES_PER_TURN: u32 = 10;
/// Synthetic-input tools faster than this are dropped (INPUT_COOLDOWN_MS).
/// ReadOnly tools and `open_url` are exempt; `press_key` media keys are
/// exempt (T4 Q2 — pausing a song twice in a row is the point).
pub const INPUT_COOLDOWN_MS: u128 = 400;
/// Session caps (T4 §4, safety P0-f): the run aborts via settle/cancel,
/// never via a bare string the model could talk its way around.
pub const MAX_MUTATING_PER_SESSION: u32 = 100;
pub const MAX_TYPED_CHARS_PER_SESSION: u32 = 5000;
/// Consecutive denials in one turn before the turn is aborted.
pub const MAX_DENIALS_BEFORE_ABORT: u32 = 3;

/// Safety P0-c: mouse tools are refused unless `get_display_info` AND a
/// successful `capture_screen` both landed **this turn**. Out-of-range
/// coordinates are refused (fail-closed, never clamped into range).
#[derive(Debug, Clone, Default)]
pub struct SessionBudget {
    pub mutating_calls: u32,
    pub typed_chars: u32,
}

#[derive(Debug, Clone)]
pub struct TurnBudget {
    pub tool_calls: u32,
    pub readonly_calls: u32,
    pub typed_chars: u32,
    pub type_text_calls: u32,
    pub mouse_moves: u32,
    consecutive_denials: u32,
    last_input_ms: Option<u128>,
    /// `POST /v1/ai/run {preview:true}`: Medium/High return a preview
    /// title with no side effect and no Action row.
    pub dry_run: bool,
    /// Turn language for preview titles ("Vista previa: …" / "Preview: …").
    pub lang: String,
    saw_display_info: bool,
    saw_screenshot_ok: bool,
    /// Set when a denial (or session cap) must unwind the turn. The bridge
    /// consumes it via `take_abort` and calls `request_cancel`.
    abort_pending: bool,
    pub session: SessionBudget,
}

impl TurnBudget {
    pub fn new(dry_run: bool, lang: &str) -> Self {
        Self {
            tool_calls: 0,
            readonly_calls: 0,
            typed_chars: 0,
            type_text_calls: 0,
            mouse_moves: 0,
            consecutive_denials: 0,
            last_input_ms: None,
            dry_run,
            lang: lang.to_string(),
            saw_display_info: false,
            saw_screenshot_ok: false,
            abort_pending: false,
            session: SessionBudget::default(),
        }
    }

    /// Reset the per-turn counters for a new turn; session caps and the
    /// abort flag carry over (an aborted session stays aborted).
    pub fn new_turn(&mut self, dry_run: bool, lang: &str) {
        let session = std::mem::take(&mut self.session);
        let abort_pending = self.abort_pending;
        *self = Self::new(dry_run, lang);
        self.session = session;
        self.abort_pending = abort_pending;
    }

    pub fn mark_display_info_ok(&mut self) {
        self.saw_display_info = true;
    }

    pub fn mark_screenshot_ok(&mut self) {
        self.saw_screenshot_ok = true;
    }

    pub fn grounded(&self) -> bool {
        self.saw_display_info && self.saw_screenshot_ok
    }

    /// Consume a pending turn-abort (bridge calls `request_cancel` then).
    pub fn take_abort(&mut self) -> bool {
        std::mem::replace(&mut self.abort_pending, false)
    }

    /// Merge a concurrently-executed sibling back into the map entry.
    /// Counters take the max (every sibling charged from the same base, so
    /// max keeps each charge exactly once in the common sequential case and
    /// under-counts by at most the concurrency degree otherwise); flags OR.
    pub fn absorb(&mut self, other: TurnBudget) {
        self.tool_calls = self.tool_calls.max(other.tool_calls);
        self.readonly_calls = self.readonly_calls.max(other.readonly_calls);
        self.typed_chars = self.typed_chars.max(other.typed_chars);
        self.type_text_calls = self.type_text_calls.max(other.type_text_calls);
        self.mouse_moves = self.mouse_moves.max(other.mouse_moves);
        self.consecutive_denials = self.consecutive_denials.max(other.consecutive_denials);
        self.last_input_ms = self.last_input_ms.max(other.last_input_ms);
        self.saw_display_info |= other.saw_display_info;
        self.saw_screenshot_ok |= other.saw_screenshot_ok;
        self.abort_pending |= other.abort_pending;
        self.session.mutating_calls = self
            .session
            .mutating_calls
            .max(other.session.mutating_calls);
        self.session.typed_chars = self.session.typed_chars.max(other.session.typed_chars);
        // dry_run/lang identify the turn: the map entry (reset at turn
        // start) is authoritative, never the sibling.
    }

    fn deny(&mut self, message: impl Into<String>, abort: bool) -> Deny {
        self.consecutive_denials += 1;
        // Three consecutive denials unwind the turn (T4 §4): the model is
        // looping against a wall, so stop it instead of feeding it errors.
        let abort = abort || self.consecutive_denials >= MAX_DENIALS_BEFORE_ABORT;
        if abort {
            self.abort_pending = true;
        }
        Deny {
            message: message.into(),
            abort,
        }
    }

    fn allow(&mut self) -> Charge {
        self.consecutive_denials = 0;
        Charge::Allow
    }

    /// Top-of-`execute()` gate. `cooldown_exempt` covers ReadOnly tools,
    /// `open_url`, and `press_key` media keys. On success the counters are
    /// already charged; on denial the denial streak grows (3 → abort).
    pub fn check_and_charge(
        &mut self,
        tool: &str,
        risk: Risk,
        args: &serde_json::Value,
        now_ms: u128,
        cooldown_exempt: bool,
    ) -> Result<Charge, Deny> {
        if self.tool_calls + 1 > MAX_TOOL_CALLS_PER_TURN {
            return Err(self.deny(
                format!(
                    "turn tool budget exhausted (max {MAX_TOOL_CALLS_PER_TURN} calls per turn); tell the user and stop"
                ),
                false,
            ));
        }
        if risk == Risk::ReadOnly {
            if self.readonly_calls + 1 > MAX_READONLY_PER_TURN {
                return Err(self.deny(
                    format!(
                        "read-only budget exhausted (max {MAX_READONLY_PER_TURN} per turn); tell the user and stop"
                    ),
                    false,
                ));
            }
            self.tool_calls += 1;
            self.readonly_calls += 1;
            return Ok(self.allow());
        }
        if !cooldown_exempt {
            if let Some(last) = self.last_input_ms {
                if now_ms.saturating_sub(last) < INPUT_COOLDOWN_MS {
                    return Err(self.deny(
                        "input too fast (400 ms cooldown on synthetic input); tell the user and stop",
                        false,
                    ));
                }
            }
        }
        match tool {
            "type_text" => {
                let n = args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .map(|s| s.chars().count() as u32)
                    .unwrap_or(0);
                if self.type_text_calls + 1 > MAX_TYPE_TEXT_CALLS {
                    return Err(self.deny(
                        format!(
                            "type_text budget exhausted (max {MAX_TYPE_TEXT_CALLS} calls per turn); tell the user and stop"
                        ),
                        false,
                    ));
                }
                if self.typed_chars + n > MAX_TYPED_CHARS_PER_TURN {
                    return Err(self.deny(
                        format!(
                            "typing budget exhausted (max {MAX_TYPED_CHARS_PER_TURN} chars per turn); tell the user and stop"
                        ),
                        false,
                    ));
                }
                if self.session.typed_chars + n > MAX_TYPED_CHARS_PER_SESSION {
                    return Err(self.deny(
                        "session typing budget exhausted; the run ends here — tell the user and stop",
                        true,
                    ));
                }
                self.tool_calls += 1;
                self.type_text_calls += 1;
                self.typed_chars += n;
                self.session.typed_chars += n;
                self.session.mutating_calls += 1;
                if self.session.mutating_calls > MAX_MUTATING_PER_SESSION {
                    return Err(self.deny(
                        "session action budget exhausted; the run ends here — tell the user and stop",
                        true,
                    ));
                }
                self.last_input_ms = Some(now_ms);
                Ok(self.preview_or(tool, args))
            }
            "mouse_move" => {
                if self.mouse_moves + 1 > MAX_MOUSE_MOVES_PER_TURN {
                    return Err(self.deny(
                        format!(
                            "mouse budget exhausted (max {MAX_MOUSE_MOVES_PER_TURN} moves per turn); tell the user and stop"
                        ),
                        false,
                    ));
                }
                if !self.grounded() {
                    return Err(self.deny(
                        "need fresh screenshot first (run get_display_info and capture_screen this turn); tell the user and stop",
                        false,
                    ));
                }
                self.tool_calls += 1;
                self.mouse_moves += 1;
                self.session.mutating_calls += 1;
                if self.session.mutating_calls > MAX_MUTATING_PER_SESSION {
                    return Err(self.deny(
                        "session action budget exhausted; the run ends here — tell the user and stop",
                        true,
                    ));
                }
                self.last_input_ms = Some(now_ms);
                Ok(self.preview_or(tool, args))
            }
            "mouse_click" | "mouse_scroll" | "key_combo" | "press_key" => {
                // P0-c: pointer tools need fresh eyes this turn. Keyboard
                // tools don't (typing blind into a focused app is the
                // normal case); they stay cooldown-gated instead.
                if (tool == "mouse_click" || tool == "mouse_scroll") && !self.grounded() {
                    return Err(self.deny(
                        "need fresh screenshot first (run get_display_info and capture_screen this turn); tell the user and stop",
                        false,
                    ));
                }
                self.tool_calls += 1;
                self.session.mutating_calls += 1;
                if self.session.mutating_calls > MAX_MUTATING_PER_SESSION {
                    return Err(self.deny(
                        "session action budget exhausted; the run ends here — tell the user and stop",
                        true,
                    ));
                }
                if !cooldown_exempt {
                    self.last_input_ms = Some(now_ms);
                }
                Ok(self.preview_or(tool, args))
            }
            _ => {
                // open_app/close_app/open_url/capture_screen: charged as a
                // mutating session call when they record actions.
                self.tool_calls += 1;
                if super::tools::catalog()
                    .iter()
                    .find(|t| t.name == tool)
                    .is_some_and(|t| t.records_action)
                {
                    self.session.mutating_calls += 1;
                    if self.session.mutating_calls > MAX_MUTATING_PER_SESSION {
                        return Err(self.deny(
                            "session action budget exhausted; the run ends here — tell the user and stop",
                            true,
                        ));
                    }
                }
                Ok(self.preview_or(tool, args))
            }
        }
    }

    /// Dry-run (G3): Medium/High return a preview title with no side
    /// effect; ReadOnly/Low execute normally.
    fn preview_or(&mut self, tool: &str, args: &serde_json::Value) -> Charge {
        if !self.dry_run {
            return self.allow();
        }
        let risky = super::tools::catalog()
            .iter()
            .find(|t| t.name == tool)
            .is_some_and(|t| matches!(t.risk, Risk::Medium | Risk::High));
        if risky {
            self.consecutive_denials = 0;
            Charge::Preview(super::tools::preview_title(tool, args, &self.lang))
        } else {
            self.allow()
        }
    }
}

/// Outcome of the budget gate: run it, or answer with a preview title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Charge {
    Allow,
    Preview(String),
}

/// A refused call: user-facing copy plus whether the turn must unwind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deny {
    pub message: String,
    pub abort: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn budget() -> TurnBudget {
        TurnBudget::new(false, "es")
    }

    fn now() -> u128 {
        1_000_000
    }

    #[test]
    fn tool_call_cap_is_twenty() {
        let mut b = budget();
        for _ in 0..MAX_TOOL_CALLS_PER_TURN {
            assert!(b
                .check_and_charge("list_processes", Risk::ReadOnly, &json!({}), now(), true)
                .is_ok());
        }
        let d = b
            .check_and_charge("list_processes", Risk::ReadOnly, &json!({}), now(), true)
            .unwrap_err();
        assert!(d.message.contains("budget exhausted"));
    }

    #[test]
    fn readonly_has_its_own_cap_of_thirty() {
        let mut b = budget();
        // ReadOnly cap (30) binds after the general cap (20) — drive the
        // general counter with Low tools instead.
        for _ in 0..MAX_TOOL_CALLS_PER_TURN {
            assert!(b
                .check_and_charge("open_app", Risk::Low, &json!({"name": "x"}), now(), true)
                .is_ok());
        }
        // The 21st call trips the general cap even for ReadOnly.
        assert!(b
            .check_and_charge("list_processes", Risk::ReadOnly, &json!({}), now(), true)
            .is_err());
    }

    #[test]
    fn type_text_budgets_bind() {
        let mut b = budget();
        // 5 calls of 200 chars (staggered past the cooldown): the 6th
        // call trips the call cap.
        for i in 0..MAX_TYPE_TEXT_CALLS {
            let args = json!({"text": "x".repeat(200)});
            assert!(b
                .check_and_charge(
                    "type_text",
                    Risk::High,
                    &args,
                    now() + (i as u128) * 1000,
                    false
                )
                .is_ok());
        }
        // 6th call, well past the cooldown: the CALL cap binds.
        let d = b
            .check_and_charge(
                "type_text",
                Risk::High,
                &json!({"text": "hi"}),
                now() + 20_000,
                false,
            )
            .unwrap_err();
        assert!(d.message.contains("budget exhausted"), "{}", d.message);

        // Char cap: 5×200 = 1000 exactly, so a fresh budget typing 1001
        // chars in one... (200/call schema cap binds first in exec; here
        // the turn cap is what matters).
        let mut b = budget();
        for i in 0..4 {
            let args = json!({"text": "x".repeat(200)});
            assert!(b
                .check_and_charge(
                    "type_text",
                    Risk::High,
                    &args,
                    now() + (i as u128) * 1000,
                    false
                )
                .is_ok());
        }
        // 800 + 201 more would exceed 1000 (well past the cooldown).
        let d = b
            .check_and_charge(
                "type_text",
                Risk::High,
                &json!({"text": "x".repeat(201)}),
                now() + 20_000,
                false,
            )
            .unwrap_err();
        assert!(d.message.contains("typing budget"), "{}", d.message);
    }

    #[test]
    fn session_typing_cap_aborts() {
        let mut b = budget();
        b.session.typed_chars = MAX_TYPED_CHARS_PER_SESSION - 10;
        let d = b
            .check_and_charge(
                "type_text",
                Risk::High,
                &json!({"text": "x".repeat(11)}),
                now(),
                false,
            )
            .unwrap_err();
        assert!(d.abort, "session cap must unwind the turn");
        assert!(b.take_abort());
        assert!(!b.take_abort(), "abort is consume-once");
    }

    #[test]
    fn session_mutating_cap_aborts() {
        let mut b = budget();
        b.session.mutating_calls = MAX_MUTATING_PER_SESSION;
        let d = b
            .check_and_charge("open_app", Risk::Low, &json!({"name": "x"}), now(), true)
            .unwrap_err();
        assert!(d.abort);
    }

    #[test]
    fn three_consecutive_denials_abort() {
        let mut b = budget();
        // Mouse without grounding: denied, but not yet aborting.
        for i in 0..2 {
            let d = b
                .check_and_charge(
                    "mouse_move",
                    Risk::Medium,
                    &json!({"x": 1, "y": 1}),
                    now(),
                    false,
                )
                .unwrap_err();
            assert!(!d.abort, "denial {i} must not abort yet");
        }
        let d = b
            .check_and_charge(
                "mouse_move",
                Risk::Medium,
                &json!({"x": 1, "y": 1}),
                now(),
                false,
            )
            .unwrap_err();
        assert!(d.abort, "3rd consecutive denial unwinds the turn");
        assert!(d.message.contains("fresh screenshot"));
        // Success resets the streak.
        let mut b = budget();
        for _ in 0..2 {
            let _ = b.check_and_charge(
                "mouse_move",
                Risk::Medium,
                &json!({"x": 1, "y": 1}),
                now(),
                false,
            );
        }
        b.mark_display_info_ok();
        b.mark_screenshot_ok();
        assert!(b
            .check_and_charge(
                "mouse_move",
                Risk::Medium,
                &json!({"x": 1, "y": 1}),
                now() + 10_000,
                false
            )
            .is_ok());
        let _ = b.check_and_charge(
            "mouse_move",
            Risk::Medium,
            &json!({"x": 1, "y": 1}),
            now() + 10_000,
            false,
        );
        // Only one consecutive denial: no abort.
        assert!(!b.take_abort());
    }

    #[test]
    fn cooldown_applies_to_synthetic_input_only() {
        let mut b = budget();
        b.mark_display_info_ok();
        b.mark_screenshot_ok();
        assert!(b
            .check_and_charge(
                "mouse_move",
                Risk::Medium,
                &json!({"x": 1, "y": 1}),
                now(),
                false
            )
            .is_ok());
        // Immediate second synthetic input: cooldown.
        let d = b
            .check_and_charge(
                "mouse_move",
                Risk::Medium,
                &json!({"x": 2, "y": 2}),
                now() + 100,
                false,
            )
            .unwrap_err();
        assert!(d.message.contains("too fast"), "{}", d.message);
        // ReadOnly is exempt even back-to-back.
        assert!(b
            .check_and_charge(
                "list_processes",
                Risk::ReadOnly,
                &json!({}),
                now() + 100,
                true
            )
            .is_ok());
        // After the window, input flows again.
        assert!(b
            .check_and_charge(
                "mouse_move",
                Risk::Medium,
                &json!({"x": 2, "y": 2}),
                now() + INPUT_COOLDOWN_MS + 1,
                false
            )
            .is_ok());
    }

    #[test]
    fn mouse_move_cap_is_ten() {
        let mut b = budget();
        b.mark_display_info_ok();
        b.mark_screenshot_ok();
        for i in 0..MAX_MOUSE_MOVES_PER_TURN {
            assert!(b
                .check_and_charge(
                    "mouse_move",
                    Risk::Medium,
                    &json!({"x": 1, "y": 1}),
                    now() + (i as u128) * 1000,
                    false
                )
                .is_ok());
        }
        let d = b
            .check_and_charge(
                "mouse_move",
                Risk::Medium,
                &json!({"x": 1, "y": 1}),
                now() + 99_000,
                false,
            )
            .unwrap_err();
        assert!(d.message.contains("mouse budget"), "{}", d.message);
    }

    #[test]
    fn dry_run_previews_medium_and_high_only() {
        let mut b = TurnBudget::new(true, "es");
        b.mark_display_info_ok();
        b.mark_screenshot_ok();
        match b.check_and_charge(
            "mouse_click",
            Risk::High,
            &json!({"button": "left"}),
            now(),
            false,
        ) {
            Ok(Charge::Preview(t)) => assert!(t.starts_with("Vista previa:"), "{t}"),
            other => panic!("expected preview, got {other:?}"),
        }
        // ReadOnly still executes in dry-run.
        assert_eq!(
            b.check_and_charge("list_processes", Risk::ReadOnly, &json!({}), now(), true),
            Ok(Charge::Allow)
        );
        // English titles in English turns.
        let mut en = TurnBudget::new(true, "en");
        en.mark_display_info_ok();
        en.mark_screenshot_ok();
        match en.check_and_charge(
            "press_key",
            Risk::Medium,
            &json!({"key": "enter"}),
            now(),
            false,
        ) {
            Ok(Charge::Preview(t)) => assert!(t.starts_with("Preview:"), "{t}"),
            other => panic!("expected preview, got {other:?}"),
        }
    }

    #[test]
    fn new_turn_resets_turn_but_keeps_session() {
        let mut b = budget();
        b.session.mutating_calls = 42;
        b.tool_calls = 7;
        b.mark_display_info_ok();
        b.new_turn(false, "en");
        assert_eq!(b.tool_calls, 0);
        assert_eq!(b.session.mutating_calls, 42);
        assert!(
            !b.grounded(),
            "grounding is per-turn (fresh eyes every turn)"
        );
        assert_eq!(b.lang, "en");
    }
}
