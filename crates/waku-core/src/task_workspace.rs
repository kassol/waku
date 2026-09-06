//! Explicit task-owned Git resources. No legacy workspace is adopted implicitly.
use super::*;
use crate::model::{
    ManagedWorkspace, ManagedWorkspaceLocation, SessionWorkspace, StewardWorkspaceOperation,
};

pub(super) fn git(path: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = crate::command_env::plain_command("git")
        .args(args)
        .current_dir(path)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!("{}", if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() });
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

pub(super) fn canonical_workspace(path: &Path) -> anyhow::Result<PathBuf> {
    let path = std::fs::canonicalize(path)?;
    match git(&path, &["rev-parse", "--show-toplevel"]) {
        Ok(root) => Ok(std::fs::canonicalize(root)?),
        Err(_) => Ok(path),
    }
}

impl WakuBackend {
    pub(super) fn steward_workspace(
        &self,
        session_id: Uuid,
        operation: StewardWorkspaceOperation,
        events: &EventSink,
    ) -> anyhow::Result<ResponsePayload> {
        events.ensure_steward_active()?;
        let _operation = events.reserve_steward_target(session_id)?;
        match operation {
            StewardWorkspaceOperation::Begin {
                name,
                target_branch,
                expected_commit,
            } => {
                self.begin_task_workspace(session_id, name, target_branch, expected_commit, events)
            }
            StewardWorkspaceOperation::Deliver {
                commit,
                expected_target_commit,
                evidence,
            } => self.deliver_task(session_id, commit, expected_target_commit, evidence),
            StewardWorkspaceOperation::Integrate {
                session_id: child,
                commit,
                expected_integration_commit,
                evidence,
            } => self.integrate_task_result(
                session_id,
                child,
                commit,
                expected_integration_commit,
                evidence,
                events,
            ),
            StewardWorkspaceOperation::Inspect { session_id: target } => {
                let mut state = self.task_state.lock();
                let session = if target == session_id {
                    let session = state
                        .sessions
                        .iter_mut()
                        .find(|s| s.id == target)
                        .ok_or_else(|| anyhow!("session is unavailable"))?;
                    self.task_store.hydrate(session)?;
                    session.clone()
                } else {
                    self.authorized_child(&mut state, session_id, target, events)?
                        .0
                };
                let mut session = session;
                if let Some(resource) = &mut session.managed_workspace {
                    if let Ok(commit) = git(
                        &resource.repository,
                        &["rev-parse", "--verify", &resource.integration_branch],
                    ) {
                        resource.integration_commit = commit;
                    }
                }
                Ok(ResponsePayload::TaskWorkspace { session })
            }
        }
    }

    fn begin_task_workspace(
        &self,
        session_id: Uuid,
        name: String,
        target_branch: String,
        expected_commit: String,
        events: &EventSink,
    ) -> anyhow::Result<ResponsePayload> {
        // A running provider's cwd cannot be moved. This explicit action is
        // available before the first submission, or after closing its runtime.
        let _start = self.workspace_start_gate.lock();
        if self.sessions.lock().contains_key(&session_id) {
            bail!("close the session runtime before creating its task workspace");
        }
        if name.trim().is_empty() || target_branch.starts_with('-') {
            bail!("task name and local target branch are required");
        }
        let (session, project) = {
            let mut state = self.task_state.lock();
            let session = state
                .sessions
                .iter_mut()
                .find(|s| s.id == session_id)
                .ok_or_else(|| anyhow!("session is unavailable"))?;
            self.task_store.hydrate(session)?;
            if session.is_busy() || session.active_turn_id().is_some() {
                bail!("finish the current turn before creating its task workspace");
            }
            let session = session.clone();
            let project = state
                .projects
                .iter()
                .find(|p| p.id == session.project_id)
                .ok_or_else(|| anyhow!("project is unavailable"))?
                .clone();
            if events.scoped_project.is_some_and(|id| id != project.id) {
                bail!("project is outside the steward scope");
            }
            (session, project)
        };
        if project.is_projectless() {
            bail!("non-Git tasks keep their existing working directory");
        }
        let repository = canonical_workspace(&project.path)?;
        git(
            &repository,
            &["check-ref-format", &format!("refs/heads/{target_branch}")],
        )?;
        let base = git(
            &repository,
            &[
                "rev-parse",
                "--verify",
                &format!("refs/heads/{target_branch}^{{commit}}"),
            ],
        )?;
        let mut managed = if let Some(existing) = session.managed_workspace.clone() {
            if existing.task_id != session_id
                || existing.name != name
                || existing.target_branch != target_branch
                || (!expected_commit.is_empty() && existing.base_commit != expected_commit)
            {
                bail!("task workspace already exists with different creation parameters");
            }
            if existing.ready {
                return Ok(ResponsePayload::TaskWorkspace { session });
            }
            existing
        } else {
            if !expected_commit.is_empty() && base != expected_commit {
                bail!("selected target branch moved; refresh its committed version");
            }
            let slug = name
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() {
                        c.to_ascii_lowercase()
                    } else {
                        '-'
                    }
                })
                .take(32)
                .collect::<String>();
            let branch = format!(
                "waku/task-{}-{}",
                slug.trim_matches('-'),
                &session_id.simple().to_string()[..8]
            );
            if git(
                &repository,
                &["show-ref", "--verify", &format!("refs/heads/{branch}")],
            )
            .is_ok()
            {
                bail!("task branch name is already used; existing branch was preserved");
            }
            let path = self
                .task_store
                .path()
                .parent()
                .ok_or_else(|| anyhow!("database directory unavailable"))?
                .join("task-worktrees")
                .join(session_id.to_string());
            if std::fs::symlink_metadata(&path).is_ok() {
                bail!("task workspace path is already used; existing directory was preserved");
            }
            let coordination = Some(ManagedWorkspaceLocation {
                created: false,
                path: path.with_file_name(format!("{session_id}-coordination")),
                branch: format!("{branch}-coordination"),
            });
            if let Some(location) = &coordination {
                if std::fs::symlink_metadata(&location.path).is_ok()
                    || git(
                        &repository,
                        &[
                            "show-ref",
                            "--verify",
                            &format!("refs/heads/{}", location.branch),
                        ],
                    )
                    .is_ok()
                {
                    bail!(
                        "coordination workspace name is already used; existing resources were preserved"
                    );
                }
            }
            ManagedWorkspace {
                created: false,
                deliveries: Vec::new(),
                results: Vec::new(),
                dependencies: Vec::new(),
                revision: 0,
                coordination,
                task_id: session_id,
                name,
                repository: repository.clone(),
                base_commit: base.clone(),
                target_branch,
                target_commit: base.clone(),
                integration_branch: branch.clone(),
                integration_commit: base.clone(),
                branch,
                path,
                owned: true,
                ready: false,
                error: None,
            }
        };
        self.save_task_workspace(session_id, managed.clone())?;
        let create = (|| -> anyhow::Result<()> {
            for coordination in [false, true] {
                let (path, branch, created) = if coordination {
                    let Some(location) = &managed.coordination else {
                        continue;
                    };
                    (
                        location.path.clone(),
                        location.branch.clone(),
                        location.created,
                    )
                } else {
                    (
                        managed.path.clone(),
                        managed.branch.clone(),
                        managed.created,
                    )
                };
                if std::fs::symlink_metadata(&path).is_ok() {
                    if !created {
                        bail!(
                            "workspace creation is unconfirmed; existing resources were preserved"
                        );
                    }
                    if canonical_workspace(&path)? != std::fs::canonicalize(&path)?
                        || git(&path, &["branch", "--show-current"])? != branch
                    {
                        bail!("recorded workspace branch changed; resources were preserved");
                    }
                } else {
                    if created {
                        bail!("a recorded workspace was removed; its resources were preserved");
                    }
                    std::fs::create_dir_all(path.parent().unwrap())?;
                    let path_text = path
                        .to_str()
                        .ok_or_else(|| anyhow!("workspace path is not UTF-8"))?;
                    git(
                        &repository,
                        &[
                            "worktree",
                            "add",
                            "-b",
                            &branch,
                            path_text,
                            &managed.base_commit,
                        ],
                    )?;
                    if coordination {
                        managed.coordination.as_mut().unwrap().created = true;
                    } else {
                        managed.created = true;
                    }
                    self.save_task_workspace(session_id, managed.clone())?;
                }
            }
            Ok(())
        })();
        managed.ready = create.is_ok();
        managed.error = create.err().map(|error| error.to_string());
        let session = self.save_task_workspace(session_id, managed)?;
        Ok(ResponsePayload::TaskWorkspace { session })
    }

    pub(super) fn save_task_workspace(
        &self,
        session_id: Uuid,
        mut managed: ManagedWorkspace,
    ) -> anyhow::Result<AgentSession> {
        let mut state = self.task_state.lock();
        let session = state
            .sessions
            .iter_mut()
            .find(|s| s.id == session_id)
            .ok_or_else(|| anyhow!("session was removed"))?;
        self.task_store.hydrate(session)?;
        managed.revision = session
            .managed_workspace
            .as_ref()
            .map_or(1, |old| old.revision + 1);
        if managed.ready {
            let (path, branch) = managed
                .coordination
                .as_ref()
                .map(|location| (&location.path, &location.branch))
                .unwrap_or((&managed.path, &managed.branch));
            session.workspace = SessionWorkspace::Worktree {
                path: path.clone(),
                branch: branch.clone(),
            };
        }
        session.managed_workspace = Some(managed);
        let result = session.clone();
        state.mark_session_dirty(session_id);
        self.task_store.save(&mut state).map_err(|error| {
            self.saving_failed.store(true, Ordering::Release);
            anyhow!(error)
        })?;
        Ok(result)
    }

    pub(super) fn check_workspace_writer(
        &self,
        session_id: Uuid,
        cwd: &Path,
    ) -> anyhow::Result<()> {
        let path = canonical_workspace(cwd)?;
        let active = self.sessions.lock().keys().copied().collect::<HashSet<_>>();
        let mut state = self.task_state.lock();
        let mut managed = false;
        if let Some(session) = state.sessions.iter_mut().find(|s| s.id == session_id) {
            self.task_store.hydrate(session)?;
            if let Some(resource) = &session.managed_workspace {
                if !resource.ready {
                    bail!("task workspace is not ready");
                }
                let expected = resource
                    .coordination
                    .as_ref()
                    .map_or(&resource.path, |location| &location.path);
                if canonical_workspace(expected)? != path {
                    bail!("managed runtime must use its recorded execution workspace");
                }
                managed = true;
            }
        }
        for session in &state.sessions {
            if let Some(resource) = &session.managed_workspace
                && resource.coordination.is_some()
                && canonical_workspace(&resource.path).is_ok_and(|reserved| reserved == path)
            {
                bail!("the integration workspace is reserved for daemon Git operations");
            }
        }
        for (id, other) in self.runtime_workspaces.lock().iter() {
            if *id != session_id
                && *other == path
                && (managed
                    || state
                        .sessions
                        .iter()
                        .any(|s| s.id == *id && s.managed_workspace.is_some()))
            {
                bail!(
                    "workspace is already owned by running session {id}; use an independent worktree"
                );
            }
        }
        for id in active.into_iter().filter(|id| *id != session_id) {
            let Some(index) = state.sessions.iter().position(|s| s.id == id) else {
                continue;
            };
            self.task_store.hydrate(&mut state.sessions[index])?;
            let session = &state.sessions[index];
            if !managed && session.managed_workspace.is_none() {
                continue;
            }
            let other = session.workspace.path().or_else(|| {
                state
                    .projects
                    .iter()
                    .find(|p| p.id == session.project_id)
                    .map(|p| p.path.as_path())
            });
            if other
                .is_some_and(|other| canonical_workspace(other).is_ok_and(|other| other == path))
            {
                bail!(
                    "workspace is already owned by running session {id}; use an independent worktree"
                );
            }
        }
        Ok(())
    }
}
