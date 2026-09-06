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
capability. An explicit lack of steering support returns `unsupported`.

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
