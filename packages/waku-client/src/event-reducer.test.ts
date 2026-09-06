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
