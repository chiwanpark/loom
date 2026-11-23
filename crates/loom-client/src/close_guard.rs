use std::fmt;

use loom_proto::RepositoryStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseChoice {
    Push,
    Discard,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseGuardError {
    UnknownDirectory(String),
    BothChoices(String),
    NeedsMessage(String),
    Undecided(Vec<RepositoryStatus>),
}

impl fmt::Display for CloseGuardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownDirectory(directory) => write!(
                f,
                "the codespace has no repository in the directory {directory}"
            ),
            Self::BothChoices(directory) => {
                write!(f, "{directory} has two choices: push and discard")
            }
            Self::NeedsMessage(directory) => write!(
                f,
                "{directory} has uncommitted changes, and a push needs a commit message"
            ),
            Self::Undecided(statuses) => {
                let directories: Vec<&str> =
                    statuses.iter().map(|s| s.directory.as_str()).collect();
                write!(
                    f,
                    "these repositories have unsaved work: {}",
                    directories.join(", ")
                )
            }
        }
    }
}

impl std::error::Error for CloseGuardError {}

pub fn needs_choice(status: &RepositoryStatus) -> bool {
    status.error.is_some() || status.has_unsaved_work()
}

pub fn close_plan(
    statuses: &[RepositoryStatus],
    choice: impl Fn(&str) -> Option<CloseChoice>,
    has_message: bool,
) -> Result<Vec<String>, CloseGuardError> {
    let mut push = Vec::new();
    let mut undecided = Vec::new();
    for status in statuses.iter().filter(|s| needs_choice(s)) {
        match choice(&status.directory) {
            Some(CloseChoice::Push) => {
                if status.changed_files > 0 && !has_message {
                    return Err(CloseGuardError::NeedsMessage(status.directory.clone()));
                }
                push.push(status.directory.clone());
            }
            Some(CloseChoice::Discard) => {}
            None => undecided.push(status.clone()),
        }
    }
    if undecided.is_empty() {
        Ok(push)
    } else {
        Err(CloseGuardError::Undecided(undecided))
    }
}

pub fn choices_from_lists(
    statuses: &[RepositoryStatus],
    push: &[String],
    discard: &[String],
) -> Result<impl Fn(&str) -> Option<CloseChoice> + use<>, CloseGuardError> {
    for directory in push.iter().chain(discard).filter(|d| *d != "*") {
        if !statuses.iter().any(|s| &s.directory == directory) {
            return Err(CloseGuardError::UnknownDirectory(directory.clone()));
        }
    }
    let selects =
        |list: &[String], directory: &str| list.iter().any(|d| d == "*" || d == directory);
    for status in statuses {
        if selects(push, &status.directory) && selects(discard, &status.directory) {
            return Err(CloseGuardError::BothChoices(status.directory.clone()));
        }
    }
    let (push, discard) = (push.to_vec(), discard.to_vec());
    Ok(move |directory: &str| {
        if selects(&push, directory) {
            Some(CloseChoice::Push)
        } else if selects(&discard, directory) {
            Some(CloseChoice::Discard)
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(directory: &str, changed_files: u32, unpushed: u32) -> RepositoryStatus {
        RepositoryStatus {
            directory: directory.into(),
            path: format!("/workspaces/acme/{directory}"),
            url: format!("https://github.com/acme/{directory}.git"),
            provider: Some("github".into()),
            branch: "loom/c1".into(),
            base_branch: Some("main".into()),
            changed_files,
            ahead: unpushed,
            unpushed,
            pushed: false,
            pull_request: None,
            error: None,
        }
    }

    #[test]
    fn each_repository_with_work_needs_a_choice() {
        let statuses = vec![
            status("app", 2, 0),
            status("lib", 0, 3),
            status("docs", 0, 0),
        ];
        let err = close_plan(&statuses, |_| None, false).unwrap_err();
        let CloseGuardError::Undecided(undecided) = &err else {
            panic!("{err}");
        };
        assert_eq!(undecided.len(), 2);
        assert_eq!(
            err.to_string(),
            "these repositories have unsaved work: app, lib"
        );

        let choose = choices_from_lists(&statuses, &["lib".into()], &["app".into()]).unwrap();
        assert_eq!(
            close_plan(&statuses, &choose, false).unwrap(),
            vec!["lib".to_owned()]
        );

        let all = choices_from_lists(&statuses, &["*".into()], &[]).unwrap();
        assert_eq!(
            close_plan(&statuses, &all, false),
            Err(CloseGuardError::NeedsMessage("app".into()))
        );
        assert_eq!(
            close_plan(&statuses, &all, true).unwrap(),
            vec!["app".to_owned(), "lib".to_owned()]
        );

        let mut broken = status("app", 0, 0);
        broken.error = Some("the clone is missing".into());
        assert!(close_plan(&[broken], |_| None, false).is_err());
    }

    #[test]
    fn choices_name_known_repositories_once() {
        let statuses = vec![status("app", 1, 0)];
        assert!(matches!(
            choices_from_lists(&statuses, &["nope".into()], &[]),
            Err(CloseGuardError::UnknownDirectory(_))
        ));
        assert!(matches!(
            choices_from_lists(&statuses, &["app".into()], &["*".into()]),
            Err(CloseGuardError::BothChoices(_))
        ));
    }
}
