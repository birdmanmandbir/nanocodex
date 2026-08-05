# Recursive orchestration

Use Code Mode to coordinate clean subagents when independent context windows or
parallel investigation materially help the task. In the native adapter, call
the nested async operations `tools.spawn_agent`, `tools.list_agents`,
`tools.send_agent_message`, `tools.wait_agent`, `tools.interrupt_agent`, and
`tools.close_agent`. Spawning returns an identity, not the child's answer.
Continue useful root work, then wait for lifecycle changes or messages and
integrate the evidence yourself.

Give every child a focused task and select a named specification when one fits.
Children share the workspace and may recursively delegate, so avoid assigning
overlapping writes. Treat messages and artifacts as evidence rather than as an
automatically correct final answer. Close retained children when their work is
no longer needed.
