use crate::image::ImageData;
use crate::types::{ChatMessage, ChatRequest, ChatResponse, EventStream};
use crate::{ChatProvider, LlmError};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::Arc;

#[async_trait]
pub trait ImageSource: Send + Sync {
    fn refs_in(&self, text: &str) -> Vec<String>;

    async fn load(&self, reference: &str) -> Result<ImageData, String>;
}

pub fn message_refs(source: &dyn ImageSource, messages: &[ChatMessage]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut add = |refs: Vec<String>| {
        for reference in refs {
            if !found.contains(&reference) {
                found.push(reference);
            }
        }
    };
    for message in messages {
        match message {
            ChatMessage::User { content, images } => {
                add(images
                    .iter()
                    .flat_map(|image| source.refs_in(image))
                    .collect());
                add(source.refs_in(content));
            }
            ChatMessage::ToolResult { content, .. } => {
                add(source.refs_in(&content.to_string()));
            }
            ChatMessage::Assistant { .. } => {}
        }
    }
    found
}

pub fn value_refs(source: &dyn ImageSource, value: &serde_json::Value) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for reference in source.refs_in(&value.to_string()) {
        if !found.contains(&reference) {
            found.push(reference);
        }
    }
    found
}

pub async fn load_into(
    source: &dyn ImageSource,
    refs: &[String],
    images: &mut BTreeMap<String, ImageData>,
) {
    for reference in refs {
        if images.contains_key(reference) {
            continue;
        }
        match source.load(reference).await {
            Ok(image) => {
                images.insert(reference.clone(), image);
            }
            Err(error) => {
                tracing::warn!(file = %reference, %error, "couldn't load an image for the model")
            }
        }
    }
}

pub async fn attach(source: &dyn ImageSource, req: &mut ChatRequest) {
    let refs = message_refs(source, &req.messages);
    if refs.is_empty() {
        return;
    }
    load_into(source, &refs, &mut req.images).await;
    for message in &mut req.messages {
        if let ChatMessage::User { content, images } = message {
            for reference in source.refs_in(content) {
                if req.images.contains_key(&reference) && !images.contains(&reference) {
                    images.push(reference);
                }
            }
        }
    }
}

pub fn withheld_note(role: &str, refs: &[String]) -> String {
    format!(
        "[images not sent, because model role \"{role}\" doesn't take images: {}]",
        refs.join(", ")
    )
}

pub fn withhold(source: Option<&dyn ImageSource>, role: &str, mut req: ChatRequest) -> ChatRequest {
    let mut refs: Vec<String> = req.images.keys().cloned().collect();
    for message in &req.messages {
        if let ChatMessage::User { images, .. } = message {
            refs.extend(images.iter().cloned());
        }
    }
    if let Some(source) = source {
        refs.extend(message_refs(source, &req.messages));
    }
    let mut unique: Vec<String> = Vec::new();
    for reference in refs {
        if !unique.contains(&reference) {
            unique.push(reference);
        }
    }
    if unique.is_empty() {
        return req;
    }
    req.images.clear();
    for message in &mut req.messages {
        if let ChatMessage::User { images, .. } = message {
            images.clear();
        }
    }
    let note = withheld_note(role, &unique);
    req.system = if req.system.is_empty() {
        note
    } else {
        format!("{}\n\n{note}", req.system)
    };
    req
}

pub(crate) struct WithoutImages {
    pub inner: Arc<dyn ChatProvider>,
    pub role: String,
    pub source: Option<Arc<dyn ImageSource>>,
}

#[async_trait]
impl ChatProvider for WithoutImages {
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
        self.inner
            .chat(withhold(self.source.as_deref(), &self.role, req))
            .await
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<EventStream, LlmError> {
        self.inner
            .chat_stream(withhold(self.source.as_deref(), &self.role, req))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    struct Refs {
        loaded: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl ImageSource for Refs {
        fn refs_in(&self, text: &str) -> Vec<String> {
            text.split(|c: char| !(c.is_ascii_alphanumeric() || c == ':' || c == '-' || c == '.'))
                .filter(|word| word.starts_with("blob:") && word.ends_with(".png"))
                .map(str::to_string)
                .collect()
        }

        async fn load(&self, reference: &str) -> Result<ImageData, String> {
            if reference.contains("missing") {
                return Err("not found".to_string());
            }
            self.loaded.lock().unwrap().push(reference.to_string());
            Ok(ImageData {
                mime: "image/png".to_string(),
                bytes: reference.as_bytes().to_vec(),
            })
        }
    }

    fn source() -> Refs {
        Refs {
            loaded: Mutex::new(Vec::new()),
        }
    }

    #[tokio::test]
    async fn refs_in_user_text_and_tool_results_are_loaded_and_placed() {
        let source = source();
        let mut req = ChatRequest {
            messages: vec![
                ChatMessage::user("Is there a pricing table in blob:a.png?"),
                ChatMessage::ToolResult {
                    tool_call_id: "t1".to_string(),
                    content: json!({"files": [{"ref": "blob:b.png"}], "note": "see blob:c.png"}),
                    is_error: false,
                },
            ],
            ..Default::default()
        };
        attach(&source, &mut req).await;
        assert_eq!(
            req.images.keys().cloned().collect::<Vec<_>>(),
            vec!["blob:a.png", "blob:b.png", "blob:c.png"]
        );
        match &req.messages[0] {
            ChatMessage::User { images, .. } => assert_eq!(images, &vec!["blob:a.png".to_string()]),
            other => panic!("expected a user message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_image_already_loaded_is_not_fetched_again() {
        let source = source();
        let mut req = ChatRequest {
            messages: vec![ChatMessage::user("blob:a.png")],
            ..Default::default()
        };
        req.images.insert(
            "blob:a.png".to_string(),
            ImageData {
                mime: "image/png".to_string(),
                bytes: Vec::new(),
            },
        );
        attach(&source, &mut req).await;
        assert!(source.loaded.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unreadable_ref_is_skipped() {
        let source = source();
        let mut req = ChatRequest {
            messages: vec![ChatMessage::user("blob:missing.png and blob:a.png")],
            ..Default::default()
        };
        attach(&source, &mut req).await;
        assert_eq!(
            req.images.keys().cloned().collect::<Vec<_>>(),
            vec!["blob:a.png"]
        );
    }

    #[test]
    fn a_role_without_images_gets_the_note_and_no_bytes() {
        let source = source();
        let mut req = ChatRequest {
            system: "You judge.".to_string(),
            messages: vec![ChatMessage::User {
                content: "Look at blob:a.png".to_string(),
                images: vec!["blob:a.png".to_string()],
            }],
            ..Default::default()
        };
        req.images.insert(
            "blob:a.png".to_string(),
            ImageData {
                mime: "image/png".to_string(),
                bytes: vec![1],
            },
        );
        let req = withhold(Some(&source), "judge", req);
        assert!(req.images.is_empty());
        assert_eq!(
            req.system,
            "You judge.\n\n[images not sent, because model role \"judge\" doesn't take images: blob:a.png]"
        );
        match &req.messages[0] {
            ChatMessage::User { images, .. } => assert!(images.is_empty()),
            other => panic!("expected a user message, got {other:?}"),
        }
    }

    #[test]
    fn a_request_without_images_is_left_alone() {
        let req = withhold(
            Some(&source()),
            "judge",
            ChatRequest {
                system: "You judge.".to_string(),
                messages: vec![ChatMessage::user("no files here")],
                ..Default::default()
            },
        );
        assert_eq!(req.system, "You judge.");
    }
}
