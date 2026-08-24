use super::continuation::ProjectionDiffMemo;

#[derive(Clone, Debug)]
pub(super) struct OpenAiInlineArtifactBinding {
    instructions: String,
    projection_diff_memo: Option<ProjectionDiffMemo>,
}

impl OpenAiInlineArtifactBinding {
    pub(super) fn new(instructions: String) -> Self {
        Self {
            instructions,
            projection_diff_memo: None,
        }
    }

    pub(super) fn instructions(&self) -> &str {
        &self.instructions
    }

    pub(super) fn projection_diff_memo(&self) -> Option<&ProjectionDiffMemo> {
        self.projection_diff_memo.as_ref()
    }

    pub(super) fn with_projection_diff_memo(mut self, memo: ProjectionDiffMemo) -> Self {
        self.projection_diff_memo = Some(memo);
        self
    }
}
