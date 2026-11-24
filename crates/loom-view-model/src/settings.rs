use std::collections::BTreeMap;

use loom_oracle_proto::{
    AuditEntry, Environment, GitCredential, HostCheck, HostCheckStatus, HostLimits, Preset,
    Repository, Volume, VolumeKind, VolumeUsage, WorkspaceVolume,
};

pub const SIGN_IN_PROVIDERS: &[&str] = &["anthropic", "openai", "openai-codex"];
pub const DEVICE_CODE_PROVIDERS: &[&str] = &["openai-codex"];
pub const EXPIRY_WARNING_SECS: u64 = 14 * 24 * 3600;

fn days(n: u64) -> String {
    if n == 1 {
        "1 day".to_owned()
    } else {
        format!("{n} days")
    }
}

pub fn age(now: u64, then: u64) -> String {
    match now.saturating_sub(then) {
        0..60 => "just now".to_owned(),
        seconds @ 60..3600 => format!("{} min ago", seconds / 60),
        seconds @ 3600..86_400 => format!("{} h ago", seconds / 3600),
        seconds => format!("{} ago", days(seconds / 86_400)),
    }
}

pub fn until(now: u64, then: u64) -> String {
    if then <= now {
        return "expired".to_owned();
    }
    match then - now {
        0..3600 => format!("expires in {} min", (then - now).div_ceil(60)),
        seconds @ 3600..86_400 => format!("expires in {} h", seconds / 3600),
        seconds => format!("expires in {}", days(seconds / 86_400)),
    }
}

pub fn in_time(now: u64, then: u64) -> String {
    match then.saturating_sub(now) {
        0 => "now".to_owned(),
        seconds @ 1..3600 => format!("in {} min", seconds.div_ceil(60)),
        seconds @ 3600..86_400 => format!("in {} h", seconds / 3600),
        seconds => format!("in {}", days(seconds / 86_400)),
    }
}

const UNITS: [(&str, u64); 4] = [("d", 86_400), ("h", 3600), ("m", 60), ("s", 1)];

pub fn duration_text(secs: u64) -> String {
    if secs == 0 {
        return "never".to_owned();
    }
    UNITS
        .iter()
        .find(|(_, size)| secs.is_multiple_of(*size))
        .map(|(unit, size)| format!("{}{unit}", secs / size))
        .unwrap_or_else(|| format!("{secs}s"))
}

pub fn parse_limit_count(text: &str) -> Result<Option<u32>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    match text.parse::<u32>() {
        Ok(0) | Err(_) => Err(format!("{text:?} is not a number of 1 or more")),
        Ok(n) => Ok(Some(n)),
    }
}

pub fn parse_limit_duration(text: &str) -> Result<Option<u64>, String> {
    let text = text.trim().to_ascii_lowercase();
    if text.is_empty() {
        return Ok(None);
    }
    if text == "never" || text == "0" {
        return Ok(Some(0));
    }
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let size = match unit.trim() {
        "s" | "sec" | "secs" => 1,
        "m" | "min" | "mins" => 60,
        "h" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86_400,
        _ => {
            return Err(format!(
                "{text:?} is not a time; use a number and a unit, such as 30m, 2h, or 7d, or 'never'"
            ));
        }
    };
    number
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(size))
        .filter(|secs| *secs > 0)
        .map(Some)
        .ok_or_else(|| format!("{text:?} is not a time such as 30m, 2h, or 7d"))
}

pub fn limit_duration_field(secs: Option<u64>) -> String {
    secs.map(duration_text).unwrap_or_default()
}

pub fn limits_line(limits: &HostLimits) -> String {
    let mut parts = Vec::new();
    if let Some(max) = limits.max_codespaces {
        parts.push(format!("at most {max} codespaces"));
    }
    if let Some(max) = limits.max_running {
        parts.push(format!("at most {max} running"));
    }
    match limits.idle_stop_secs {
        Some(0) => parts.push("no idle stop".to_owned()),
        Some(secs) => parts.push(format!("stop after {} idle", duration_text(secs))),
        None => parts.push("idle stop of the oracle".to_owned()),
    }
    match limits.retention_secs {
        Some(0) => parts.push("keep stopped codespaces".to_owned()),
        Some(secs) => parts.push(format!("delete after {} stopped", duration_text(secs))),
        None => parts.push("retention of the oracle".to_owned()),
    }
    parts.join(" · ")
}

pub fn audit_actor(entry: &AuditEntry) -> String {
    match (&entry.peer_label, entry.peer) {
        (Some(label), _) => label.clone(),
        (None, Some(peer)) => format!("peer {peer}"),
        (None, None) => "oracle".to_owned(),
    }
}

pub fn audit_action(action: &str) -> String {
    let text = action.replace('_', " ");
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => text,
    }
}

pub fn expiry_warning(now: u64, expires_at: Option<u64>) -> Option<String> {
    let expires_at = expires_at?;
    (expires_at <= now + EXPIRY_WARNING_SECS).then(|| until(now, expires_at))
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn usage_label(usage: &VolumeUsage) -> String {
    match (usage.used_bytes, usage.total_bytes, &usage.error) {
        (_, _, Some(error)) => error.clone(),
        (Some(used), Some(total), None) => {
            format!("{} of {}", human_bytes(used), human_bytes(total))
        }
        (Some(used), None, None) => human_bytes(used),
        (None, _, None) => "size unknown".to_owned(),
    }
}

pub fn check_problems(checks: &[HostCheck]) -> usize {
    checks
        .iter()
        .filter(|c| c.status != HostCheckStatus::Ok)
        .count()
}

pub fn variables_text(environment: &Environment) -> String {
    environment
        .variables
        .iter()
        .map(|(name, value)| format!("{name}={value}\n"))
        .collect()
}

pub fn parse_variables(lines: &[String]) -> Result<BTreeMap<String, String>, String> {
    let mut variables = BTreeMap::new();
    for line in lines {
        let Some((name, value)) = line.split_once('=') else {
            return Err(format!("{line:?} is not NAME=VALUE"));
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(format!("{line:?} has no variable name"));
        }
        if variables
            .insert(name.to_owned(), value.trim().to_owned())
            .is_some()
        {
            return Err(format!("the variable {name} is there twice"));
        }
    }
    Ok(variables)
}

pub fn split_certificates(text: &str) -> Vec<String> {
    const END: &str = "-----END CERTIFICATE-----";
    let mut certificates = Vec::new();
    let mut rest = text;
    while let Some(end) = rest.find(END) {
        let certificate = rest[..end + END.len()].trim();
        if !certificate.is_empty() {
            certificates.push(format!("{certificate}\n"));
        }
        rest = &rest[end + END.len()..];
    }
    let tail = rest.trim();
    if !tail.is_empty() {
        certificates.push(format!("{tail}\n"));
    }
    certificates
}

pub fn secret_uses(presets: &[Preset], secret: &str) -> Vec<(String, String)> {
    let mut uses = Vec::new();
    for preset in presets {
        for (variable, name) in &preset.secrets {
            if name == secret {
                uses.push((preset.name.clone(), variable.clone()));
            }
        }
    }
    uses
}

pub fn credential_presets(presets: &[Preset], credential: &str) -> Vec<String> {
    presets
        .iter()
        .filter(|p| p.git_credentials.iter().any(|c| c == credential))
        .map(|p| p.name.clone())
        .collect()
}

pub fn provider_credentials<'a>(
    credentials: &'a [GitCredential],
    provider: &'a str,
) -> impl Iterator<Item = &'a GitCredential> + 'a {
    credentials.iter().filter(move |c| c.provider == provider)
}

pub fn volumes_on_host<'a>(volumes: &'a [Volume], host: &'a str) -> Vec<&'a Volume> {
    volumes
        .iter()
        .filter(|v| v.hosts.iter().any(|h| h == host))
        .collect()
}

pub fn volume_label(volume: &Volume) -> String {
    let mut parts = vec![match volume.kind {
        VolumeKind::Incus => "Incus volume".to_owned(),
        VolumeKind::Directory => "host directory".to_owned(),
        VolumeKind::Unknown => "unknown kind".to_owned(),
    }];
    if let Some(path) = &volume.path {
        parts.push(path.clone());
    }
    match volume.size_mib {
        Some(mib) => parts.push(format!("limit {}", crate::codespaces::format_memory(mib))),
        None if volume.kind == VolumeKind::Incus => parts.push("no size limit".to_owned()),
        None => {}
    }
    if volume.deleting {
        parts.push("being deleted".to_owned());
    }
    parts.join(" · ")
}

pub fn mount_label(mount: &WorkspaceVolume) -> String {
    if mount.read_only {
        format!("{} at {} (read-only)", mount.volume, mount.path)
    } else {
        format!("{} at {}", mount.volume, mount.path)
    }
}

pub fn repository_label(repository: &Repository) -> String {
    let mut text = repository.url.clone();
    let directory = crate::codespaces::directory(repository);
    if repository.directory.is_some() {
        text.push_str(&format!(" → {directory}"));
    }
    if let Some(branch) = &repository.base_branch {
        text.push_str(&format!(" · base {branch}"));
    }
    if let Some(provider) = &repository.provider {
        text.push_str(&format!(" · {provider}"));
    }
    text
}

pub struct Redirect {
    pub host: String,
    pub port: u16,
    pub path: String,
}

pub fn parse_redirect(uri: &str) -> Option<Redirect> {
    let rest = uri.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(ix) => (&rest[..ix], &rest[ix..]),
        None => (rest, "/"),
    };
    let (host, port) = authority.rsplit_once(':')?;
    let local = matches!(host, "localhost" | "127.0.0.1" | "[::1]");
    let port = port.parse().ok()?;
    let path = path.split(['?', '#']).next().unwrap_or("/");
    local.then(|| Redirect {
        host: host.trim_matches(['[', ']']).to_owned(),
        port,
        path: path.to_owned(),
    })
}

pub fn copy_name(name: &str, taken: &[String]) -> String {
    let base = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '-');
    let base = if base.is_empty() { "preset" } else { base };
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !taken.iter().any(|t| t == candidate))
        .unwrap_or_else(|| format!("{base}-copy"))
}

pub fn suggest_name(text: &str, taken: &[String]) -> String {
    let mut name = String::new();
    for c in text.chars().map(|c| c.to_ascii_lowercase()) {
        if c.is_ascii_lowercase() || (c.is_ascii_digit() && !name.is_empty()) {
            name.push(c);
        } else if !name.is_empty() && !name.ends_with('-') {
            name.push('-');
        }
    }
    name.truncate(34);
    let name = name.trim_end_matches('-');
    let name = if name.is_empty() { "workspace" } else { name };
    if taken.iter().any(|t| t == name) {
        copy_name(name, taken)
    } else {
        name.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset(name: &str, credentials: &[&str], secrets: &[(&str, &str)]) -> Preset {
        Preset {
            name: name.into(),
            yaml: String::new(),
            git_credentials: credentials.iter().map(|c| (*c).to_owned()).collect(),
            agents_md: String::new(),
            files: Vec::new(),
            model_accounts: BTreeMap::new(),
            secrets: secrets
                .iter()
                .map(|(v, s)| ((*v).to_owned(), (*s).to_owned()))
                .collect(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn times_read_as_words() {
        assert_eq!(age(1000, 990), "just now");
        assert_eq!(age(10_000, 10_000 - 600), "10 min ago");
        assert_eq!(age(200_000, 0), "2 days ago");
        assert_eq!(until(100, 50), "expired");
        assert_eq!(until(0, 90), "expires in 2 min");
        assert_eq!(until(0, 3 * 86_400), "expires in 3 days");
        assert_eq!(expiry_warning(0, Some(30 * 86_400)), None);
        assert_eq!(
            expiry_warning(0, Some(86_400 + 60)).as_deref(),
            Some("expires in 1 day")
        );
        assert_eq!(expiry_warning(0, None), None);
    }

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(196_608), "192.0 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
        let usage = VolumeUsage {
            name: "c".into(),
            used_bytes: Some(1024),
            total_bytes: Some(1024 * 1024),
            error: None,
        };
        assert_eq!(usage_label(&usage), "1.0 KiB of 1.0 MiB");
    }

    #[test]
    fn variables_and_certificates_come_from_text() {
        let lines = vec![
            "HTTP_PROXY=http://proxy:3128".to_owned(),
            "A = b=c".to_owned(),
        ];
        let parsed = parse_variables(&lines).unwrap();
        assert_eq!(parsed["A"], "b=c");
        assert!(parse_variables(&["NOEQUALS".to_owned()]).is_err());
        assert!(parse_variables(&["A=1".to_owned(), "A=2".to_owned()]).is_err());
        let environment = Environment {
            variables: parsed,
            ..Default::default()
        };
        assert_eq!(
            variables_text(&environment),
            "A=b=c\nHTTP_PROXY=http://proxy:3128\n"
        );
        let pem = "-----BEGIN CERTIFICATE-----\nAA\n-----END CERTIFICATE-----\n\n-----BEGIN CERTIFICATE-----\nBB\n-----END CERTIFICATE-----";
        let certificates = split_certificates(pem);
        assert_eq!(certificates.len(), 2);
        assert!(certificates[1].contains("BB"));
        assert_eq!(split_certificates("  \n"), Vec::<String>::new());
    }

    #[test]
    fn presets_show_where_credentials_and_secrets_go() {
        let presets = vec![
            preset(
                "work",
                &["ghe"],
                &[("NPM_TOKEN", "npm-work"), ("X", "npm-work")],
            ),
            preset("personal", &["github"], &[("NPM_TOKEN", "npm-mine")]),
        ];
        assert_eq!(
            secret_uses(&presets, "npm-work"),
            [
                ("work".to_owned(), "NPM_TOKEN".to_owned()),
                ("work".to_owned(), "X".to_owned())
            ]
        );
        assert_eq!(credential_presets(&presets, "github"), ["personal"]);
    }

    #[test]
    fn local_redirects_can_be_caught() {
        let redirect = parse_redirect("http://localhost:54545/callback?x=1").unwrap();
        assert_eq!(
            (
                redirect.host.as_str(),
                redirect.port,
                redirect.path.as_str()
            ),
            ("localhost", 54545, "/callback")
        );
        assert!(parse_redirect("https://console.example.com/oauth/code").is_none());
        assert!(parse_redirect("http://example.com:80/cb").is_none());
        assert!(parse_redirect("http://127.0.0.1/cb").is_none());
    }

    #[test]
    fn limits_read_and_show_their_times() {
        assert_eq!(parse_limit_count(""), Ok(None));
        assert_eq!(parse_limit_count(" 3 "), Ok(Some(3)));
        assert!(parse_limit_count("0").is_err());
        assert!(parse_limit_count("x").is_err());
        assert_eq!(parse_limit_duration(""), Ok(None));
        assert_eq!(parse_limit_duration("never"), Ok(Some(0)));
        assert_eq!(parse_limit_duration("30m"), Ok(Some(1800)));
        assert_eq!(parse_limit_duration("2 h"), Ok(Some(7200)));
        assert_eq!(parse_limit_duration("7days"), Ok(Some(7 * 86_400)));
        assert!(parse_limit_duration("30").is_err());
        assert!(parse_limit_duration("m").is_err());
        assert_eq!(duration_text(1800), "30m");
        assert_eq!(duration_text(7 * 86_400), "7d");
        assert_eq!(duration_text(90), "90s");
        assert_eq!(limit_duration_field(Some(0)), "never");
        assert_eq!(limit_duration_field(None), "");
        let limits = HostLimits {
            max_running: Some(2),
            idle_stop_secs: Some(0),
            retention_secs: Some(3600),
            ..Default::default()
        };
        assert_eq!(
            limits_line(&limits),
            "at most 2 running · no idle stop · delete after 1h stopped"
        );
        assert_eq!(in_time(100, 100), "now");
        assert_eq!(in_time(0, 90), "in 2 min");
        assert_eq!(in_time(0, 2 * 86_400), "in 2 days");
    }

    #[test]
    fn audit_entries_read_as_text() {
        let mut entry = AuditEntry {
            id: 1,
            at: 0,
            peer: None,
            peer_label: None,
            action: "codespace_idle_stopped".into(),
            detail: "codespace c1".into(),
        };
        assert_eq!(audit_actor(&entry), "oracle");
        entry.peer = Some(4);
        assert_eq!(audit_actor(&entry), "peer 4");
        entry.peer_label = Some("laptop".into());
        assert_eq!(audit_actor(&entry), "laptop");
        assert_eq!(audit_action(&entry.action), "Codespace idle stopped");
    }

    #[test]
    fn copies_get_a_free_name() {
        let taken = vec!["work".to_owned(), "work-2".to_owned()];
        assert_eq!(copy_name("work", &taken), "work-3");
        assert_eq!(copy_name("work-2", &taken), "work-3");
    }

    #[test]
    fn names_come_from_repositories() {
        assert_eq!(suggest_name("Web_Server.v2", &[]), "web-server-v2");
        assert_eq!(suggest_name("2048", &[]), "workspace");
        assert_eq!(suggest_name("api", &["api".to_owned()]), "api-2");
        assert!(loom_oracle_proto::valid_name(&suggest_name(
            &"x".repeat(80),
            &[]
        )));
    }
}
