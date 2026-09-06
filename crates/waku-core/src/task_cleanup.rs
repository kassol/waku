//! Cleanup only task-owned, delivered resources; history and result refs survive.
use super::task_integration::is_ancestor;
use super::task_workspace::{canonical_workspace, git};
use super::*;
use crate::model::{
    BackgroundWorkEvent, BackgroundWorkKind, BackgroundWorkStatus, ManagedWorkspace,
    WorkspaceCleanup, WorkspaceCleanupStatus as Status,
};

impl WakuBackend {
    pub(super) fn track_cleanup_background(
        &self,
        session: Uuid,
        runtime: Uuid,
        event: &DriverEvent,
    ) {
        if !matches!(
            event,
            DriverEvent::ProcessExited | DriverEvent::BackgroundWork(_)
        ) {
            return;
        }
        let mut work = self.live_background.lock();
        if matches!(event, DriverEvent::ProcessExited) {
            work.remove(&(session, runtime));
            return;
        }
        let DriverEvent::BackgroundWork(event) = event else {
            return;
        };
        let keys = work.entry((session, runtime)).or_default();
        let live = |item: &crate::model::BackgroundWorkItem| {
            item.status.is_live() || item.status == BackgroundWorkStatus::Lost
        };
        match event {
            BackgroundWorkEvent::Upsert(item) => {
                if live(item) {
                    keys.insert(item.key.clone());
                } else {
                    keys.remove(&item.key);
                }
            }
            BackgroundWorkEvent::ReconcileLive { items } => {
                *keys = items
                    .iter()
                    .filter(|item| live(item))
                    .map(|item| item.key.clone())
                    .collect();
            }
            BackgroundWorkEvent::ReconcileProcesses { items } => {
                keys.retain(|key| key.kind != BackgroundWorkKind::Process);
                keys.extend(
                    items
                        .iter()
                        .filter(|item| live(item))
                        .map(|item| item.key.clone()),
                );
            }
            BackgroundWorkEvent::StopRequested(key)
            | BackgroundWorkEvent::StopFailed { key, .. } => {
                keys.insert(key.clone());
            }
            BackgroundWorkEvent::OutputDelta { .. } => {}
        }
    }

    pub(super) fn cleanup_delivered_tasks(&self, events: &EventSink) {
        if self.quitting.load(Ordering::Acquire) {
            return;
        }
        let _work = self.work_gate.read();
        if self.quitting.load(Ordering::Acquire) {
            return;
        }
        let tasks = {
            let state = self.task_state.lock();
            let delivered = state
                .sessions
                .iter()
                .filter_map(|session| {
                    session
                        .managed_workspace
                        .as_ref()
                        .filter(|task| {
                            task.task_id == session.id
                                && task.deliveries.iter().any(|item| item.completed)
                        })
                        .map(|_| session.id)
                })
                .collect::<HashSet<_>>();
            state
                .sessions
                .iter()
                .filter(|session| {
                    !session.is_busy()
                        && session.active_turn_id().is_none()
                        && session.pending_permission.is_none()
                        && session.pending_user_input.is_none()
                        && session.queued_messages.is_empty()
                        && session.steward_wait.is_none()
                        && session.managed_workspace.as_ref().is_some_and(|task| {
                            delivered.contains(&task.task_id)
                                && (task.cleanup.len()
                                    < if task.coordination.is_some() { 2 } else { 1 }
                                    || task.cleanup.iter().any(|item| {
                                        matches!(item.status, Status::Ready | Status::Waiting)
                                    }))
                        })
                })
                .map(|session| session.id)
                .collect::<Vec<_>>()
        };
        for task in tasks {
            let _ = self.cleanup_task_resources(task, events, None, false);
        }
    }

    pub(super) fn cleanup_task_resources(
        &self,
        owner: Uuid,
        events: &EventSink,
        reserved: Option<Uuid>,
        retry: bool,
    ) -> anyhow::Result<()> {
        if self.quitting.load(Ordering::Acquire) {
            return Ok(());
        }
        let (_, resource) = self.managed_task(owner)?;
        let (_, task) = self.managed_task(resource.task_id)?;
        let Some(delivery) = task.deliveries.iter().find(|item| item.completed) else {
            return Ok(());
        };
        if git(
            &task.repository,
            &["rev-parse", "--verify", &delivery.reference],
        )? != delivery.commit
        {
            bail!("retained delivery reference changed; resources were preserved");
        }
        let ids = self
            .task_state
            .lock()
            .sessions
            .iter()
            .filter(|session| {
                (session.id == owner || session.parent_session_id == Some(owner))
                    && session
                        .managed_workspace
                        .as_ref()
                        .is_some_and(|item| item.task_id == task.task_id)
            })
            .map(|session| session.id)
            .collect::<Vec<_>>();
        for id in ids {
            let _reservation = if reserved == Some(id) {
                None
            } else {
                let Ok(guard) = events.reserve_steward_target(id) else {
                    continue;
                };
                Some(guard)
            };
            let _workspace = self.workspace_start_gate.lock();
            let (session, mut resource) = self.managed_task(id)?;
            let mut locations = vec![(
                resource.path.clone(),
                resource.branch.clone(),
                resource.created,
            )];
            if let Some(location) = &resource.coordination {
                locations.push((
                    location.path.clone(),
                    location.branch.clone(),
                    location.created,
                ));
            }
            for (path, branch, created) in locations {
                let previous = resource
                    .cleanup
                    .iter()
                    .find(|item| item.path == path)
                    .cloned();
                if previous.as_ref().is_some_and(|item| {
                    item.status == Status::Removed || (!retry && item.status == Status::Retained)
                }) {
                    continue;
                }
                let mut cleanup = previous.clone().unwrap_or(WorkspaceCleanup {
                    path,
                    branch,
                    commit: None,
                    status: Status::Ready,
                    reason: None,
                });
                let outcome = self.cleanup_resource(
                    &session,
                    &mut resource,
                    &mut cleanup,
                    created,
                    &delivery.reference,
                );
                if let Err(error) = outcome {
                    cleanup.reason = Some(error.to_string());
                    if cleanup.status != Status::Waiting {
                        cleanup.status = Status::Retained;
                    }
                }
                if previous.as_ref() != Some(&cleanup) {
                    Self::record_cleanup(&mut resource, cleanup);
                    let saved = self.save_task_workspace(id, resource.clone())?;
                    events.task_workspace_saved(&saved);
                }
            }
        }
        Ok(())
    }

    fn record_cleanup(resource: &mut ManagedWorkspace, cleanup: WorkspaceCleanup) {
        if let Some(existing) = resource
            .cleanup
            .iter_mut()
            .find(|item| item.path == cleanup.path)
        {
            *existing = cleanup;
        } else {
            resource.cleanup.push(cleanup);
        }
    }

    fn cleanup_resource(
        &self,
        session: &AgentSession,
        resource: &mut ManagedWorkspace,
        cleanup: &mut WorkspaceCleanup,
        created: bool,
        retained: &str,
    ) -> anyhow::Result<()> {
        cleanup.status = Status::Ready;
        cleanup.reason = None;
        if !resource.owned || !created {
            bail!("resource was not created and owned by this task");
        }
        // Check provider state before any filesystem work. Completion can leave a
        // reusable process; background work and user queues still require it.
        let execution = resource
            .coordination
            .as_ref()
            .map_or(&resource.path, |item| &item.path)
            .clone();
        if execution == cleanup.path {
            let has_background =
                self.sessions
                    .lock()
                    .get(&session.id)
                    .is_some_and(|(runtime, _)| {
                        self.live_background
                            .lock()
                            .get(&(session.id, *runtime))
                            .is_some_and(|keys| !keys.is_empty())
                    });
            if session.active_turn_id().is_some()
                || session.is_busy()
                || session.pending_permission.is_some()
                || session.pending_user_input.is_some()
                || !session.queued_messages.is_empty()
                || session.steward_wait.is_some()
                || session.history_save_error.is_some()
                || has_background
            {
                cleanup.status = Status::Waiting;
                bail!("session has active work, user input, unsaved history or background work");
            }
        }
        let reference = format!("refs/heads/{}", cleanup.branch);
        let branch_commit = git(&resource.repository, &["rev-parse", "--verify", &reference]);
        let exists = std::fs::symlink_metadata(&cleanup.path).is_ok();
        if !exists {
            if cleanup.commit.is_none() {
                bail!("owned directory disappeared before cleanup was recorded");
            }
        } else {
            let physical = std::fs::canonicalize(&cleanup.path)?;
            if std::fs::symlink_metadata(&cleanup.path)?
                .file_type()
                .is_symlink()
                || canonical_workspace(&cleanup.path)? != physical
            {
                bail!("workspace identity changed; directory was preserved");
            }
            let common = |path: &Path| -> anyhow::Result<PathBuf> {
                let value = git(
                    path,
                    &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                )?;
                Ok(std::fs::canonicalize(value)?)
            };
            if common(&cleanup.path)? != common(&resource.repository)? {
                bail!("workspace belongs to another repository");
            }
            if git(&cleanup.path, &["branch", "--show-current"])? != cleanup.branch {
                bail!("workspace branch changed; directory was preserved");
            }
            // Ignore rules do not authorize discarding a user's files.
            if !git(
                &cleanup.path,
                &[
                    "status",
                    "--porcelain=v1",
                    "--untracked-files=all",
                    "--ignored",
                ],
            )?
            .is_empty()
            {
                bail!("workspace contains modified, untracked or ignored files");
            }
            let shared = {
                let state = self.task_state.lock();
                state.sessions.iter().any(|other| {
                    if other.id == session.id {
                        return false;
                    }
                    let path = match &other.workspace {
                        crate::model::SessionWorkspace::Worktree { path, .. } => Some(path),
                        crate::model::SessionWorkspace::Local => state
                            .projects
                            .iter()
                            .find(|project| project.id == other.project_id)
                            .map(|project| &project.path),
                        crate::model::SessionWorkspace::NewWorktree { .. } => None,
                    };
                    path.is_some_and(|path| {
                        std::fs::canonicalize(path).is_ok_and(|path| path == physical)
                    })
                })
            };
            if shared {
                bail!("another session still references this workspace");
            }
            if self
                .terminal_workspaces
                .lock()
                .values()
                .any(|path| path == &physical)
            {
                bail!("a terminal still uses this workspace");
            }
            for (owner, path) in self.runtime_workspaces.lock().iter() {
                if path == &physical && *owner != session.id {
                    bail!("another runtime still uses this workspace");
                }
            }
        }
        if let Ok(commit) = &branch_commit {
            if cleanup
                .commit
                .as_ref()
                .is_some_and(|expected| expected != commit)
            {
                bail!("branch moved after cleanup was prepared");
            }
            if !is_ancestor(&resource.repository, commit, retained) {
                bail!("resource contains commits absent from the retained delivery");
            }
            if resource.coordination.is_none()
                && !resource
                    .results
                    .iter()
                    .any(|result| result.commit == *commit && result.integration_commit.is_some())
            {
                bail!("child head has not been accepted and integrated");
            }
            cleanup.commit = Some(commit.clone());
        } else if exists || cleanup.commit.is_none() {
            bail!("owned branch disappeared before cleanup was prepared");
        }
        // Persist the fixed branch head before the first destructive operation.
        Self::record_cleanup(resource, cleanup.clone());
        self.save_task_workspace(session.id, resource.clone())?;
        if execution == cleanup.path {
            self.close_runtime(session.id, None)?;
        }
        if exists {
            self.check_clean_workspace(&cleanup.path, &cleanup.branch)?;
            if !git(
                &cleanup.path,
                &[
                    "status",
                    "--porcelain=v1",
                    "--untracked-files=all",
                    "--ignored",
                ],
            )?
            .is_empty()
            {
                bail!("workspace changed while its idle runtime was closing");
            }
            git(
                &resource.repository,
                &[
                    "worktree",
                    "remove",
                    cleanup
                        .path
                        .to_str()
                        .ok_or_else(|| anyhow!("workspace path is not UTF-8"))?,
                ],
            )?;
        }
        if let Ok(expected) = branch_commit {
            if git(&resource.repository, &["rev-parse", "--verify", &reference])? != expected {
                bail!("branch moved before removal; the branch was preserved");
            }
            git(&resource.repository, &["branch", "-d", &cleanup.branch])?;
        }
        cleanup.status = Status::Removed;
        cleanup.reason = None;
        Ok(())
    }
}
