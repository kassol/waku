//! Durable child creation through the shared provider pipeline.
use super::*;
use crate::persistence::CreationRecord;
use crate::protocol::{CreationStage, CreationWorkspace, ResponseOutcome};

fn failure(record: &CreationRecord, error: String, uncertain: bool) -> ResponsePayload {
    ResponsePayload::SessionCreationFailed {
        stage: record.stage,
        uncertain,
        session_id: record.session.as_ref().map(|session| session.id),
        workspace_path: record.workspace_path.clone(),
        branch: record.branch.clone(),
        error,
    }
}

pub(super) fn recover(store: &StateStore, state: &mut PersistedState) -> anyhow::Result<()> {
    for mut record in store.unfinished_creations()? {
        let error = format!(
            "Creation was interrupted during {:?}; no first prompt was resent. Check the retained session and workspace before starting more work.",
            record.stage
        );
        if let Some(id) = record.session.as_ref().map(|session| session.id)
            && let Some(session) = state.sessions.iter_mut().find(|session| session.id == id)
        {
            store.hydrate(session)?;
            session.finish_active_turn(crate::model::TurnStatus::Interrupted);
            session.status = SessionStatus::Failed;
            session.last_driver_error = Some(error.clone());
            session.push_message(crate::model::MessageRole::System, error.clone());
            state.mark_session_dirty(id);
            store.save(state)?;
        }
        record.outcome = Some(ResponseOutcome::Ok {
            payload: failure(&record, error, true),
        });
        store.save_creation(&record)?;
    }
    Ok(())
}

impl WakuBackend {
    fn persist_creation(&self, record: &CreationRecord) -> anyhow::Result<()> {
        self.task_store.save_creation(record).map_err(|error| {
            self.saving_failed.store(true, Ordering::Release);
            anyhow!(error)
        })
    }

    pub(super) fn create_session(
        &self,
        parent_id: Uuid,
        command: Command,
        events: EventSink,
    ) -> anyhow::Result<ResponsePayload> {
        let Command::CreateSession {
            provider,
            prompt,
            model,
            title,
            runtime_mode,
            idempotency_key,
            workspace,
        } = command.clone()
        else {
            unreachable!("creation accepts only CreateSession")
        };
        if !matches!(provider, ProviderKind::Claude | ProviderKind::Codex) {
            bail!("child creation supports Claude and Codex only");
        }
        if prompt.trim().is_empty() {
            bail!("a child session requires a nonempty prompt");
        }
        if idempotency_key
            .as_ref()
            .is_some_and(|key| key.trim().is_empty())
        {
            bail!("idempotency key must not be empty");
        }
        let creation_lock = idempotency_key.as_ref().map(|key| {
            let mut locks = self.creation_locks.lock();
            locks.retain(|_, lock| lock.strong_count() > 0);
            let slot = locks.entry((parent_id, key.clone())).or_default();
            if let Some(lock) = slot.upgrade() {
                lock
            } else {
                let lock = Arc::new(Mutex::new(()));
                *slot = Arc::downgrade(&lock);
                lock
            }
        });
        let _creation_lock = creation_lock.as_ref().map(|lock| lock.lock());
        events.ensure_steward_active()?;
        // Revalidate after waiting for this key, including retries of a cached outcome.
        let (parent, project) = {
            let mut state = self.task_state.lock();
            let parent = state
                .sessions
                .iter_mut()
                .find(|session| session.id == parent_id)
                .ok_or_else(|| anyhow!("the parent session is unavailable"))?;
            if events
                .scoped_project
                .is_some_and(|project| project != parent.project_id)
            {
                bail!("steward project is no longer available");
            }
            self.task_store.hydrate(parent)?;
            if !parent.has_started()
                || !matches!(parent.provider, ProviderKind::Claude | ProviderKind::Codex)
            {
                bail!("the parent must be an existing Claude or Codex session");
            }
            let parent = parent.clone();
            let project = state
                .projects
                .iter()
                .find(|project| project.id == parent.project_id)
                .ok_or_else(|| anyhow!("the parent project is unavailable"))?
                .clone();
            (parent, project)
        };
        let mode = runtime_mode.unwrap_or(parent.runtime_mode);
        validate_child_mode(parent.provider, provider, parent.runtime_mode, mode)?;
        let mut child = AgentSession::new(project.id, provider);
        child.parent_session_id = Some(parent.id);
        child.runtime_mode = mode;
        child.model = model.clone();
        child.set_title_from_prompt(&prompt);
        if let Some(title) = title {
            child.set_title(title);
        }
        let turn_id = child.begin_turn(prompt.clone());
        let message_id = child.messages.last().expect("begin_turn adds its input").id;
        child.status = SessionStatus::Connecting;
        let child_id = child.id;
        let record = CreationRecord {
            id: child_id,
            manager_session_id: parent_id,
            idempotency_key,
            command: command.clone(),
            project_id: project.id,
            session: Some(child.clone()),
            runtime_id: None,
            workspace_path: None,
            branch: None,
            stage: CreationStage::Workspace,
            outcome: None,
        };
        let (mut record, claimed) = self.task_store.claim_creation(record).map_err(|error| {
            self.saving_failed.store(true, Ordering::Release);
            error
        })?;
        if !claimed {
            if record.project_id != project.id {
                bail!("the previous creation belongs to a different steward project");
            }
            if serde_json::to_value(&record.command)? != serde_json::to_value(&command)? {
                bail!("idempotency key conflicts with a different creation request");
            }
            if let Some(child) = &record.session {
                validate_child_mode(parent.provider, child.provider, parent.runtime_mode, child.runtime_mode)?;
            }
            return match record.outcome {
                Some(ResponseOutcome::Ok { payload }) => Ok(payload),
                Some(ResponseOutcome::Error { error }) => Err(anyhow!(error.message)),
                None => Ok(failure(
                    &record,
                    "Previous creation is incomplete; its first prompt will not be repeated."
                        .into(),
                    true,
                )),
            };
        }
        let _creation = events.reserve_child(child_id);
        let mut runtime_events: Option<EventSink> = None;
        let result = (|| -> anyhow::Result<ResponsePayload> {
            let worktree_root = self
                .task_store
                .path()
                .parent()
                .ok_or_else(|| anyhow!("the task database has no workspace directory"))?
                .join("worktrees");
            let task_base = parent
                .managed_workspace
                .as_ref()
                .map(|task| {
                    if !task.ready {
                        bail!("task integration workspace is not ready");
                    }
                    task_workspace::git(
                        &task.repository,
                        &["rev-parse", "--verify", &task.integration_branch],
                    )
                })
                .transpose()?;
            match workspace {
                CreationWorkspace::Worktree => {
                    if project.is_projectless() {
                        bail!("child worktrees require a Git project");
                    }
                    // Distinct requests reserve distinct names even while another Git creation is in flight.
                    let name = format!("{} {prompt}", child_id.simple());
                    crate::worktree::create_in_before(
                        &project.path,
                        &worktree_root,
                        project.id,
                        child_id,
                        &name,
                        task_base.as_deref(),
                        |planned| {
                            child.workspace = crate::model::SessionWorkspace::Worktree {
                                path: planned.path.clone(),
                                branch: planned.branch.clone(),
                            };
                            if let Some(task) = &parent.managed_workspace {
                                let mut managed = task.clone();
                                managed.coordination = None;
                                managed.name = child.display_title().to_string();
                                managed.base_commit = task_base.clone().expect("managed task has a base");
                                managed.path = planned.path.clone();
                                managed.branch = planned.branch.clone();
                                managed.owned = true;
                                managed.ready = false;
                                managed.error = None;
                                child.managed_workspace = Some(managed);
                            }
                            record.workspace_path = Some(planned.path.clone());
                            record.branch = Some(planned.branch.clone());
                            record.session = Some(child.clone());
                            self.persist_creation(&record)
                        },
                    )?;
                }
                CreationWorkspace::Local => {
                    record.workspace_path = Some(project.path.clone());
                }
                CreationWorkspace::Inherit => {
                    if matches!(
                        parent.workspace,
                        crate::model::SessionWorkspace::NewWorktree { .. }
                    ) {
                        bail!("the parent workspace has not been created");
                    }
                    child.workspace = parent.workspace.clone();
                    record.workspace_path =
                        Some(parent.workspace.path().unwrap_or(&project.path).to_owned());
                    if let crate::model::SessionWorkspace::Worktree { branch, .. } =
                        &parent.workspace
                    {
                        record.branch = Some(branch.clone());
                    }
                }
            }
            let path = record
                .workspace_path
                .clone()
                .expect("workspace selection resolves a path");
            if !path.is_dir() {
                bail!(
                    "the requested workspace is not an existing directory: {}",
                    path.display()
                );
            }
            if let Some(task) = &parent.managed_workspace {
                let mut managed = task.clone();
                managed.coordination = None;
                managed.name = child.display_title().to_string();
                managed.base_commit = task_workspace::git(&path, &["rev-parse", "HEAD"])?;
                managed.path = path.clone();
                managed.branch = task_workspace::git(&path, &["branch", "--show-current"])?;
                managed.owned = workspace == CreationWorkspace::Worktree;
                managed.ready = true;
                managed.error = None;
                child.managed_workspace = Some(managed);
            }
            record.stage = CreationStage::SessionSave;
            record.session = Some(child.clone());
            self.persist_creation(&record)?;
            events.ensure_steward_active()?;
            {
                let mut state = self.task_state.lock();
                let current_parent = state
                    .sessions
                    .iter()
                    .find(|session| session.id == parent.id)
                    .ok_or_else(|| anyhow!("the parent was removed during workspace creation"))?;
                if current_parent.project_id != project.id
                    || !state
                        .projects
                        .iter()
                        .any(|current| current.id == project.id)
                {
                    bail!("the parent project changed during workspace creation");
                }
                validate_child_mode(current_parent.provider, provider, current_parent.runtime_mode, mode)?;
                state.push_session(child.clone());
                if let Err(error) = self.task_store.save(&mut state) {
                    self.saving_failed.store(true, Ordering::Release);
                    state.sessions.retain(|session| session.id != child_id);
                    return Err(error)
                        .context("could not save child; allocated workspace was retained");
                }
            }
            record.stage = CreationStage::ProviderStart;
            let child_runtime = Uuid::new_v4();
            record.runtime_id = Some(child_runtime);
            self.persist_creation(&record)?;
            let binary = self.provider_binary(provider)?;
            let (child_events, creation_started) = events.begin_child(child_id, child_runtime);
            runtime_events = Some(child_events.clone());
            self.handle_accepted(
                Request {
                    request_id: Uuid::new_v4(),
                    session_id: child_id,
                    runtime_id: child_runtime,
                    command: Command::Start {
                        options: crate::WireDriverStartOptions {
                            provider: waku_protocol::encode_enum(provider)?,
                            binary,
                            cwd: path.clone(),
                            mode: serde_json::to_value(mode)?.as_str().unwrap().to_owned(),
                            model,
                            reasoning_effort: None,
                            service_tier: None,
                            context_window: None,
                            agent_preset: None,
                            computer_use_enabled: false,
                            provider_cursor: None,
                        },
                    },
                },
                child_events.clone(),
            )?;
            record.stage = CreationStage::FirstPrompt;
            self.persist_creation(&record)?;
            self.handle_accepted(
                Request {
                    request_id: Uuid::new_v4(),
                    session_id: child_id,
                    runtime_id: child_runtime,
                    command: Command::Prompt {
                        prompt,
                        turn_id: Some(turn_id),
                        message_id: Some(message_id),
                    },
                },
                child_events,
            )?;
            creation_started
                .recv_timeout(std::time::Duration::from_secs(60))
                .context("timed out waiting for the child provider to accept its first prompt")?
                .map_err(anyhow::Error::msg)?;
            let session = self
                .task_state
                .lock()
                .sessions
                .iter()
                .find(|session| session.id == child_id)
                .cloned()
                .ok_or_else(|| anyhow!("the child was removed while starting"))?;
            Ok(ResponsePayload::SessionCreated {
                session,
                runtime_id: child_runtime,
                turn_id,
                workspace_path: path,
                branch: record.branch.clone(),
            })
        })();
        let payload = match result {
            Ok(payload) => payload,
            Err(error) => {
                let mut message = format!("{error:#}");
                if let Some(sink) = &runtime_events {
                    let saved = sink.send_batch(vec![
                        event_to_wire(DriverEvent::Error(message.clone()))?,
                        event_to_wire(DriverEvent::TurnFinished {
                            success: false,
                            summary: Some(message.clone()),
                        })?,
                    ]);
                    let stopped = self.close_runtime(child_id, record.runtime_id);
                    if saved.is_ok() && stopped.is_ok() {
                        sink.end_runtime();
                    }
                    if let Err(error) = saved {
                        message.push_str(&format!("; history save failed: {error:#}"));
                    }
                    if let Err(error) = stopped {
                        message.push_str(&format!("; provider stop failed: {error:#}"));
                    }
                } else {
                    let mut state = self.task_state.lock();
                    if let Some(session) = state
                        .sessions
                        .iter_mut()
                        .find(|session| session.id == child_id)
                    {
                        session.status = SessionStatus::Failed;
                        session.last_driver_error = Some(message.clone());
                        session.push_message(crate::model::MessageRole::Assistant, message.clone());
                        session.finish_active_turn(crate::model::TurnStatus::Failed);
                        state.mark_session_dirty(child_id);
                        if let Err(error) = self.task_store.save(&mut state) {
                            self.saving_failed.store(true, Ordering::Release);
                            message.push_str(&format!("; history save failed: {error:#}"));
                        }
                    }
                }
                failure(&record, message, record.stage == CreationStage::FirstPrompt)
            }
        };
        record.outcome = Some(ResponseOutcome::Ok {
            payload: payload.clone(),
        });
        if let Err(error) = self.persist_creation(&record) {
            if runtime_events.is_some() {
                let _ = self.close_runtime(child_id, record.runtime_id);
            }
            return Ok(failure(
                &record,
                format!(
                    "creation result could not be saved: {error:#}; check the retained session before more work"
                ),
                true,
            ));
        }
        Ok(payload)
    }
}
