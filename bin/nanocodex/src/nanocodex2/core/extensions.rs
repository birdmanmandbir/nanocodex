//! UI-facing extension metadata owned by the managed client.

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Skill {
    name: String,
    description: String,
}

impl Skill {
    pub(crate) fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn description(&self) -> &str {
        &self.description
    }
}
