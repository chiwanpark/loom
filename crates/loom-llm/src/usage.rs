//! Subscription usage limits (the rolling 5-hour and weekly windows, model
//! scoped caps, and credit balances) that Claude Pro/Max and ChatGPT plans
//! enforce on top of the context window.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Usage is polled in the background, so a slow endpoint gives up early.
pub(crate) const USAGE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageLimits {
    /// Headline limits first, in the order the provider reports them.
    pub limits: Vec<UsageLimit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageLimit {
    /// Short name such as "5h", "weekly", "weekly (Opus)", or "extra usage".
    pub label: String,
    /// Share of the limit used, 0 to 100; `None` when the limit is uncapped.
    pub used_percent: Option<u8>,
    /// Amounts for credit limits, e.g. "$12.34 / $100.00".
    pub detail: Option<String>,
    /// When the window resets, in seconds since the Unix epoch.
    pub resets_at: Option<u64>,
    /// Shown in compact views such as a status bar; the rest are details.
    pub headline: bool,
}

impl UsageLimit {
    fn new(label: impl Into<String>, used_percent: Option<u8>, headline: bool) -> Self {
        Self {
            label: label.into(),
            used_percent,
            detail: None,
            resets_at: None,
            headline,
        }
    }

    fn resets_at(mut self, at: Option<u64>) -> Self {
        self.resets_at = at;
        self
    }

    fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

fn percent(value: &Value) -> Option<u8> {
    let value = value.as_f64().filter(|v| v.is_finite())?;
    Some(value.round().clamp(0.0, 100.0) as u8)
}

/// Amounts arrive as numbers or as decimal strings.
fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|v| v.is_finite())
}

fn ratio_percent(used: f64, limit: f64) -> Option<u8> {
    (limit > 0.0).then(|| (used / limit * 100.0).round().clamp(0.0, 100.0) as u8)
}

/// Parses an RFC 3339 timestamp such as `2026-11-10T17:00:00.123+00:00` into
/// seconds since the Unix epoch.
pub(crate) fn parse_rfc3339(text: &str) -> Option<u64> {
    let (date, time) = text.trim().split_once(['T', 't', ' '])?;
    let mut parts = date.splitn(3, '-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    let (clock, offset) = if let Some(clock) = time.strip_suffix(['Z', 'z']) {
        (clock, 0)
    } else {
        let at = time.rfind(['+', '-'])?;
        let (clock, zone) = time.split_at(at);
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        let (hours, minutes) = zone[1..].split_once(':')?;
        let hours: i64 = hours.parse().ok()?;
        let minutes: i64 = minutes.parse().ok()?;
        (clock, sign * (hours * 3600 + minutes * 60))
    };
    let clock = clock.split('.').next()?;
    let mut parts = clock.splitn(3, ':');
    let hour: i64 = parts.next()?.parse().ok()?;
    let minute: i64 = parts.next()?.parse().ok()?;
    let second: i64 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    // Days from 1970-01-01 to the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset;
    u64::try_from(secs).ok()
}

fn timestamp(value: &Value) -> Option<u64> {
    value.as_str().and_then(parse_rfc3339)
}

fn currency(cents: f64, code: &str) -> String {
    let amount = cents / 100.0;
    match code {
        "USD" => format!("${amount:.2}"),
        code => format!("{code} {amount:.2}"),
    }
}

/// Parses the response of Anthropic's `GET /api/oauth/usage`.
pub(crate) fn parse_anthropic(body: &Value) -> UsageLimits {
    let generic: Vec<&Value> = body["limits"]
        .as_array()
        .map(|l| l.iter().filter(|l| l.is_object()).collect())
        .unwrap_or_default();
    let scope_name = |limit: &Value| -> Option<String> {
        let scope = &limit["scope"];
        [
            &scope["model"]["display_name"],
            &scope["model"]["id"],
            &scope["surface"],
            &scope["surface"]["display_name"],
            &scope["surface"]["id"],
        ]
        .into_iter()
        .find_map(|v| v.as_str().filter(|s| !s.is_empty()).map(str::to_owned))
    };
    let generic_window = |group: &str| {
        generic.iter().find(|l| {
            scope_name(l).is_none()
                && (l["group"] == group || (group == "session" && l["kind"] == group))
        })
    };

    let mut limits = Vec::new();
    for (key, group, label) in [
        ("five_hour", "session", "5h"),
        ("seven_day", "weekly", "weekly"),
    ] {
        let window = &body[key];
        let limit = if !window.is_null() {
            UsageLimit::new(label, percent(&window["utilization"]), true)
                .resets_at(timestamp(&window["resets_at"]))
        } else if let Some(limit) = generic_window(group) {
            UsageLimit::new(label, percent(&limit["percent"]), true)
                .resets_at(timestamp(&limit["resets_at"]))
        } else {
            continue;
        };
        limits.push(limit);
    }
    let rolling = !limits.is_empty();

    let scoped: Vec<UsageLimit> = generic
        .iter()
        .filter_map(|limit| {
            let name = scope_name(limit)?;
            let used = percent(&limit["percent"])?;
            let base = match limit["group"].as_str() {
                Some("session") => "5h",
                Some("weekly") => "weekly",
                Some(group) if !group.is_empty() => group,
                _ => limit["kind"].as_str().unwrap_or("limit"),
            };
            Some(
                UsageLimit::new(format!("{base} ({name})"), Some(used), false)
                    .resets_at(timestamp(&limit["resets_at"])),
            )
        })
        .collect();
    if scoped.is_empty() {
        for (key, name) in [
            ("seven_day_opus", "Opus"),
            ("seven_day_sonnet", "Sonnet"),
            ("seven_day_cowork", "Cowork"),
        ] {
            let bucket = &body[key];
            if let Some(used) = percent(&bucket["utilization"]) {
                limits.push(
                    UsageLimit::new(format!("weekly ({name})"), Some(used), false)
                        .resets_at(timestamp(&bucket["resets_at"])),
                );
            }
        }
    } else {
        limits.extend(scoped);
    }

    // Extra usage is a monthly pool billed in cents. Its `utilization` field is
    // unreliable, so the share is computed from the amounts.
    let extra = &body["extra_usage"];
    if extra["is_enabled"] == true
        && let (Some(limit), Some(used)) = (
            number(&extra["monthly_limit"]),
            number(&extra["used_credits"]),
        )
    {
        let code = extra["currency"].as_str().unwrap_or("USD");
        limits.push(
            UsageLimit::new("extra usage", ratio_percent(used, limit), !rolling)
                .detail(format!(
                    "{} / {}",
                    currency(used, code),
                    currency(limit, code)
                ))
                .resets_at(timestamp(&extra["resets_at"])),
        );
    }
    UsageLimits { limits }
}

fn window_label(seconds: Option<f64>, fallback: &str) -> String {
    match seconds.map(|s| s.round() as u64) {
        Some(604_800) => "weekly".into(),
        Some(86_400) => "daily".into(),
        Some(s) if s > 0 && s % 86_400 == 0 => format!("{}d", s / 86_400),
        Some(s) if s > 0 && s % 3600 == 0 => format!("{}h", s / 3600),
        _ => fallback.into(),
    }
}

fn reset_time(window: &Value, now: u64) -> Option<u64> {
    number(&window["reset_at"])
        .map(|at| at as u64)
        .or_else(|| number(&window["reset_after_seconds"]).map(|after| now + after as u64))
}

fn count(value: f64) -> String {
    let rounded = (value * 100.0).round() / 100.0;
    if rounded.fract() == 0.0 {
        let mut digits = format!("{}", rounded as i64);
        let mut at = digits.len();
        while at > 3 {
            at -= 3;
            digits.insert(at, ',');
        }
        digits
    } else {
        format!("{rounded:.2}")
    }
}

/// Parses the response of ChatGPT's `GET /backend-api/wham/usage`, which also
/// covers the Codex limits.
pub(crate) fn parse_codex(body: &Value, now: u64) -> UsageLimits {
    let mut limits = Vec::new();
    let rate = &body["rate_limit"];
    for (key, fallback) in [("primary_window", "5h"), ("secondary_window", "weekly")] {
        let window = &rate[key];
        if let Some(used) = percent(&window["used_percent"]) {
            limits.push(
                UsageLimit::new(
                    window_label(number(&window["limit_window_seconds"]), fallback),
                    Some(used),
                    true,
                )
                .resets_at(reset_time(window, now)),
            );
        }
    }
    let rolling = !limits.is_empty();

    // Business and Enterprise workspaces meter Codex with a credit allowance.
    let spend = &body["spend_control"];
    let allowance = ["individual_limit", "group_limit", "limit"]
        .into_iter()
        .map(|key| &spend[key])
        .find_map(|c| {
            let limit = number(&c["limit"]).filter(|l| *l > 0.0)?;
            let used = number(&c["used"])
                .or_else(|| number(&c["remaining"]).map(|r| (limit - r).max(0.0)))?;
            Some(
                UsageLimit::new("credits", ratio_percent(used, limit), !rolling)
                    .detail(format!("{} / {} used", count(used), count(limit)))
                    .resets_at(reset_time(c, now)),
            )
        });
    let credits = &body["credits"];
    let balance = || {
        if credits["unlimited"] == true {
            return Some(UsageLimit::new("credits", None, !rolling).detail("unlimited"));
        }
        let balance = number(&credits["balance"])?;
        Some(
            match number(&credits["total_granted"]).filter(|g| *g > 0.0) {
                Some(granted) => {
                    let used = (granted - balance).max(0.0);
                    UsageLimit::new("credits", ratio_percent(used, granted), !rolling)
                        .detail(format!("{} / {} used", count(used), count(granted)))
                }
                None => UsageLimit::new("credits", None, !rolling)
                    .detail(format!("{} left", count(balance))),
            },
        )
    };
    limits.extend(allowance.or_else(balance));
    UsageLimits { limits }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_rfc3339_timestamps() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-11-10T17:00:00Z"), Some(1_794_330_000));
        assert_eq!(
            parse_rfc3339("2026-11-10T17:00:00.123456+00:00"),
            Some(1_794_330_000)
        );
        assert_eq!(
            parse_rfc3339("2026-11-11T02:00:00+09:00"),
            Some(1_794_330_000)
        );
        assert_eq!(
            parse_rfc3339("2026-11-10T12:30:00-04:30"),
            Some(1_794_330_000)
        );
        assert_eq!(parse_rfc3339("2024-02-29T00:00:00Z"), Some(1_709_164_800));
        assert_eq!(parse_rfc3339("not a date"), None);
        assert_eq!(parse_rfc3339("2026-13-01T00:00:00Z"), None);
    }

    fn limit(label: &str, used: Option<u8>, headline: bool) -> UsageLimit {
        UsageLimit::new(label, used, headline)
    }

    #[test]
    fn anthropic_rolling_windows_and_model_caps() {
        let body = json!({
            "five_hour": {"utilization": 23.4, "resets_at": "2026-11-10T17:00:00+00:00"},
            "seven_day": {"utilization": 41.0, "resets_at": null},
            "seven_day_opus": {"utilization": 12.0, "resets_at": "2026-11-12T00:00:00Z"},
            "seven_day_sonnet": null,
            "extra_usage": {"is_enabled": true, "monthly_limit": 10000, "used_credits": 1234, "currency": "USD", "utilization": 0.1234},
        });
        assert_eq!(
            parse_anthropic(&body).limits,
            vec![
                limit("5h", Some(23), true).resets_at(Some(1_794_330_000)),
                limit("weekly", Some(41), true),
                limit("weekly (Opus)", Some(12), false).resets_at(Some(1_794_441_600)),
                limit("extra usage", Some(12), false).detail("$12.34 / $100.00"),
            ]
        );
    }

    #[test]
    fn anthropic_generic_limits_replace_the_legacy_fields() {
        let body = json!({
            "five_hour": null,
            "seven_day": null,
            "seven_day_opus": {"utilization": 99.0},
            "limits": [
                {"kind": "session", "group": "session", "percent": 7, "resets_at": "2026-11-10T17:00:00Z"},
                {"kind": "weekly", "group": "weekly", "percent": 55.5},
                {"kind": "weekly", "group": "weekly", "percent": 80, "scope": {"model": {"id": "fable", "display_name": "Fable"}}},
                "junk",
            ],
        });
        assert_eq!(
            parse_anthropic(&body).limits,
            vec![
                limit("5h", Some(7), true).resets_at(Some(1_794_330_000)),
                limit("weekly", Some(56), true),
                limit("weekly (Fable)", Some(80), false),
            ]
        );
    }

    #[test]
    fn anthropic_extra_usage_is_the_headline_without_rolling_windows() {
        let body = json!({
            "five_hour": null,
            "seven_day": null,
            "extra_usage": {"is_enabled": true, "monthly_limit": 100000, "used_credits": 22869, "currency": "EUR"},
        });
        assert_eq!(
            parse_anthropic(&body).limits,
            vec![limit("extra usage", Some(23), true).detail("EUR 228.69 / EUR 1000.00")]
        );
        let disabled =
            json!({"extra_usage": {"is_enabled": false, "monthly_limit": 1, "used_credits": 1}});
        assert!(parse_anthropic(&disabled).limits.is_empty());
    }

    #[test]
    fn codex_windows_take_labels_from_their_length() {
        let body = json!({
            "rate_limit": {
                "primary_window": {"used_percent": 12, "limit_window_seconds": 18000, "reset_after_seconds": 600},
                "secondary_window": {"used_percent": 64.6, "limit_window_seconds": 604800, "reset_at": 1_794_330_000},
            },
            "credits": {"has_credits": false, "unlimited": false, "balance": "0"},
        });
        assert_eq!(
            parse_codex(&body, 1000).limits,
            vec![
                limit("5h", Some(12), true).resets_at(Some(1600)),
                limit("weekly", Some(65), true).resets_at(Some(1_794_330_000)),
                limit("credits", None, false).detail("0 left"),
            ]
        );
        let unlabelled = json!({"rate_limit": {"primary_window": {"used_percent": 1}}});
        assert_eq!(
            parse_codex(&unlabelled, 0).limits,
            vec![limit("5h", Some(1), true)]
        );
    }

    #[test]
    fn codex_credit_plans_show_the_allowance() {
        let body = json!({
            "rate_limit": null,
            "spend_control": {"individual_limit": {"limit": "2500", "remaining": "1250.5", "reset_after_seconds": 60}},
            "credits": {"unlimited": true},
        });
        assert_eq!(
            parse_codex(&body, 100).limits,
            vec![
                limit("credits", Some(50), true)
                    .detail("1249.50 / 2,500 used")
                    .resets_at(Some(160))
            ]
        );
        let unlimited = json!({"credits": {"unlimited": true}});
        assert_eq!(
            parse_codex(&unlimited, 0).limits,
            vec![limit("credits", None, true).detail("unlimited")]
        );
        assert!(parse_codex(&json!({}), 0).limits.is_empty());
    }
}
