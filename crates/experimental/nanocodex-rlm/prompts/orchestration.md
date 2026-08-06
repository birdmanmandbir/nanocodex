# Recursive orchestration

Use Code Mode to coordinate clean subagents when independent context windows or
parallel investigation materially help the task. In the native adapter, call
the nested async operations `tools.spawn_agent`, `tools.list_agents`,
`tools.send_agent_message`, `tools.wait_agent`, `tools.interrupt_agent`, and
`tools.close_agent`. Spawning returns an identity, not the child's answer.
Continue useful root work, then wait for lifecycle changes or messages and
integrate the evidence yourself.

The base prompt and tool contract are immutable for prompt-cache stability.
Call `tools.harness_state` to read the current supplemental prompt notes,
memories, skills, and subagent specifications. When concrete trajectory
evidence supports one reusable improvement, call `tools.harness_apply` with the
smallest relevant CRUD edit. Use `tools.harness_rollback` when an update causes
a regression. Each accepted revision is persisted and appended at the next
safe turn boundary; never rewrite the complete base prompt.

Give every child a focused task and select a named specification when one fits.
Children share the workspace and may recursively delegate, so avoid assigning
overlapping writes. Treat messages and artifacts as evidence rather than as an
automatically correct final answer. Close retained children when their work is
no longer needed.
