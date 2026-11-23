use std::collections::{BTreeMap, HashSet};

use loom_llm::{limit_to_accounts, validate_config};
use loom_oracle_proto::{
    CodespaceSession, ErrorCode, Event, FirstMessage, MAX_AGENTS_MD, MAX_FIRST_MESSAGE, MAX_NOTES,
    MAX_PRESET_FILES, MAX_PRESET_FILES_BYTES, MAX_SECRET, OracleError, Preset, PresetBundle,
    PresetDetail, PresetFile, Secret, SecretInfo, SessionSummary, WorkspaceNotes, valid_name,
    valid_preset_path,
};

use super::git::{parse_preset, preset};
use super::{Oracle, bad_request, not_found};
use crate::store::{PresetInput, SecretRow, SessionRow};

const MAX_SESSIONS: usize = 500;
const MAX_TITLE: usize = 500;

fn session(row: &SessionRow) -> CodespaceSession {
    CodespaceSession {
        codespace: row.codespace.clone(),
        workspace: row.workspace.clone(),
        session: row.session.clone(),
    }
}

fn check_files(files: &[PresetFile]) -> Result<Vec<(String, String)>, OracleError> {
    if files.len() > MAX_PRESET_FILES {
        return Err(bad_request(format!(
            "a preset has up to {MAX_PRESET_FILES} skill files"
        )));
    }
    let total: usize = files.iter().map(|f| f.content.len()).sum();
    if total > MAX_PRESET_FILES_BYTES {
        return Err(bad_request(format!(
            "the skill files of a preset have up to {MAX_PRESET_FILES_BYTES} bytes"
        )));
    }
    let mut seen = HashSet::new();
    let mut checked = Vec::new();
    for file in files {
        if !valid_preset_path(&file.path) {
            return Err(bad_request(format!(
                "{:?} is not a path in a skill directory, such as skills/<name>/SKILL.md",
                file.path
            )));
        }
        if !seen.insert(file.path.as_str()) {
            return Err(bad_request(format!(
                "the path {} is there twice",
                file.path
            )));
        }
        if file.content.contains('\0') {
            return Err(bad_request(format!("{} is not a text file", file.path)));
        }
        checked.push((file.path.clone(), file.content.clone()));
    }
    let skills: HashSet<&str> = files
        .iter()
        .filter_map(|f| f.path.split('/').nth(1))
        .collect();
    for skill in skills {
        let main = format!("skills/{skill}/SKILL.md");
        if !seen.contains(main.as_str()) {
            return Err(bad_request(format!("the skill {skill} has no {main}")));
        }
    }
    Ok(checked)
}

fn check_summary(summary: &SessionSummary) -> Result<(), OracleError> {
    let short = |text: &str, max: usize| {
        !text.is_empty() && text.len() <= max && !text.chars().any(char::is_control)
    };
    if !short(&summary.id, 128)
        || !short(&summary.state, 64)
        || summary
            .title
            .as_ref()
            .is_some_and(|title| title.len() > MAX_TITLE || title.contains('\0'))
        || summary
            .model
            .as_ref()
            .is_some_and(|model| !short(model, 200))
    {
        return Err(bad_request("a session summary is not valid"));
    }
    Ok(())
}

impl Oracle {
    pub fn set_preset(
        &self,
        name: &str,
        yaml: &str,
        agents_md: Option<&str>,
        files: Option<&[PresetFile]>,
        by: Option<u64>,
    ) -> Result<Preset, OracleError> {
        if !valid_name(name) {
            return Err(bad_request(format!(
                "{name:?} is not a preset name: use 1 to 40 lowercase letters, digits, and '-', and start with a letter"
            )));
        }
        let config = parse_preset(name, yaml)?;
        let credentials = self.preset_git_credentials(name, &config)?;
        let mut accounts = Vec::new();
        for (provider, account) in &config.llm.accounts {
            let row = self.store.model_account_by_name(account)?.ok_or_else(|| {
                bad_request(format!(
                    "preset {name}: llm.accounts names {account:?}, but the oracle has no such model account"
                ))
            })?;
            if &row.provider != provider {
                return Err(bad_request(format!(
                    "preset {name}: llm.accounts.{provider} names {account:?}, which is an account of {}",
                    row.provider
                )));
            }
            accounts.push((provider.clone(), row.id));
        }
        let mut llm = config.llm.clone();
        limit_to_accounts(&mut llm);
        validate_config(&llm).map_err(|err| {
            bad_request(format!(
                "preset {name}: {err}; a provider needs an account in llm.accounts"
            ))
        })?;
        let mut secrets = Vec::new();
        for (variable, secret) in &config.environment.secrets {
            let row = self.store.secret_by_name(secret)?.ok_or_else(|| {
                bad_request(format!(
                    "preset {name}: environment.secrets names {secret:?}, but the oracle has no such secret"
                ))
            })?;
            secrets.push((variable.clone(), row.id));
        }
        if agents_md.is_some_and(|text| text.len() > MAX_AGENTS_MD || text.contains('\0')) {
            return Err(bad_request(format!(
                "AGENTS.md of a preset is a text of up to {MAX_AGENTS_MD} bytes"
            )));
        }
        let files = files.map(check_files).transpose()?;
        let row = self.store.put_preset(&PresetInput {
            name,
            yaml,
            agents_md,
            files: files.as_deref(),
            credentials: &credentials,
            accounts: &accounts,
            secrets: &secrets,
        })?;
        self.audit(by, "preset_set", &format!("preset {name}"));
        let preset = preset(&row);
        self.emit(Event::PresetChanged {
            preset: preset.clone(),
        });
        self.emit(Event::ConfigChanged {
            presets: vec![name.to_owned()],
        });
        self.git_changed();
        Ok(preset)
    }

    pub fn preset_detail(&self, name: &str) -> Result<PresetDetail, OracleError> {
        let row = self.preset_by_name(name)?;
        let files = self
            .store
            .preset_files(row.id)?
            .into_iter()
            .map(|(path, content)| PresetFile { path, content })
            .collect();
        Ok(PresetDetail {
            preset: preset(&row),
            files,
        })
    }

    pub(super) fn preset_bundle(&self, preset: Option<u64>) -> Result<PresetBundle, OracleError> {
        let Some(row) = preset
            .map(|id| self.store.preset(id))
            .transpose()?
            .flatten()
        else {
            return Ok(PresetBundle::default());
        };
        let files = self
            .store
            .preset_files(row.id)?
            .into_iter()
            .map(|(path, content)| PresetFile { path, content })
            .collect();
        let mut secrets = BTreeMap::new();
        for (variable, id, _) in &row.secrets {
            let value = self.store.secret_value(*id)?;
            secrets.insert(variable.clone(), Secret::new(value.as_str()));
        }
        Ok(PresetBundle {
            name: row.name.clone(),
            yaml: row.yaml.clone(),
            agents_md: row.agents_md.clone(),
            files,
            secrets,
            accounts: row
                .accounts
                .iter()
                .map(|(provider, _, name)| (provider.clone(), name.clone()))
                .collect(),
        })
    }

    fn secret_info(&self, row: &SecretRow) -> Result<SecretInfo, OracleError> {
        Ok(SecretInfo {
            name: row.name.clone(),
            presets: self.store.presets_with_secret(row.id)?,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }

    pub fn secrets(&self) -> Result<Vec<SecretInfo>, OracleError> {
        self.store
            .secrets()?
            .iter()
            .map(|row| self.secret_info(row))
            .collect()
    }

    pub fn set_secret(
        &self,
        name: &str,
        value: &Secret,
        by: Option<u64>,
    ) -> Result<SecretInfo, OracleError> {
        if !valid_name(name) {
            return Err(bad_request(format!(
                "{name:?} is not a secret name: use 1 to 40 lowercase letters, digits, and '-', and start with a letter"
            )));
        }
        let value = value.expose();
        if value.is_empty() || value.len() > MAX_SECRET || value.contains(['\0', '\n', '\r']) {
            return Err(bad_request(format!(
                "a secret is one line of 1 to {MAX_SECRET} bytes"
            )));
        }
        let row = self.store.put_secret(name, value)?;
        self.audit(by, "secret_set", &format!("secret {name}"));
        let info = self.secret_info(&row)?;
        if !info.presets.is_empty() {
            self.emit(Event::ConfigChanged {
                presets: info.presets.clone(),
            });
        }
        self.emit(Event::SecretChanged {
            secret: info.clone(),
        });
        Ok(info)
    }

    pub fn delete_secret(&self, name: &str, by: Option<u64>) -> Result<(), OracleError> {
        if !self.store.delete_secret(name)? {
            return Err(not_found(format!("no secret has the name {name}")));
        }
        self.audit(by, "secret_deleted", &format!("secret {name}"));
        self.emit(Event::SecretRemoved {
            name: name.to_owned(),
        });
        Ok(())
    }

    pub fn workspace_notes(&self, workspace: &str) -> Result<WorkspaceNotes, OracleError> {
        let row = self
            .store
            .workspace_by_name(workspace)?
            .ok_or_else(|| not_found(format!("no workspace has the name {workspace}")))?;
        let notes = self.store.notes(row.id)?;
        Ok(WorkspaceNotes {
            workspace: row.name,
            text: notes.as_ref().map(|n| n.0.clone()).unwrap_or_default(),
            updated_at: notes.map(|n| n.1),
        })
    }

    fn store_notes(&self, workspace: u64, name: &str, text: &str) -> Result<u64, OracleError> {
        if text.len() > MAX_NOTES || text.contains('\0') {
            return Err(bad_request(format!(
                "the notes are a text of up to {MAX_NOTES} bytes"
            )));
        }
        let updated_at = self.store.set_notes(workspace, text)?;
        self.emit(Event::WorkspaceNotesChanged {
            workspace: name.to_owned(),
            updated_at: Some(updated_at),
        });
        Ok(updated_at)
    }

    pub fn set_workspace_notes(
        &self,
        workspace: &str,
        text: &str,
        by: Option<u64>,
    ) -> Result<WorkspaceNotes, OracleError> {
        let row = self
            .store
            .workspace_by_name(workspace)?
            .ok_or_else(|| not_found(format!("no workspace has the name {workspace}")))?;
        let updated_at = self.store_notes(row.id, &row.name, text)?;
        self.audit(by, "notes_set", &format!("notes of workspace {}", row.name));
        Ok(WorkspaceNotes {
            workspace: row.name,
            text: text.to_owned(),
            updated_at: Some(updated_at),
        })
    }

    pub fn codespace_notes(&self, peer: u64, text: &str) -> Result<(), OracleError> {
        let row = self.codespace_of_peer(peer)?;
        if !row.state.is_open() {
            return Err(OracleError::new(
                ErrorCode::Conflict,
                format!("the codespace is {}", row.state),
            ));
        }
        self.store_notes(row.workspace, &row.workspace_name, text)?;
        Ok(())
    }

    pub fn sessions(&self, codespace: Option<&str>) -> Result<Vec<CodespaceSession>, OracleError> {
        Ok(self
            .store
            .sessions(codespace)?
            .iter()
            .map(session)
            .collect())
    }

    pub fn session_summaries(
        &self,
        peer: u64,
        sessions: &[SessionSummary],
        removed: &[String],
        full: bool,
    ) -> Result<(), OracleError> {
        let row = self.codespace_of_peer(peer)?;
        if sessions.len() > MAX_SESSIONS || removed.len() > MAX_SESSIONS {
            return Err(bad_request(format!(
                "send up to {MAX_SESSIONS} session summaries at a time"
            )));
        }
        for summary in sessions {
            check_summary(summary)?;
        }
        let deleted = self.store.put_sessions(&row.id, sessions, removed, full)?;
        for id in deleted {
            self.emit(Event::SessionRemoved {
                codespace: row.id.clone(),
                id,
            });
        }
        for summary in sessions {
            self.emit(Event::SessionChanged {
                session: CodespaceSession {
                    codespace: row.id.clone(),
                    workspace: row.workspace_name.clone(),
                    session: summary.clone(),
                },
            });
        }
        Ok(())
    }

    pub(super) fn check_first_message(message: &FirstMessage) -> Result<String, OracleError> {
        if message.text.trim().is_empty()
            || message.text.len() > MAX_FIRST_MESSAGE
            || message.text.contains('\0')
        {
            return Err(bad_request(format!(
                "the first message is a text of 1 to {MAX_FIRST_MESSAGE} bytes"
            )));
        }
        if message.model.as_ref().is_some_and(|model| {
            model.is_empty() || model.len() > 200 || !model.chars().all(|c| c.is_ascii_graphic())
        }) {
            return Err(bad_request("the model of the first message is not a model"));
        }
        serde_json::to_string(message).map_err(|err| bad_request(err.to_string()))
    }

    pub fn take_first_message(&self, peer: u64) -> Result<Option<FirstMessage>, OracleError> {
        let row = self.codespace_of_peer(peer)?;
        let Some(text) = self.store.take_first_message(&row.id)? else {
            return Ok(None);
        };
        let message = serde_json::from_str(&text).ok();
        if let Ok(Some(row)) = self.store.codespace(&row.id) {
            self.emit(Event::CodespaceChanged {
                codespace: super::codespaces::codespace(&row),
            });
        }
        Ok(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str) -> PresetFile {
        PresetFile {
            path: path.into(),
            content: "text".into(),
        }
    }

    #[test]
    fn skill_files_need_a_skill_file_in_each_directory() {
        assert!(check_files(&[file("skills/a/SKILL.md"), file("skills/a/run.sh")]).is_ok());
        assert!(check_files(&[file("skills/a/run.sh")]).is_err());
        assert!(check_files(&[file("skills/a/SKILL.md"), file("skills/a/SKILL.md")]).is_err());
        assert!(check_files(&[file("AGENTS.md")]).is_err());
        let large = PresetFile {
            path: "skills/a/SKILL.md".into(),
            content: "x".repeat(MAX_PRESET_FILES_BYTES + 1),
        };
        assert!(check_files(&[large]).is_err());
    }

    #[test]
    fn summaries_are_checked() {
        let mut summary = SessionSummary {
            id: "s1".into(),
            title: Some("Fix the build".into()),
            state: "idle".into(),
            archived: false,
            model: Some("anthropic/claude".into()),
            created_at: 1,
            updated_at: 2,
        };
        assert!(check_summary(&summary).is_ok());
        summary.id = String::new();
        assert!(check_summary(&summary).is_err());
    }
}
