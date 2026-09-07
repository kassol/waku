//! Read-only validation of the archived execution worktree before explicit continuation.
use super::task_workspace::{canonical_workspace, git};
use super::*;
use crate::model::{SessionWorkspace, WorkspaceCleanupStatus};

impl WakuBackend {
    /// Caller holds workspace_start_gate, then task_state. This method never
    /// reacquires either lock and never creates or repairs a workspace.
    pub(super) fn continuation_workspace(
        &self,
        state: &mut PersistedState,
        manager: Uuid,
        child: &AgentSession,
    ) -> anyhow::Result<bool> {
        let parent = state
            .sessions
            .iter_mut()
            .find(|s| s.id == manager)
            .ok_or_else(|| anyhow!("Continuation manager is unavailable"))?;
        self.task_store.hydrate(parent)?;
        if child.parent_session_id != Some(manager)
            || parent.project_id != child.project_id
            || parent.archived
            || parent.cancellation_requested_turn_id.is_some()
            || parent.history_save_error.is_some()
        {
            bail!("Continuation task relationship or manager state changed")
        }
        validate_child_mode(
            parent.provider,
            child.provider,
            parent.runtime_mode,
            child.runtime_mode,
        )?;
        let resource = child
            .managed_workspace
            .as_ref()
            .ok_or_else(|| anyhow!("Only recorded task-owned worktrees can be continued"))?;
        if !resource.owned
            || parent
                .managed_workspace
                .as_ref()
                .is_none_or(|p| p.task_id != resource.task_id)
        {
            bail!("Continuation workspace is not owned by this task")
        }
        let root = state
            .sessions
            .iter_mut()
            .find(|s| s.id == resource.task_id)
            .ok_or_else(|| anyhow!("Original task is unavailable"))?;
        self.task_store.hydrate(root)?;
        let root_resource = root
            .managed_workspace
            .as_ref()
            .ok_or_else(|| anyhow!("Original task workspace is unavailable"))?;
        if root.project_id != child.project_id
            || root.archived
            || root_resource
                .deliveries
                .iter()
                .any(|delivery| delivery.completed)
            || resource
                .deliveries
                .iter()
                .any(|delivery| delivery.completed)
        {
            bail!("Original task is closed or delivered; continuation requires a new task")
        }
        let (path, branch, created) = resource
            .coordination
            .as_ref()
            .map(|location| (&location.path, &location.branch, location.created))
            .unwrap_or((&resource.path, &resource.branch, resource.created));
        if !created {
            bail!("Original execution workspace creation was never confirmed")
        }
        let SessionWorkspace::Worktree {
            path: recorded,
            branch: recorded_branch,
        } = &child.workspace
        else {
            bail!("Continuation cannot fall back to the original checkout")
        };
        if recorded != path || recorded_branch != branch {
            bail!("Recorded execution workspace identity changed")
        }
        let common = |cwd: &Path| -> anyhow::Result<PathBuf> {
            Ok(std::fs::canonicalize(git(
                cwd,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )?)?)
        };
        let repository_common = common(&resource.repository)?;
        if repository_common != common(&root_resource.repository)? {
            bail!("Recorded workspace belongs to another repository")
        }
        if self
            .runtime_workspaces
            .lock()
            .values()
            .any(|active| active == path)
            || self
                .terminal_workspaces
                .lock()
                .values()
                .any(|active| active == path)
        {
            bail!("Original execution workspace still has an active consumer")
        }
        let cleanup = resource.cleanup.iter().find(|entry| entry.path == *path);
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if cleanup.is_some_and(|entry| entry.status == WorkspaceCleanupStatus::Removed)
            || metadata.file_type().is_symlink()
            || !metadata.is_dir()
        {
            bail!("Removed or replaced execution workspace was preserved; it cannot be resumed")
        }
        if !resource.ready || resource.error.is_some() {
            bail!("Original workspace requires inspection before continuation")
        }
        let physical = std::fs::canonicalize(path)?;
        if canonical_workspace(path)? != physical || !path.join(".git").is_file() {
            bail!("Execution path is not its original linked worktree")
        }
        if common(path)? != repository_common {
            bail!("Execution worktree belongs to another repository")
        }
        let registered = git(
            &resource.repository,
            &["worktree", "list", "--porcelain", "-z"],
        )?;
        let branch_ref = format!("branch refs/heads/{branch}");
        let registered = registered.split("\0\0").any(|entry| {
            let mut fields = entry.split('\0');
            fields
                .next()
                .and_then(|field| field.strip_prefix("worktree "))
                .is_some_and(|registered| {
                    std::fs::canonicalize(registered).is_ok_and(|p| p == physical)
                })
                && fields.any(|field| field == branch_ref)
        });
        if !registered {
            bail!("Execution worktree is not registered with its recorded branch")
        }
        self.check_clean_workspace(path, branch)?;
        let head = git(path, &["rev-parse", "--verify", "HEAD"])?;
        let expected = cleanup.and_then(|entry| entry.commit.as_ref()).or_else(|| {
            resource
                .coordination
                .is_none()
                .then(|| resource.results.last().map(|r| &r.commit))
                .flatten()
        });
        if expected.is_some_and(|expected| expected != &head) {
            bail!("Execution branch moved after its saved result; existing work was preserved")
        }
        let active = self.sessions.lock().keys().copied().collect::<HashSet<_>>();
        for other in &state.sessions {
            if other.id == child.id {
                continue;
            }
            if let Some(workspace) = &other.managed_workspace {
                if workspace.coordination.is_some()
                    && std::fs::canonicalize(&workspace.path).is_ok_and(|p| p == physical)
                {
                    bail!("Execution workspace is reserved for task integration")
                }
            }
            let other_path = other.workspace.path().or_else(|| {
                active
                    .contains(&other.id)
                    .then(|| {
                        state
                            .projects
                            .iter()
                            .find(|p| p.id == other.project_id)
                            .map(|p| p.path.as_path())
                    })
                    .flatten()
            });
            if other_path.is_some_and(|p| std::fs::canonicalize(p).is_ok_and(|p| p == physical)) {
                bail!("Another session still references the execution worktree")
            }
        }
        Ok(true)
    }
}
