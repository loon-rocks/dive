use crate::image::ImageData;
use crate::types::{ChatMessage, ChatRequest, ChatResponse, EventStream};
use crate::{ChatProvider, LlmError};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::Arc;

pub const MAX_INLINE_TEXT: usize = 100 * 1024;

#[async_trait]
pub trait ImageSource: Send + Sync {
    fn refs_in(&self, text: &str) -> Vec<String>;

    fn text_refs_in(&self, _text: &str) -> Vec<String> {
        Vec::new()
    }

    async fn load(&self, reference: &str) -> Result<ImageData, String>;
}

pub async fn inline_text(source: &dyn ImageSource, req: &mut ChatRequest) {
    let mut seen: Vec<String> = Vec::new();
    for message in &mut req.messages {
        let ChatMessage::User { content, .. } = message else {
            continue;
        };
        for reference in source.text_refs_in(content) {
            if seen.contains(&reference) {
                continue;
            }
            match source.load(&reference).await {
                Ok(file) => {
                    content.push_str("\n\n");
                    content.push_str(&text_block(&reference, &file));
                }
                Err(error) => {
                    tracing::warn!(file = %reference, %error, "couldn't load a text file for the model")
                }
            }
            seen.push(reference);
        }
    }
}

pub fn cut_at(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn text_block(reference: &str, file: &ImageData) -> String {
    let text = String::from_utf8_lossy(&file.bytes);
    let kept = cut_at(&text, MAX_INLINE_TEXT);
    let mut block = format!(
        "[file {reference} · {} · {} bytes]\n{kept}",
        file.mime,
        file.bytes.len(),
    );
    if kept.len() < text.len() {
        block.push_str("\n[the rest is cut off after 100 KB; read it with builtin__read_file]");
    }
    block
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
    attach_since(source, req, 0).await;
}

pub async fn attach_since(source: &dyn ImageSource, req: &mut ChatRequest, from: usize) {
    let start = from.min(req.messages.len());
    let refs = message_refs(source, &req.messages[start..]);
    load_into(source, &refs, &mut req.images).await;
    if req.images.is_empty() {
        return;
    }
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

fn add_unique(found: &mut Vec<String>, refs: impl IntoIterator<Item = String>) {
    for reference in refs {
        if !found.contains(&reference) {
            found.push(reference);
        }
    }
}

pub fn withhold(source: Option<&dyn ImageSource>, role: &str, mut req: ChatRequest) -> ChatRequest {
    let mut withheld: Vec<String> = Vec::new();
    for message in &mut req.messages {
        let mut named: Vec<String> = Vec::new();
        match message {
            ChatMessage::User { content, images } => {
                add_unique(&mut named, images.drain(..));
                if let Some(source) = source {
                    add_unique(&mut named, source.refs_in(content));
                }
                if !named.is_empty() {
                    content.push_str("\n\n");
                    content.push_str(&withheld_note(role, &named));
                }
            }
            ChatMessage::ToolResult { content, .. } => {
                if let Some(source) = source {
                    add_unique(&mut named, source.refs_in(&content.to_string()));
                }
                if !named.is_empty() {
                    let note = serde_json::Value::String(withheld_note(role, &named));
                    match content {
                        serde_json::Value::Object(fields) => {
                            fields.insert("images_not_sent".to_string(), note);
                        }
                        other => {
                            *other = serde_json::json!({"result": other.take(), "images_not_sent": note});
                        }
                    }
                }
            }
            ChatMessage::Assistant { .. } => {}
        }
        add_unique(&mut withheld, named);
    }
    add_unique(&mut withheld, std::mem::take(&mut req.images).into_keys());
    req.withheld_images = withheld;
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

    struct Texts;

    #[async_trait]
    impl ImageSource for Texts {
        fn refs_in(&self, _text: &str) -> Vec<String> {
            Vec::new()
        }

        fn text_refs_in(&self, text: &str) -> Vec<String> {
            text.split_whitespace()
                .filter(|word| word.starts_with("blob:") && !word.ends_with(".png"))
                .map(str::to_string)
                .collect()
        }

        async fn load(&self, reference: &str) -> Result<ImageData, String> {
            let text = if reference.contains("long") {
                "é".repeat(MAX_INLINE_TEXT)
            } else {
                "body { color: red; }".to_string()
            };
            Ok(ImageData {
                mime: "text/css".to_string(),
                bytes: text.into_bytes().into(),
            })
        }
    }

    #[tokio::test]
    async fn a_text_ref_in_a_user_message_is_inlined_once_and_tool_results_keep_only_the_ref() {
        let mut req = ChatRequest {
            messages: vec![
                ChatMessage::user("compare blob:sha256-a.css"),
                ChatMessage::ToolResult {
                    tool_call_id: "t".into(),
                    content: json!({"ref": "blob:sha256-a.css"}),
                    is_error: false,
                },
                ChatMessage::user("again blob:sha256-a.css"),
            ],
            ..Default::default()
        };
        inline_text(&Texts, &mut req).await;
        let ChatMessage::User { content, .. } = &req.messages[0] else {
            panic!()
        };
        assert!(content
            .ends_with("[file blob:sha256-a.css · text/css · 20 bytes]\nbody { color: red; }"));
        let ChatMessage::ToolResult { content, .. } = &req.messages[1] else {
            panic!()
        };
        assert_eq!(content, &json!({"ref": "blob:sha256-a.css"}));
        let ChatMessage::User { content, .. } = &req.messages[2] else {
            panic!()
        };
        assert_eq!(content, "again blob:sha256-a.css");
    }

    #[tokio::test]
    async fn a_long_text_file_is_cut_at_100_kb_with_a_note() {
        let mut req = ChatRequest {
            messages: vec![ChatMessage::user("read blob:sha256-long.css")],
            ..Default::default()
        };
        inline_text(&Texts, &mut req).await;
        let ChatMessage::User { content, .. } = &req.messages[0] else {
            panic!()
        };
        assert!(content.ends_with("read it with builtin__read_file]"));
        assert!(content.len() < MAX_INLINE_TEXT + 200);
    }

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
                bytes: reference.as_bytes().to_vec().into(),
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
                bytes: Vec::new().into(),
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

    #[tokio::test]
    async fn only_messages_after_the_last_scan_are_read_for_new_refs() {
        let source = source();
        let mut req = ChatRequest {
            messages: vec![
                ChatMessage::user("blob:a.png"),
                ChatMessage::user("blob:b.png"),
            ],
            ..Default::default()
        };
        attach_since(&source, &mut req, 1).await;
        assert_eq!(*source.loaded.lock().unwrap(), ["blob:b.png"]);
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
                bytes: vec![1].into(),
            },
        );
        let req = withhold(Some(&source), "judge", req);
        assert!(req.images.is_empty());
        assert_eq!(req.system, "You judge.");
        assert_eq!(req.withheld_images, ["blob:a.png"]);
        match &req.messages[0] {
            ChatMessage::User { content, images } => {
                assert!(images.is_empty());
                assert_eq!(
                    content,
                    "Look at blob:a.png\n\n[images not sent, because model role \"judge\" doesn't take images: blob:a.png]"
                );
            }
            other => panic!("expected a user message, got {other:?}"),
        }
    }

    #[test]
    fn a_tool_result_that_names_an_image_carries_the_note_beside_it() {
        let req = withhold(
            Some(&source()),
            "judge",
            ChatRequest {
                messages: vec![
                    ChatMessage::ToolResult {
                        tool_call_id: "t1".to_string(),
                        content: json!({"files": [{"ref": "blob:b.png"}]}),
                        is_error: false,
                    },
                    ChatMessage::ToolResult {
                        tool_call_id: "t2".to_string(),
                        content: json!("blob:c.png"),
                        is_error: false,
                    },
                ],
                ..Default::default()
            },
        );
        let notes: Vec<serde_json::Value> = req
            .messages
            .iter()
            .map(|message| match message {
                ChatMessage::ToolResult { content, .. } => content.clone(),
                other => panic!("expected a tool result, got {other:?}"),
            })
            .collect();
        assert_eq!(
            notes,
            [
                json!({"files": [{"ref": "blob:b.png"}], "images_not_sent": withheld_note("judge", &["blob:b.png".to_string()])}),
                json!({"result": "blob:c.png", "images_not_sent": withheld_note("judge", &["blob:c.png".to_string()])}),
            ]
        );
        assert_eq!(req.withheld_images, ["blob:b.png", "blob:c.png"]);
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
