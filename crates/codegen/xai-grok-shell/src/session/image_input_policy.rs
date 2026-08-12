use serde::Serialize;
use xai_grok_sampling_types::{ContentPart, ConversationItem};

pub(crate) const TEXT_MODEL_HISTORY_IMAGES_CODE: &str = "TEXT_MODEL_HISTORY_CONTAINS_IMAGES";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageInputPolicy {
    PassThrough,
    Transcribe,
}

impl ImageInputPolicy {
    pub(crate) const fn from_accepts_images(accepts_images: bool) -> Self {
        if accepts_images {
            Self::PassThrough
        } else {
            Self::Transcribe
        }
    }

    pub(crate) const fn attaches_images(self) -> bool {
        match self {
            Self::PassThrough => true,
            Self::Transcribe => false,
        }
    }

    pub(crate) const fn normalizes_strictly(self) -> bool {
        matches!(self, Self::Transcribe)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TextModelHistoryImagesError {
    pub(crate) code: &'static str,
    pub(crate) model: String,
    /// Count of `ContentPart::Image` on user and tool-result items that
    /// would be sent to a text-only model.
    pub(crate) user_image_count: usize,
    pub(crate) action: &'static str,
}

/// Reject sampling when a text-only model would still receive structural
/// images from history (user attachments or prior tool-result images).
///
/// `model` should be the **catalog** model id (config key / picker id), not
/// the provider API `model` wire string.
pub(crate) fn reject_text_model_image_history(
    policy: ImageInputPolicy,
    model: &str,
    items: &[ConversationItem],
) -> Result<(), TextModelHistoryImagesError> {
    if policy.attaches_images() {
        return Ok(());
    }
    let user_image_count = items
        .iter()
        .map(|item| match item {
            ConversationItem::User(user) => user
                .content
                .iter()
                .filter(|part| matches!(part, ContentPart::Image { .. }))
                .count(),
            ConversationItem::ToolResult(tr) => tr
                .images
                .iter()
                .filter(|part| matches!(part, ContentPart::Image { .. }))
                .count(),
            ConversationItem::System(_)
            | ConversationItem::Assistant(_)
            | ConversationItem::BackendToolCall(_)
            | ConversationItem::Reasoning(_) => 0,
        })
        .sum();
    if user_image_count == 0 {
        return Ok(());
    }
    Err(TextModelHistoryImagesError {
        code: TEXT_MODEL_HISTORY_IMAGES_CODE,
        model: model.to_owned(),
        user_image_count,
        action: "switch_to_vision_model_or_start_new_session",
    })
}

/// Back-compat alias used by older call sites / docs.
#[allow(dead_code)]
pub(crate) fn reject_text_model_user_image_history(
    policy: ImageInputPolicy,
    model: &str,
    items: &[ConversationItem],
) -> Result<(), TextModelHistoryImagesError> {
    reject_text_model_image_history(policy, model, items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_is_derived_from_image_capability() {
        assert_eq!(
            ImageInputPolicy::from_accepts_images(true),
            ImageInputPolicy::PassThrough
        );
        assert_eq!(
            ImageInputPolicy::from_accepts_images(false),
            ImageInputPolicy::Transcribe
        );
    }

    #[test]
    fn text_model_rejects_historical_user_images() {
        let mut user = ConversationItem::user("look");
        user.add_image("data:image/png;base64,AAAA");
        let error = reject_text_model_image_history(
            ImageInputPolicy::Transcribe,
            "deepseek-flash",
            &[user],
        )
        .expect_err("text-only history must be blocked");
        assert_eq!(error.user_image_count, 1);
        assert_eq!(error.model, "deepseek-flash");
        assert_eq!(
            serde_json::to_value(error).unwrap(),
            serde_json::json!({
                "code": "TEXT_MODEL_HISTORY_CONTAINS_IMAGES",
                "model": "deepseek-flash",
                "userImageCount": 1,
                "action": "switch_to_vision_model_or_start_new_session",
            })
        );
    }

    #[test]
    fn text_model_rejects_historical_tool_result_images() {
        let image = ContentPart::Image {
            url: "data:image/png;base64,AAAA".into(),
        };
        let error = reject_text_model_image_history(
            ImageInputPolicy::Transcribe,
            "deepseek-flash",
            &[ConversationItem::tool_result_with_images(
                "call",
                "ok",
                vec![image],
            )],
        )
        .expect_err("tool-result images must also block text-only models");
        assert_eq!(error.user_image_count, 1);
    }

    #[test]
    fn vision_model_allows_user_and_tool_images() {
        let image = ContentPart::Image {
            url: "data:image/png;base64,AAAA".into(),
        };
        let user = {
            let mut item = ConversationItem::user("look");
            item.add_image("data:image/png;base64,AAAA");
            item
        };
        assert!(
            reject_text_model_image_history(ImageInputPolicy::PassThrough, "vision", &[user])
                .is_ok()
        );
        assert!(
            reject_text_model_image_history(
                ImageInputPolicy::PassThrough,
                "vision",
                &[ConversationItem::tool_result_with_images(
                    "call",
                    "ok",
                    vec![image]
                )]
            )
            .is_ok()
        );
    }
}
