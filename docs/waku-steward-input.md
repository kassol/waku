# Steward input delivery

`waku_prompt` accepts an optional UUID `delivery_id`. Repeating the same caller,
target, ID, and text returns the saved delivery. Reusing the ID with different
text fails. Clients that need to recover a lost response must choose the ID
before submitting and use `waku_prompt_status` to query it.

The daemon authorizes the direct child and its current permission ceiling before
both submission and lookup. A target operation excludes concurrent desktop
input and callback submission. Pending approvals and user questions reject
ordinary input. Idle targets start one saved turn; busy targets use the current
provider's native steering path. A missing runtime is a failure with unknown
capability. An explicit lack of steering support queues the input for the next eligible turn.

The daemon saves `accepted` before crossing the provider boundary and records
`uncertain` before sending. Codex RPC success confirms provider receipt; Claude
stdin completion confirms transport receipt. Neither confirmation asserts model
adoption. Explicit rejection becomes `failed`. A lost confirmation or daemon
restart remains `uncertain`; it must not trigger automatic resubmission.

Codex preserves the native turn ID captured when the delivery enters its writer
queue and passes it as `expectedTurnId`. Claude checks a turn generation because
its protocol has no expected-turn parameter. Its confirmation remains limited to
the transport boundary.

Delivery records survive stale desktop saves and history rewind. Forks start
with no delivery records. Native history shows each delivery's text, state,
confirmation, ID, target turn, and failure reason in an expandable activity.

## Persistent fallback queue

An explicitly unsupported busy runtime records the input as `queued`. A missing
runtime does not qualify. The existing daemon event worker consumes this queue
before checking steward callbacks. It does not depend on a connected desktop or
periodic polling.

Each queued input waits for its predecessor turn. Dequeue rechecks the saved
caller, direct-child relationship, permission ceiling, pending interactions, and
predecessor turn. A user-started replacement turn invalidates the queued input.
When a queued input starts, later entries advance to that new predecessor in the
same save, retaining FIFO order. An unconfirmed submitted prompt blocks later
queue entries until its receipt is resolved. A restart preserves pending entries
and never resends an accepted or uncertain entry.

Queue transitions use the same durable input identity and history activity.
Shutdown prevents automatic dispatch. A failed validation retains the original
input with its failure reason.

Cancellation fails queued inputs before provider submission. A restart that loses
an unanswered provider interaction retains those inputs as failed and requires a
new user decision.

Explicit user directions from an independent discussion use `ExecuteConsultation`.
The daemon retains the original instruction, a fixed prompt, its delivery ID,
and the pending child targets in the discussion record before submission. This
user-only command targets the source task; scoped MCP clients cannot use it.
`LoadConsultation` reads current delivery states without resending any input.
A retry reuses the saved prompt and ID, including after restart.

An accepted or uncertain user steer holds the previous wait for that same turn.
It cannot automatically resume the old plan while the new direction is unresolved.
A confirmed rejection permits the old wait to resume; receipt clears the wait.
Historical uncertain deliveries from other turns do not block a new wait. Queued
input uses the existing durable queue. The instruction asks the steward to retain
pending results, steer only affected direct children, preserve unrelated work,
and establish necessary waits under the new plan. A cancellation is complete only
after actual stopped confirmation.
