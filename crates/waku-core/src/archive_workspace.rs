//! Global workspace requests have no session envelope; check their actual mutation targets.
use super::*;

impl WakuBackend {
    /// Caller holds workspace_start_gate through the subsequent workspace operation.
    pub(super) fn guard_archived_workspace(
        &self,
        operation: &WorkspaceOperation,
    ) -> anyhow::Result<()> {
        use WorkspaceOperation::*;
        let (path, target_session, reference) = match operation {
            WriteTextFile {
                root,
                relative_path,
                ..
            } => (Some(root.join(relative_path)), None, None),
            MigrateProjectlessWorkspace { path } => (Some(path.clone()), None, None),
            CheckoutBranch { cwd, .. }
            | GenerateCommitMessage { cwd, .. }
            | Commit { cwd, .. }
            | Push { cwd }
            | RestoreRef { cwd, .. } => (Some(cwd.clone()), None, None),
            CreateWorktree {
                project_path,
                session_id,
                ..
            } => (Some(project_path.clone()), Some(*session_id), None),
            CaptureTurnStart {
                cwd, session_id, ..
            }
            | CaptureTurn {
                cwd, session_id, ..
            }
            | DeleteTurnRefsAfter {
                cwd, session_id, ..
            }
            | DeleteSessionRefs { cwd, session_id } => (Some(cwd.clone()), Some(*session_id), None),
            CopySessionRefs {
                target_session_id, ..
            } => (None, Some(*target_session_id), None),
            CaptureRef { cwd, git_ref } | DeleteRef { cwd, git_ref } => {
                (Some(cwd.clone()), None, Some(git_ref.as_str()))
            }
            ListTree { .. }
            | BrowseDirectory { .. }
            | ReadTextFile { .. }
            | ListProjectFiles { .. }
            | DiscoverSlashCommands { .. }
            | CreateProjectlessWorkspace { .. }
            | InspectBranches { .. }
            | InspectCommit { .. }
            | HasRef { .. }
            | SessionTurnRefs { .. }
            | CollectReviewDiff { .. } => return Ok(()),
        };
        let resources = {
            let mut state = self.task_state.lock();
            let mut paths = Vec::new();
            for session in state.sessions.iter_mut().filter(|s| s.archived) {
                if target_session == Some(session.id)
                    || reference.is_some_and(|name| {
                        name.starts_with(&format!("refs/waku/session-{}-turn-", session.id))
                    })
                {
                    bail!(
                        "Archived session history is read-only; explicitly continue it before changing its workspace"
                    )
                }
                self.task_store.hydrate(session)?;
                if let Some(workspace) = &session.managed_workspace {
                    if workspace.owned && workspace.created {
                        paths.push(workspace.path.clone());
                    }
                    if workspace.owned
                        && let Some(location) = &workspace.coordination
                    {
                        if location.created {
                            paths.push(location.path.clone());
                        }
                    }
                }
            }
            paths
        };
        if resources.is_empty() {
            return Ok(());
        }
        let Some(path) = path else {
            return Ok(());
        };
        let target = canonical_mutation_target(&path)?;
        for resource in resources {
            let owned = match std::fs::canonicalize(&resource) {
                Ok(path) => path,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if target.starts_with(&owned)
                || (matches!(operation, MigrateProjectlessWorkspace { .. })
                    && owned.starts_with(&target))
            {
                bail!(
                    "Archived task-owned workspace is read-only: {}",
                    owned.display()
                )
            }
        }
        Ok(())
    }
}

// Writes may create a file. Resolve its existing ancestor so a symlink from an
// unrelated browser root cannot bypass the owned-workspace boundary.
fn canonical_mutation_target(path: &Path) -> anyhow::Result<PathBuf> {
    let mut existing = path;
    let mut suffix = Vec::new();
    loop {
        match std::fs::canonicalize(existing) {
            Ok(mut result) => {
                for name in suffix.into_iter().rev() {
                    result.push(name);
                }
                return Ok(result);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(
                    existing
                        .file_name()
                        .ok_or_else(|| anyhow!("Mutation path has no existing ancestor"))?,
                );
                existing = existing
                    .parent()
                    .ok_or_else(|| anyhow!("Mutation path has no existing ancestor"))?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}
