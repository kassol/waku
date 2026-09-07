import { describe, expect, test } from 'bun:test'

import { reduceRuntimeEvent, reduceRuntimeEventAfterPersistence } from './event-reducer'
import type { AgentSession, SequencedEvent } from './generated'

const clock = {
  nowSeconds: () => 200,
  nowMillis: () => 200_000,
  randomUUID: (() => {
    let id = 0
    return () => `00000000-0000-4000-8000-${String(++id).padStart(12, '0')}`
  })(),
}

const SUBMISSION = {
  message: 'Second prompt',
  turnId: '10000000-0000-4000-8000-000000000002',
  messageId: '20000000-0000-4000-8000-000000000002',
}

describe('promptSubmitted', () => {
  test('a client following the runtime mirrors another client’s submission under its ids', () => {
    // The desktop stayed attached to the idle runtime after the first turn;
    // the phone then submitted the second prompt.
    const result = reduceRuntimeEvent(idleSession(), event('promptSubmitted', SUBMISSION), clock)
    const session = result.session

    expect(session.status).toBe('connecting')
    expect(session.turns).toHaveLength(2)
    expect(session.turns.at(-1)).toMatchObject({
      id: SUBMISSION.turnId,
      turn_count: 2,
      status: 'running',
      provider_turn_started: false,
    })
    expect(session.messages.at(-1)).toMatchObject({
      id: SUBMISSION.messageId,
      turn_id: SUBMISSION.turnId,
      role: 'user',
      content: 'Second prompt',
    })

    // The provider's start confirms that turn instead of inventing one, so
    // the reply streams under the prompt that asked for it.
    const started = apply(session, 'turnStarted', null)
    expect(started.turns).toHaveLength(2)
    expect(started.turns.at(-1)?.provider_turn_started).toBe(true)
    const replied = apply(started, 'textDelta', 'Sure.')
    expect(replied.messages.map((message) => message.role)).toEqual([
      'user',
      'assistant',
      'user',
      'assistant',
    ])
    expect(replied.messages.at(-1)?.turn_id).toBe(SUBMISSION.turnId)
  })

  test('callback presentation reaches a fresh follower and repairs an earlier raw echo', () => {
    const submission = { ...SUBMISSION, message: '[Waku automatic decision notification] {"question":"private context"}', displayContent: '子会话请求管家决定。' }
    const fresh = apply(idleSession(), 'promptSubmitted', submission)
    expect(fresh.messages.at(-1)?.display_content).toBe('子会话请求管家决定。')
    expect(fresh.messages.at(-1)?.content).toBe(submission.message)
    const old = apply(idleSession(), 'promptSubmitted', { ...submission, displayContent: undefined })
    expect(old.messages.at(-1)?.display_content).toBeUndefined()
    const repaired = apply(old, 'promptSubmitted', submission)
    expect(repaired.messages).toHaveLength(old.messages.length)
    expect(repaired.messages.at(-1)?.display_content).toBe('子会话请求管家决定。')
    const replay = apply(repaired, 'promptSubmitted', { ...submission, displayContent: undefined })
    expect(replay.messages.at(-1)?.display_content).toBe('子会话请求管家决定。')
  })

  test('the submitting client’s own echo changes nothing', () => {
    const session = runningSession()
    const result = reduceRuntimeEvent(
      session,
      event('promptSubmitted', {
        message: 'Go',
        turnId: SUBMISSION.turnId,
        messageId: SUBMISSION.messageId,
      }),
      clock,
    )

    expect(result.session.turns).toEqual(session.turns)
    expect(result.session.messages).toEqual(session.messages)
    expect(result.session.status).toBe('connecting')
  })

  test('a provider-started turn without a prompt receives the submitted message', () => {
    const session: AgentSession = {
      ...idleSession(),
      status: 'working',
      turns: [
        ...idleSession().turns,
        {
          id: 'provider-turn',
          turn_count: 2,
          status: 'running',
          provider_turn_started: true,
          provider_resume_at: null,
          started_at: 150,
          completed_at: null,
          checkpoint: null,
        },
      ],
    }
    const result = reduceRuntimeEvent(session, event('promptSubmitted', SUBMISSION), clock)

    expect(result.session.turns).toHaveLength(2)
    expect(result.session.messages.at(-1)).toMatchObject({
      id: SUBMISSION.messageId,
      turn_id: 'provider-turn',
      role: 'user',
      content: 'Second prompt',
    })
  })

  test('names an unnamed task after its first submitted prompt', () => {
    const session: AgentSession = { ...idleSession(), messages: [], turns: [] }
    const result = reduceRuntimeEvent(
      session,
      event('promptSubmitted', { ...SUBMISSION, message: 'Add a dark mode toggle to settings' }),
      clock,
    )

    expect(result.session.auto_title).toBe('Add a dark mode toggle to settings')
    expect(result.session.turns.at(-1)?.turn_count).toBe(1)
  })
})

function apply(session: AgentSession, kind: string, payload: unknown) {
  return reduceRuntimeEvent(session, event(kind, payload), clock).session
}

function event(kind: string, payload: unknown): SequencedEvent {
  return {
    sessionId: 'session',
    runtimeId: 'runtime',
    epoch: 'epoch',
    sequence: 1,
    event: { kind, payload: payload as never },
  }
}

/** One completed turn, runtime still attached, nothing running. */
function idleSession(): AgentSession {
  return {
    ...runningSession(),
    status: 'idle',
    messages: [
      {
        id: 'message',
        turn_id: 'turn',
        role: 'user',
        content: 'Go',
        created_at: 100,
        streaming: false,
      },
      {
        id: 'reply',
        turn_id: 'turn',
        role: 'assistant',
        content: 'Done',
        created_at: 110,
        streaming: false,
      },
    ],
    turns: [
      {
        id: 'turn',
        turn_count: 1,
        status: 'completed',
        provider_turn_started: true,
        provider_resume_at: null,
        started_at: 100,
        completed_at: 110,
        checkpoint: null,
      },
    ],
  }
}

function runningSession(): AgentSession {
  return {
    id: 'session',
    title: 'New task',
    project_id: 'project',
    workspace: { kind: 'local' },
    provider: 'codex',
    runtime_mode: 'fullAccess',
    status: 'connecting',
    created_at: 100,
    updated_at: 100,
    provider_cursor: null,
    messages: [
      {
        id: 'message',
        turn_id: 'turn',
        role: 'user',
        content: 'Go',
        created_at: 100,
        streaming: false,
      },
    ],
    transcript_blocks: [],
    turns: [
      {
        id: 'turn',
        turn_count: 1,
        status: 'running',
        provider_turn_started: false,
        provider_resume_at: null,
        started_at: 100,
        completed_at: null,
        checkpoint: null,
      },
    ],
  }
}

test('durable acknowledgments preserve pending and failed boundaries across stale replies', () => {
  const cursor = { runtime_id: 'runtime', epoch: 'epoch', sequence: 10 }
  let session: AgentSession = { ...runningSession(), runtime_event_cursor: cursor, history_save_error: 'disk full' }
  const acknowledge = (sequence: number, error: string | null, runtimeId = 'runtime') => {
    session = reduceRuntimeEvent(session, {
      sessionId: 'session', runtimeId, epoch: 'epoch', sequence,
      event: { kind: 'historyPersistence', payload: { error } },
    }).session
  }
  acknowledge(8, null)
  expect(session.history_save_error).toBe('disk full')
  expect(session.runtime_event_cursor?.sequence).toBe(10)
  acknowledge(10, null)
  expect(session.history_save_error).toBeUndefined()
  acknowledge(9, 'old failure')
  acknowledge(11, 'other runtime', 'stale-runtime')
  expect(session.history_save_error).toBeUndefined()
  expect(session.history_saved_cursor?.sequence).toBe(10)
})


test('cancel and interaction responses preserve authoritative user actions', () => {
  const waiting = { ...runningSession(), status: 'waiting' as const }
  const responded = apply(waiting, 'interactionResponded', { request_id: 'approval' })
  expect(responded.status).toBe('working')
  const cancelled = apply(responded, 'cancelRequested', null)
  expect(cancelled.status).toBe('working')
  expect(cancelled.turns.at(-1)?.status).toBe('running')
  const stopped = apply(cancelled, 'processExited', null)
  expect(stopped.status).toBe('idle')
  expect(stopped.turns.at(-1)?.status).toBe('interrupted')
  const exited = apply(apply(cancelled, 'turnFinished', { success: false }), 'processExited', null)
  expect(exited.status).toBe('idle')
  expect(exited.turns.at(-1)?.status).toBe('interrupted')
})


test('terminal subscriptions wait for the matching save outcome before removal', () => {
  const state = { pendingExit: null }
  let session = runningSession()
  const exit = { ...event('processExited', null), sequence: 8 }
  expect(reduceRuntimeEventAfterPersistence(session, exit, state, clock)).toBeNull()
  const stale = reduceRuntimeEventAfterPersistence(session, {
    ...event('historyPersistence', { error: null }), sequence: 7,
  }, state, clock)!
  expect(stale.removeRuntime).toBe(false)
  session = stale.session
  const saved = reduceRuntimeEventAfterPersistence(session, {
    ...event('historyPersistence', { error: null }), sequence: 8,
  }, state, clock)!
  expect(saved.removeRuntime).toBe(true)
  expect(saved.session.runtime_event_cursor?.sequence).toBe(8)
  expect(saved.session.history_saved_cursor?.sequence).toBe(8)
  expect(saved.session.turns.at(-1)?.status).toBe('failed')
})


test('a terminal save failure survives removal and ignores another runtime acknowledgment', () => {
  const state = { pendingExit: null }
  const session = { ...runningSession(), runtime_event_cursor: { runtime_id: 'runtime', epoch: 'epoch', sequence: 7 } }
  expect(reduceRuntimeEventAfterPersistence(session, { ...event('processExited', null), sequence: 8 }, state, clock)).toBeNull()
  const unrelated = reduceRuntimeEventAfterPersistence(session, {
    ...event('historyPersistence', { error: null }), runtimeId: 'previous-runtime', sequence: 9,
  }, state, clock)!
  expect(unrelated.removeRuntime).toBe(false)
  const failed = reduceRuntimeEventAfterPersistence(unrelated.session, {
    ...event('historyPersistence', { error: 'disk full' }), sequence: 8,
  }, state, clock)!
  expect(failed.removeRuntime).toBe(true)
  expect(failed.session.history_save_error).toBe('disk full')
  expect(failed.session.history_saved_cursor).toBeUndefined()
})

test('an exit already covered by hydrated history does not wait for another acknowledgment', () => {
  const session = { ...runningSession(), history_saved_cursor: { runtime_id: 'runtime', epoch: 'epoch', sequence: 8 } }
  const result = reduceRuntimeEventAfterPersistence(session, {
    ...event('processExited', null), sequence: 8,
  }, { pendingExit: null }, clock)!
  expect(result.removeRuntime).toBe(true)
  expect(result.session.history_saved_cursor?.sequence).toBe(8)
})


test('hydrated pending interactions survive cursor-based attachment and matching replies', () => {
  const waiting = apply(runningSession(), 'permission', {
    requestId: 'approval', title: 'Run checks', detail: 'cargo test', options: [],
  })
  expect(waiting.pending_permission?.requestId).toBe('approval')
  const restored = JSON.parse(JSON.stringify(waiting)) as AgentSession
  const stale = apply(restored, 'interactionResponded', { request_id: 'old' })
  expect(stale.pending_permission?.requestId).toBe('approval')
  const responded = apply(stale, 'interactionResponded', { request_id: 'approval' })
  expect(responded.pending_permission).toBeUndefined()
  const asked = apply(responded, 'userInputRequested', {
    requestId: 'question', questions: [{ id: 'q', header: 'Scope', question: 'Which files?', options: [], multiSelect: false }],
  })
  expect(asked.pending_user_input?.requestId).toBe('question')
  expect(apply(asked, 'turnFinished', { success: true }).pending_user_input).toBeUndefined()
})

test('history snapshot replaces stale content and resumes the same message', () => {
  const old = idleSession()
  const saved = apply(apply(old, 'turnStarted', null), 'textDelta', 'preserved')
  saved.parent_session_id = 'original-parent'
  saved.history_saved_cursor = { runtime_id: 'runtime', epoch: 'epoch', sequence: 20_006 }
  const snapshot = { ...event('historySnapshot', saved), sequence: 20_006 }
  const restored = reduceRuntimeEvent(old, snapshot, clock).session
  expect(restored.messages.at(-1)?.content).toBe('preserved')
  expect(restored.parent_session_id).toBe('original-parent')
  const continued = reduceRuntimeEvent(restored, { ...event('textDelta', ' tail'), sequence: 20_007 }, clock).session
  expect(continued.messages.at(-1)?.content).toBe('preserved tail')
  expect(continued.messages).toHaveLength(saved.messages.length)
})

test('snapshot preserves the provider error until the exit is settled', () => {
  const started = apply(idleSession(), 'turnStarted', null)
  const saved = apply(started, 'error', 'provider lost connection')
  const restored = reduceRuntimeEvent(started, event('historySnapshot', saved), clock).session
  const exited = reduceRuntimeEvent(restored, event('processExited', null), clock).session
  expect(exited.messages.at(-1)?.content).toBe('provider lost connection')
})

test('snapshot keeps newer user choices and the local submitted turn', () => {
  const saved = idleSession()
  saved.messages[1]!.content = 'complete saved answer'
  saved.last_driver_error = 'previous failure'
  saved.context_usage = { tokens: 321, window: 1000 }
  const current = apply(idleSession(), 'promptSubmitted', SUBMISSION)
  current.project_id = 'new-project'
  current.title = 'new title'
  current.model = 'new model'
  current.runtime_mode = 'fullAccess'
  current.queued_messages = [{ id: 'queued', content: 'queued follow-up', attachments: [], created_at: 200 }]
  const restored = reduceRuntimeEvent(current, event('historySnapshot', saved), clock).session
  expect(restored.title).toBe('new title')
  expect(restored.project_id).toBe('new-project')
  expect(restored.model).toBe('new model')
  expect(restored.runtime_mode).toBe('fullAccess')
  expect(restored.queued_messages?.[0]?.content).toBe('queued follow-up')
  expect(restored.turns.at(-1)?.id).toBe(SUBMISSION.turnId)
  expect(restored.messages[1]?.content).toBe('complete saved answer')
  expect(restored.messages.at(-1)?.content).toBe('Second prompt')
  expect(restored.context_usage?.tokens).toBe(321)
  expect(restored.last_driver_error).toBeUndefined()
  expect(restored.status).toBe('connecting')
})


test('snapshot does not rewind an already applied event', () => {
  const saved = apply(runningSession(), 'textDelta', 'saved')
  saved.runtime_event_cursor = { runtime_id: 'runtime', epoch: 'epoch', sequence: 8 }
  const current = apply(saved, 'textDelta', ' later')
  current.runtime_event_cursor = { runtime_id: 'runtime', epoch: 'epoch', sequence: 9 }
  const restored = reduceRuntimeEvent(current, event('historySnapshot', saved), clock).session
  expect(restored.messages.at(-1)?.content).toBe('saved later')
  expect(restored.runtime_event_cursor?.sequence).toBe(9)
})

test('failed turn reasons survive exit and reconnect until a new submitted turn', () => {
  const failed = apply(apply(runningSession(), 'error', 'provider lost connection'), 'processExited', null)
  expect(failed.last_driver_error).toBe('provider lost connection')
  expect(failed.turns.at(-1)?.status).toBe('failed')
  const reconnected = apply(failed, 'connected', null)
  expect(reconnected.last_driver_error).toBe('provider lost connection')
  expect(apply(reconnected, 'promptSubmitted', SUBMISSION).last_driver_error).toBeUndefined()
  const ended = apply(runningSession(), 'turnFinished', { success: false, summary: 'provider failed turn' })
  expect(ended.last_driver_error).toBe('provider failed turn')
  expect(apply(ended, 'processExited', null).last_driver_error).toBe('provider failed turn')
  const success = apply(apply(runningSession(), 'error', 'temporary error'), 'turnFinished', { success: true })
  expect(success.last_driver_error).toBeUndefined()
})


test('durable child waiting follows completion, replay, and user intervention', () => {
  const original = runningSession()
  const wait = { id: 'wait', parent_turn_id: original.turns.at(-1)!.id, targets: [{ session_id: 'child', turn_id: 'child-turn' }] }
  const registered = apply(original, 'stewardWaitChanged', wait)
  expect(original.steward_wait).toBeUndefined()
  expect(registered.steward_wait).toEqual(wait)
  const completed = apply(registered, 'turnFinished', { success: true })
  expect(completed.status).toBe('idle')
  expect(apply(completed, 'processExited', null).steward_wait).toEqual(wait)
  const saved = { ...completed, runtime_event_cursor: { runtime_id: 'runtime', epoch: 'epoch', sequence: 2 } }
  const stale = { ...original, runtime_event_cursor: { runtime_id: 'runtime', epoch: 'epoch', sequence: 1 } }
  expect(apply(saved, 'historySnapshot', stale).steward_wait).toEqual(wait)
  expect(apply(original, 'historySnapshot', saved).steward_wait).toEqual(wait)
  const next = apply(completed, 'promptSubmitted', SUBMISSION)
  const replayed = apply(next, 'historySnapshot', completed)
  expect(replayed.turns.at(-1)?.id).toBe(SUBMISSION.turnId)
  expect(replayed.steward_wait).toBeUndefined()
  expect(apply(replayed, 'stewardWaitChanged', wait).steward_wait).toBeUndefined()
  for (const kind of ['cancelRequested', 'steerAccepted', 'turnInterrupted']) {
    expect(apply(registered, kind, { message: 'user takes over' }).steward_wait).toBeUndefined()
  }
  expect(apply(registered, 'turnFinished', { success: false }).steward_wait).toBeUndefined()
  expect(apply(registered, 'processExited', null).steward_wait).toBeUndefined()
  expect(apply(completed, 'stewardWaitChanged', null).steward_wait).toBeUndefined()
})


test('tracked steering preserves receipt, original turn, and message identity on replay', () => {
  const original = idleSession()
  const turn = original.turns[0]!.id
  const delivery = { id: 'delivery', caller_session_id: 'parent', target_session_id: original.id,
    prompt: 'Check the boundary', turn_id: turn, mode: 'steer', state: 'accepted',
    confirmation: null, reason: null, created_at: 100 }
  const accepted = apply(original, 'inputDeliveryChanged', delivery)
  const uncertain = apply(accepted, 'inputDeliveryOutcome', { id: delivery.id, state: 'uncertain', confirmation: null, reason: 'No confirmation' })
  const received = apply(uncertain, 'inputDeliveryOutcome', { id: delivery.id, state: 'received', confirmation: 'provider', reason: null })
  const repeated = apply(received, 'inputDeliveryOutcome', { id: delivery.id, state: 'received', confirmation: 'provider', reason: null })
  expect(repeated.input_deliveries?.[0]?.state).toBe('received')
  expect(repeated.messages.filter((message) => message.content === delivery.prompt)).toHaveLength(1)
  expect(repeated.messages.at(-1)?.turn_id).toBe(turn)
  expect(apply(repeated, 'processExited', null).input_deliveries?.[0]?.state).toBe('received')
})

test('queued input transitions to its saved new turn without duplicating the ledger', () => {
  const original = idleSession()
  const delivery = { id: 'queued-delivery', caller_session_id: 'parent', target_session_id: original.id,
    prompt: 'Queued feedback', turn_id: original.turns[0]!.id, mode: 'prompt', state: 'queued',
    confirmation: null, reason: 'Unsupported native steering', created_at: 100 }
  const queued = apply(original, 'inputDeliveryChanged', delivery)
  const submitted = apply(queued, 'inputDeliveryChanged', { ...delivery, turn_id: 'new-turn', state: 'accepted', reason: null })
  const received = apply(submitted, 'inputDeliveryOutcome', { id: delivery.id, state: 'received', confirmation: 'transport', reason: null })
  expect(received.input_deliveries).toHaveLength(1)
  expect(received.input_deliveries?.[0]).toMatchObject({ turn_id: 'new-turn', state: 'received' })
})

test('a late steer receipt preserves a wait registered by the revised plan', () => {
  const original = idleSession()
  const oldTurn = original.turns[0]!.id
  const pending = apply(original, 'inputDeliveryChanged', {
    id: 'old-direction', caller_session_id: original.id, target_session_id: original.id,
    prompt: 'Previous direction', turn_id: oldTurn, mode: 'steer', state: 'uncertain',
    confirmation: null, reason: 'Receipt lost', created_at: 100,
  })
  const revised = apply(pending, 'promptSubmitted', { message: 'Revised plan', turnId: 'revised-turn', messageId: 'revised-message' })
  const wait = { id: 'revised-wait', parent_turn_id: 'revised-turn', targets: [{ session_id: 'child', turn_id: 'child-turn' }] }
  const waiting = apply(revised, 'stewardWaitChanged', wait)
  expect(waiting.steward_wait).toEqual(wait)
  const received = apply(waiting, 'inputDeliveryOutcome', { id: 'old-direction', state: 'received', confirmation: 'provider', reason: null })
  expect(received.steward_wait).toEqual(wait)
  expect(received.messages.at(-1)?.turn_id).toBe(oldTurn)
})


test('snapshot restores task evidence without rewinding its revision', () => {
  const current = idleSession()
  const saved: AgentSession = { ...current, managed_workspace: {
    task_id: current.id, revision: 2, name: 'Accepted task', repository: '/isolated/repository',
    base_commit: 'base', target_branch: 'main', target_commit: 'base', integration_branch: 'task',
    integration_commit: 'accepted', branch: 'task', path: '/isolated/task', owned: true, ready: true,
    created: true, coordination: null, cleanup: [], deliveries: [], dependencies: [], results: [], error: null,
  } }
  const restored = reduceRuntimeEvent(current, event('historySnapshot', saved), clock).session
  expect(restored.managed_workspace?.revision).toBe(2)
  saved.managed_workspace!.revision = 1
  const retained = reduceRuntimeEvent(restored, event('historySnapshot', saved), clock).session
  expect(retained.managed_workspace?.revision).toBe(2)
  saved.managed_workspace = undefined
  expect(reduceRuntimeEvent(retained, event('historySnapshot', saved), clock).session.managed_workspace?.integration_commit).toBe('accepted')
})

test('consultation input displays the instruction while retaining provider context', () => {
  for (const mode of ['prompt', 'steer']) {
    const original = idleSession()
    const delivery = { id: 'consultation-input', caller_session_id: original.id, target_session_id: original.id,
      prompt: 'Full provider context with recent_discussion and pending_child_targets',
      display_content: 'Only change the dependent result.', turn_id: mode === 'steer' ? original.turns[0]!.id : 'new-turn',
      mode, state: 'accepted', confirmation: null, reason: null, created_at: 100 }
    let session = apply(original, 'inputDeliveryChanged', delivery)
    if (mode === 'prompt') session = apply(session, 'promptSubmitted', {
      message: delivery.prompt, turnId: delivery.turn_id, messageId: 'consultation-message',
    })
    session = apply(session, 'inputDeliveryOutcome', { id: delivery.id, state: 'received', confirmation: 'provider', reason: null })
    const message = session.messages.find(message => message.content === delivery.prompt)
    expect(message?.display_content).toBe(delivery.display_content)
    expect(session.input_deliveries?.[0]?.prompt).toBe(delivery.prompt)
  }
})

test('manager decision receipt and callback wait survive shared-client projection', () => {
  const session = runningSession()
  const turn = session.turns.at(-1)!.id
  const request = { id: 'decision', parent_session_id: 'parent', child_session_id: session.id,
    turn_id: turn, question: 'Format?', context: 'Output', recommendation: 'JSON', blocked_work: 'Write file',
    instruction_message_id: 'instruction', instruction: 'Produce output', state: 'pendingReceipt' as const,
    decision: 'Use JSON', authority_message_id: 'instruction', reason: null, notified: true }
  session.decision_requests = [request]
  session.input_deliveries = [{ id: 'decision', caller_session_id: 'parent', target_session_id: session.id,
    prompt: 'Use JSON', display_content: 'Manager decision', turn_id: turn, mode: 'prompt', state: 'uncertain',
    confirmation: null, reason: null, created_at: 1 }]
  const received = apply(session, 'inputDeliveryOutcome', { id: 'decision', state: 'received', confirmation: 'transport', reason: null })
  expect(received.decision_requests?.[0]?.state).toBe('resolved')
  const cancelled = structuredClone(session)
  cancelled.decision_requests![0]!.state = 'invalidated'
  expect(apply(cancelled, 'inputDeliveryOutcome', { id: 'decision', state: 'received', confirmation: 'transport', reason: null }).decision_requests?.[0]?.state).toBe('invalidated')
  expect(apply(idleSession(), 'historySnapshot', received).decision_requests?.[0]?.decision).toBe('Use JSON')
  const wait = { id: 'results', parent_turn_id: turn, targets: [{ session_id: 'child', turn_id: 'child-turn' }] }
  const callback = apply(session, 'stewardWaitChanged', wait)
  expect(apply(callback, 'promptSubmitted', { message: 'Automatic decision notification', turnId: turn, messageId: 'callback' }).steward_wait).toEqual(wait)
})
