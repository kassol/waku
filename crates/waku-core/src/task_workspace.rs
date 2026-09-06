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
        bail!(
            "{}",
            if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            }
        );
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
            } => {
                self.deliver_task(session_id, commit, expected_target_commit, evidence)?;
                self.cleanup_task_resources(session_id, events, Some(session_id), true)?;
                let (session, _) = self.managed_task(session_id)?;
                Ok(ResponsePayload::TaskWorkspace { session })
            }
            StewardWorkspaceOperation::Cleanup { session_id: target } => {
                if target != session_id {
                    self.authorized_child(&mut self.task_state.lock(), session_id, target, events)?;
                }
                self.cleanup_task_resources(target, events, Some(session_id), true)?;
                let (session, _) = self.managed_task(target)?;
                Ok(ResponsePayload::TaskWorkspace { session })
            }
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
                drop(state);
                let mut session = session;
                if let Some(resource) = &mut session.managed_workspace {
                    if let Ok(commit) = git(
                        &resource.repository,
                        &["rev-parse", "--verify", &resource.integration_branch],
                    ) {
                        resource.integration_commit = commit;
                    }
                }
                events.ensure_steward_active()?;
                let state = self.task_state.lock();
                if !state.sessions.iter().any(|current| {
                    current.id == session.id
                        && current.project_id == session.project_id
                        && current.parent_session_id == session.parent_session_id
                }) {
                    bail!("session scope changed while inspecting its workspace");
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
                cleanup: Vec::new(),
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

    /// A leaf acquires an integration directory only when it starts delegating.
    /// Its existing runtime stays in the recorded coordination directory.
    pub(super) fn prepare_managed_delegation(
        &self,
        owner: Uuid,
        events: &EventSink,
    ) -> anyhow::Result<AgentSession> {
        let _workspace = self.workspace_start_gate.lock();
        let (session, mut task) = self.managed_task(owner)?;
        let (_, root) = self.managed_task(task.task_id)?;
        if root.deliveries.iter().any(|delivery| delivery.completed) {
            bail!("task has already been delivered; new child work requires a new task");
        }
        if task.coordination.is_some() && task.ready {
            return Ok(session);
        }
        if task.coordination.is_none() {
            if !task.ready {
                bail!("managed execution workspace is not ready");
            }
            let path = self
                .task_store
                .path()
                .parent()
                .ok_or_else(|| anyhow!("database directory unavailable"))?
                .join("task-worktrees")
                .join(format!("{owner}-integration"));
            let branch = format!("{}-integration", task.branch);
            if std::fs::symlink_metadata(&path).is_ok()
                || git(
                    &task.repository,
                    &["show-ref", "--verify", &format!("refs/heads/{branch}")],
                )
                .is_ok()
            {
                bail!("nested integration name is already used; existing resources were preserved");
            }
            let base = git(&task.path, &["rev-parse", "HEAD"])?;
            task.coordination = Some(ManagedWorkspaceLocation {
                path: task.path.clone(),
                branch: task.branch.clone(),
                created: task.created,
            });
            task.path = path;
            task.branch = branch.clone();
            task.integration_branch = branch;
            task.integration_commit = base;
            task.created = false;
            task.owned = true;
            task.ready = false;
            task.cleanup.clear();
            self.save_task_workspace(owner, task.clone())?;
        } else if task.task_id == owner {
            bail!("initial task workspace creation must finish before delegation");
        }
        let outcome = (|| -> anyhow::Result<()> {
            if task.created {
                if canonical_workspace(&task.path)? != std::fs::canonicalize(&task.path)?
                    || git(&task.path, &["branch", "--show-current"])? != task.branch
                {
                    bail!("nested integration identity changed; resources were preserved");
                }
                return Ok(());
            }
            if std::fs::symlink_metadata(&task.path).is_ok()
                || git(
                    &task.repository,
                    &[
                        "show-ref",
                        "--verify",
                        &format!("refs/heads/{}", task.branch),
                    ],
                )
                .is_ok()
            {
                bail!("unconfirmed nested integration resources were preserved");
            }
            std::fs::create_dir_all(
                task.path
                    .parent()
                    .ok_or_else(|| anyhow!("workspace parent unavailable"))?,
            )?;
            git(
                &task.repository,
                &[
                    "worktree",
                    "add",
                    "-b",
                    &task.branch,
                    task.path
                        .to_str()
                        .ok_or_else(|| anyhow!("workspace path is not UTF-8"))?,
                    &task.integration_commit,
                ],
            )?;
            task.created = true;
            self.save_task_workspace(owner, task.clone())?;
            Ok(())
        })();
        task.ready = outcome.is_ok();
        task.error = outcome.as_ref().err().map(ToString::to_string);
        let session = self.save_task_workspace(owner, task)?;
        events.task_workspace_saved(&session);
        outcome?;
        Ok(session)
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
        let runtime_paths = self.runtime_workspaces.lock().clone();
        let (expected, known, owners, reserved, fallback) = {
            let mut state = self.task_state.lock();
            if let Some(session) = state
                .sessions
                .iter_mut()
                .find(|session| session.id == session_id)
            {
                self.task_store.hydrate(session)?;
            }
            let selected = state
                .sessions
                .iter()
                .find(|session| session.id == session_id);
            let known = selected.map(|session| {
                (
                    session.project_id,
                    session
                        .managed_workspace
                        .as_ref()
                        .map(|resource| resource.revision),
                )
            });
            let expected = selected
                .and_then(|session| session.managed_workspace.as_ref())
                .map(|resource| {
                    (
                        resource.ready,
                        resource
                            .coordination
                            .as_ref()
                            .map_or(&resource.path, |location| &location.path)
                            .clone(),
                    )
                });
            let owners = state
                .sessions
                .iter()
                .filter(|session| session.managed_workspace.is_some())
                .map(|session| session.id)
                .collect::<HashSet<_>>();
            let reserved = state
                .sessions
                .iter()
                .filter_map(|session| session.managed_workspace.as_ref())
                .filter(|resource| resource.coordination.is_some())
                .map(|resource| resource.path.clone())
                .collect::<Vec<_>>();
            let fallback = state
                .sessions
                .iter()
                .filter(|session| {
                    session.id != session_id
                        && active.contains(&session.id)
                        && !runtime_paths.contains_key(&session.id)
                        && (expected.is_some() || session.managed_workspace.is_some())
                })
                .filter_map(|session| {
                    let path = session.workspace.path().or_else(|| {
                        state
                            .projects
                            .iter()
                            .find(|project| project.id == session.project_id)
                            .map(|project| project.path.as_path())
                    })?;
                    Some((session.id, path.to_path_buf()))
                })
                .collect::<Vec<_>>();
            (expected, known, owners, reserved, fallback)
        };
        if let Some((ready, expected)) = &expected {
            if !ready {
                bail!("task workspace is not ready");
            }
            if canonical_workspace(expected)? != path {
                bail!("managed runtime must use its recorded execution workspace");
            }
        }
        if reserved
            .iter()
            .any(|reserved| canonical_workspace(reserved).is_ok_and(|reserved| reserved == path))
        {
            bail!("the integration workspace is reserved for daemon Git operations");
        }
        for (id, other) in runtime_paths {
            if id != session_id && other == path && (expected.is_some() || owners.contains(&id)) {
                bail!(
                    "workspace is already owned by running session {id}; use an independent worktree"
                );
            }
        }
        for (id, other) in fallback {
            if canonical_workspace(&other).is_ok_and(|other| other == path) {
                bail!(
                    "workspace is already owned by running session {id}; use an independent worktree"
                );
            }
        }
        let current = self
            .task_state
            .lock()
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .map(|session| {
                (
                    session.project_id,
                    session
                        .managed_workspace
                        .as_ref()
                        .map(|resource| resource.revision),
                )
            });
        if current != known {
            bail!("session workspace changed before runtime startup");
        }
        Ok(())
    }
}
