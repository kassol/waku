//! Acceptance is bound to immutable commits; execution completion is independent.
use super::task_workspace::{canonical_workspace, git};
use super::*;
use crate::model::{ManagedWorkspace, WorkspaceDelivery, WorkspaceEvidence, TaskResult};

pub(super) fn fixed_commit(repository: &Path, commit: &str) -> anyhow::Result<String> {
    if !matches!(commit.len(), 40 | 64) || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("a full immutable commit ID is required");
    }
    let actual = git(
        repository,
        &["rev-parse", "--verify", &format!("{commit}^{{commit}}")],
    )?;
    if actual != commit {
        bail!("commit does not match its canonical ID");
    }
    Ok(actual)
}

pub(super) fn is_ancestor(repository: &Path, commit: &str, target: &str) -> bool {
    git(repository, &["merge-base", "--is-ancestor", commit, target]).is_ok()
}

pub(super) fn check_evidence(commit: &str, evidence: &[WorkspaceEvidence]) -> anyhow::Result<()> {
    if evidence.is_empty()
        || evidence.iter().any(|item| {
            item.commit != commit
                || item.checks.trim().is_empty()
                || item.environment.trim().is_empty()
                || item.reviewer.trim().is_empty()
        })
    {
        bail!("acceptance evidence must name the fixed commit, checks, environment and reviewer");
    }
    Ok(())
}

impl WakuBackend {
    pub(super) fn check_task_dependencies(
        &self,
        parent: Uuid,
        dependencies: &[crate::model::WorkspaceDependency],
        base: &str,
        events: &EventSink,
    ) -> anyhow::Result<()> {
        let (_, task) = self.managed_task(parent)?;
        let mut state = self.task_state.lock();
        for dependency in dependencies {
            let child = self
                .authorized_child(&mut state, parent, dependency.session_id, events)?
                .0;
            let resource = child
                .managed_workspace
                .as_ref()
                .ok_or_else(|| anyhow!("dependency has no managed result"))?;
            if resource.task_id != task.task_id {
                bail!("dependency belongs to another task");
            }
            let result = resource
                .results
                .iter()
                .find(|result| result.commit == dependency.commit)
                .and_then(|result| result.integration_commit.as_ref())
                .ok_or_else(|| {
                    anyhow!(
                        "dependency has not been accepted and integrated at the requested commit"
                    )
                })?;
            if !is_ancestor(&task.repository, result, base)
                || !is_ancestor(&task.repository, &dependency.commit, base)
            {
                bail!("dependency is absent from the selected integration commit");
            }
        }
        Ok(())
    }

    pub(super) fn managed_task(
        &self,
        id: Uuid,
    ) -> anyhow::Result<(AgentSession, ManagedWorkspace)> {
        let mut state = self.task_state.lock();
        let session = state
            .sessions
            .iter_mut()
            .find(|session| session.id == id)
            .ok_or_else(|| anyhow!("session is unavailable"))?;
        self.task_store.hydrate(session)?;
        let workspace = session
            .managed_workspace
            .clone()
            .ok_or_else(|| anyhow!("session has no managed workspace"))?;
        Ok((session.clone(), workspace))
    }

    pub(super) fn check_workspace_idle(&self, path: &Path) -> anyhow::Result<()> {
        let path = canonical_workspace(path)?;
        for (owner, cwd) in self.runtime_workspaces.lock().iter() {
            if *cwd == path {
                bail!("workspace is in use by session {owner}");
            }
        }
        for (owner, cwd) in self.terminal_workspaces.lock().iter() {
            if *cwd == path {
                bail!("workspace is in use by terminal {owner}");
            }
        }
        Ok(())
    }

    pub(super) fn check_clean_workspace(&self, path: &Path, branch: &str) -> anyhow::Result<()> {
        self.check_workspace_idle(path)?;
        if git(path, &["branch", "--show-current"])? != branch {
            bail!("workspace branch changed; the directory was preserved");
        }
        if !git(path, &["status", "--porcelain=v1", "--untracked-files=all"])?.is_empty() {
            bail!("workspace contains changes or untracked files; the directory was preserved");
        }
        for operation in ["MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD"] {
            if git(path, &["rev-parse", "--verify", operation]).is_ok() {
                bail!("a Git operation is in progress; the directory was preserved");
            }
        }
        Ok(())
    }

    pub(super) fn integrate_task_result(
        &self,
        parent: Uuid,
        child: Uuid,
        commit: String,
        expected: String,
        evidence: Vec<WorkspaceEvidence>,
        events: &EventSink,
    ) -> anyhow::Result<ResponsePayload> {
        let _child = events.reserve_steward_target(child)?;
        let _workspace = self.workspace_start_gate.lock();
        let (_, mut task) = self.managed_task(parent)?;
        if task.task_id != parent || task.coordination.is_none() || !task.ready {
            bail!("integration requires this task's coordinating session");
        }
        let child_session = self
            .authorized_child(&mut self.task_state.lock(), parent, child, events)?
            .0;
        let mut resource = child_session
            .managed_workspace
            .clone()
            .ok_or_else(|| anyhow!("child has no managed result workspace"))?;
        if resource.task_id != parent {
            bail!("child workspace belongs to another task");
        }
        fixed_commit(&task.repository, &commit)?;
        fixed_commit(&task.repository, &expected)?;
        check_evidence(&commit, &evidence)?;
        let existing = resource
            .results
            .iter()
            .position(|result| result.commit == commit);
        if let Some(index) = existing {
            let result = &resource.results[index];
            if result.evidence != evidence {
                bail!("this commit already has different acceptance parameters");
            }
            if let Some(integrated) = &result.integration_commit {
                let retained = task
                    .deliveries
                    .iter()
                    .find(|delivery| delivery.completed)
                    .map_or(task.integration_branch.as_str(), |delivery| {
                        delivery.reference.as_str()
                    });
                if !is_ancestor(&task.repository, integrated, retained) {
                    bail!("recorded integrated result is no longer on the integration branch");
                }
                return Ok(ResponsePayload::TaskWorkspace {
                    session: child_session,
                });
            }
        }
        if task.deliveries.iter().any(|delivery| delivery.completed) {
            bail!("task has already been delivered; start a new task for more work");
        }
        if !is_ancestor(&task.repository, &resource.base_commit, &commit)
            || !is_ancestor(&task.repository, &commit, &resource.branch)
        {
            bail!("result commit is outside the recorded child branch and base");
        }
        if let Some(index) = existing {
            resource.results[index].expected_integration_commit = expected.clone();
            resource.error = None;
            self.save_task_workspace(child, resource.clone())?;
        } else {
            resource.results.push(TaskResult {
                commit: commit.clone(),
                owner: child,
                evidence,
                expected_integration_commit: expected.clone(),
                integration_commit: None,
            });
            resource.error = None;
            self.save_task_workspace(child, resource.clone())?;
        }
        let outcome = (|| -> anyhow::Result<String> {
            self.check_clean_workspace(&task.path, &task.integration_branch)?;
            let head = git(&task.path, &["rev-parse", "HEAD"])?;
            // Recover the saved intent after a completed Git merge and a lost response/save.
            if existing.is_some()
                && is_ancestor(&task.repository, &commit, &head)
                && is_ancestor(&task.repository, &expected, &head)
            {
                return Ok(head);
            }
            if head != expected {
                bail!("integration branch moved; refresh the fixed target commit");
            }
            git(
                &task.path,
                &[
                    "-c",
                    "user.name=Waku",
                    "-c",
                    "user.email=waku@localhost",
                    "merge",
                    "--no-edit",
                    &commit,
                ],
            )?;
            git(&task.path, &["rev-parse", "HEAD"])
        })();
        match outcome {
            Ok(integrated) => {
                task.integration_commit = integrated.clone();
                self.save_task_workspace(parent, task)?;
                resource
                    .results
                    .iter_mut()
                    .find(|result| result.commit == commit)
                    .unwrap()
                    .integration_commit = Some(integrated);
                resource.error = None;
            }
            Err(error) => resource.error = Some(error.to_string()),
        }
        let session = self.save_task_workspace(child, resource)?;
        Ok(ResponsePayload::TaskWorkspace { session })
    }
}

fn target_workspaces(repository: &Path, target: &str) -> anyhow::Result<Vec<PathBuf>> {
    let listing = git(repository, &["worktree", "list", "--porcelain", "-z"])?;
    let reference = format!("branch refs/heads/{target}");
    let mut path = None;
    let mut paths = Vec::new();
    for field in listing.split('\0') {
        if let Some(directory) = field.strip_prefix("worktree ") {
            path = Some(PathBuf::from(directory));
        } else if field == reference {
            if let Some(path) = &path {
                paths.push(path.clone());
            }
        } else if field.is_empty() {
            path = None;
        }
    }
    Ok(paths)
}

impl WakuBackend {
    pub(super) fn deliver_task(
        &self,
        parent: Uuid,
        commit: String,
        expected: String,
        evidence: Vec<WorkspaceEvidence>,
    ) -> anyhow::Result<ResponsePayload> {
        let _workspace = self.workspace_start_gate.lock();
        let (session, mut task) = self.managed_task(parent)?;
        if task.task_id != parent || task.coordination.is_none() || !task.ready {
            bail!("delivery requires this task's coordinating session");
        }
        fixed_commit(&task.repository, &commit)?;
        fixed_commit(&task.repository, &expected)?;
        check_evidence(&commit, &evidence)?;
        let reference = format!("refs/waku/tasks/{parent}/deliveries/{commit}");
        let existing = task
            .deliveries
            .iter()
            .position(|delivery| delivery.commit == commit);
        if let Some(index) = existing {
            let delivery = &task.deliveries[index];
            if delivery.evidence != evidence {
                bail!("this commit already has different overall acceptance evidence");
            }
            if delivery.completed {
                if git(
                    &task.repository,
                    &["rev-parse", "--verify", &delivery.reference],
                )? != commit
                {
                    bail!("the retained delivery reference changed; resources were preserved");
                }
                return Ok(ResponsePayload::TaskWorkspace { session });
            }
        }
        if task.deliveries.iter().any(|delivery| delivery.completed) {
            bail!("task has already been delivered; start a new task for more work");
        }
        let index = if let Some(index) = existing {
            task.deliveries[index].previous_target_commit = expected.clone();
            task.deliveries[index].error = None;
            index
        } else {
            task.deliveries.push(WorkspaceDelivery {
                commit: commit.clone(),
                reference: reference.clone(),
                target_branch: task.target_branch.clone(),
                previous_target_commit: expected.clone(),
                evidence,
                completed: false,
                error: None,
            });
            task.deliveries.len() - 1
        };
        task.error = None;
        self.save_task_workspace(parent, task.clone())?;
        let outcome = (|| -> anyhow::Result<()> {
            self.check_clean_workspace(&task.path, &task.integration_branch)?;
            if git(&task.path, &["rev-parse", "HEAD"])? != commit {
                bail!("integration branch moved since the overall acceptance");
            }
            let target_ref = format!("refs/heads/{}", task.target_branch);
            let target = git(&task.repository, &["rev-parse", "--verify", &target_ref])?;
            // A durable reference plus containment recognizes a lost successful response.
            if git(&task.repository, &["rev-parse", "--verify", &reference])
                .is_ok_and(|saved| saved == commit)
                && is_ancestor(&task.repository, &commit, &target)
            {
                return Ok(());
            }
            if target != expected {
                bail!("delivery target moved; refresh and review its committed version");
            }
            if !is_ancestor(&task.repository, &target, &commit) {
                bail!("delivery requires integration of the current target; no branch was reset");
            }
            let workspaces = target_workspaces(&task.repository, &task.target_branch)?;
            if workspaces.len() > 1 {
                bail!("delivery target has multiple checked-out workspaces");
            }
            for path in &workspaces {
                self.check_clean_workspace(path, &task.target_branch)?;
            }
            if let Ok(saved) = git(&task.repository, &["rev-parse", "--verify", &reference]) {
                if saved != commit {
                    bail!("delivery reference already names another result");
                }
            } else {
                git(
                    &task.repository,
                    &["update-ref", &reference, &commit, &"0".repeat(commit.len())],
                )?;
            }
            if let Some(path) = workspaces.first() {
                git(path, &["merge", "--ff-only", "--no-edit", &commit])?;
            } else {
                git(
                    &task.repository,
                    &["update-ref", &target_ref, &commit, &expected],
                )?;
            }
            if !is_ancestor(&task.repository, &commit, &target_ref) {
                bail!(
                    "delivery target changed before confirmation; the retained result was preserved"
                );
            }
            Ok(())
        })();
        match outcome {
            Ok(()) => {
                task.deliveries[index].completed = true;
                task.integration_commit = commit;
                task.error = None;
            }
            Err(error) => {
                task.deliveries[index].error = Some(error.to_string());
                task.error = Some(error.to_string());
            }
        }
        let session = self.save_task_workspace(parent, task)?;
        Ok(ResponsePayload::TaskWorkspace { session })
    }
}
