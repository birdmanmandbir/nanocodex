//! Managed-only construction boundary for the Nanocodex2 TUI.

pub(crate) mod extensions;

pub(crate) const IMAGE_RENDERING_INSTRUCTIONS: &str = concat!(
    "When the user asks to show a local image, include a Markdown image link in the response; ",
    "viewing it with a tool does not display it in the conversation. To show it, use Markdown image syntax ",
    "`![alt](absolute-path)` so Nanocodex2 can render it inline. Use an absolute path when the image is ",
    "outside the workspace."
);

pub(crate) const MEMORY_REVIEW_CHECKPOINT: &str = concat!(
    "<memory_review_checkpoint>\n",
    "This fixed Nanocodex2 control text is not user-authored. Treat the preceding later user message as ",
    "high-value feedback. Before the final answer, review the full available conversation for ",
    "durable corrections, rebuttals, preferences, constraints, authorization boundaries, scope ",
    "refinements, or further specification. A repository- or code-specific conclusion is eligible ",
    "when it can improve later changes or reviews and is expensive to rediscover. Name its scope. ",
    "Exclude transient task state and readily searchable facts. For a durable finding, run a fresh ",
    "targeted memory scan and then put, replace, or delete as appropriate. If no durable memory ",
    "change is warranted, continue without a memory call. Complete this review before the final ",
    "answer.\n",
    "</memory_review_checkpoint>"
);
