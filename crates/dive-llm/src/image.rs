use crate::types::Usage;
use crate::LlmError;
use async_trait::async_trait;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageData {
    pub mime: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    Text(String),
    Image(ImageData),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageRequest {
    pub model: String,
    pub parts: Vec<Part>,
    pub aspect_ratio: Option<String>,
    pub size: Option<String>,
}

impl ImageRequest {
    pub fn images(&self) -> usize {
        self.parts
            .iter()
            .filter(|part| matches!(part, Part::Image(_)))
            .count()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageResponse {
    pub parts: Vec<Part>,
    pub usage: Usage,
}

impl ImageResponse {
    pub fn images(&self) -> impl Iterator<Item = &ImageData> {
        self.parts.iter().filter_map(|part| match part {
            Part::Image(image) => Some(image),
            Part::Text(_) => None,
        })
    }

    pub fn text(&self) -> Option<String> {
        let text: Vec<&str> = self
            .parts
            .iter()
            .filter_map(|part| match part {
                Part::Text(text) => Some(text.as_str()),
                Part::Image(_) => None,
            })
            .collect();
        (!text.is_empty()).then(|| text.join("\n"))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageOptions {
    pub aspect_ratios: Vec<&'static str>,
    pub sizes: Vec<&'static str>,
    pub max_images: usize,
}

impl ImageOptions {
    pub fn check(&self, req: &ImageRequest) -> Result<(), LlmError> {
        let refuse = |what: &str, value: &str, supported: &[&str]| {
            let supported = if supported.is_empty() {
                format!("{} doesn't take a {what}; leave it out", req.model)
            } else {
                format!(
                    "{} supports {}",
                    req.model,
                    supported
                        .iter()
                        .map(|value| format!("\"{value}\""))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            Err(LlmError::Unsupported(format!(
                "{what} \"{value}\" isn't supported: {supported}"
            )))
        };
        if let Some(ratio) = &req.aspect_ratio {
            if !self.aspect_ratios.contains(&ratio.as_str()) {
                return refuse("aspect_ratio", ratio, &self.aspect_ratios);
            }
        }
        if let Some(size) = &req.size {
            if !self.sizes.contains(&size.as_str()) {
                return refuse("size", size, &self.sizes);
            }
        }
        if req.images() > self.max_images {
            return Err(LlmError::Unsupported(format!(
                "{} takes at most {} input images, and this request has {}",
                req.model,
                self.max_images,
                req.images()
            )));
        }
        Ok(())
    }
}

#[async_trait]
pub trait ImageProvider: Send + Sync {
    fn options(&self, model: &str) -> ImageOptions;

    async fn generate(&self, req: ImageRequest) -> Result<ImageResponse, LlmError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> ImageOptions {
        ImageOptions {
            aspect_ratios: vec!["1:1", "16:9"],
            sizes: vec!["1K", "2K"],
            max_images: 1,
        }
    }

    fn request() -> ImageRequest {
        ImageRequest {
            model: "m".into(),
            parts: vec![Part::Text("a fox".into())],
            ..ImageRequest::default()
        }
    }

    #[test]
    fn an_unsupported_value_names_the_supported_ones() {
        let req = ImageRequest {
            size: Some("2k".into()),
            ..request()
        };
        let err = options().check(&req).unwrap_err().to_string();
        assert_eq!(
            err,
            "size \"2k\" isn't supported: m supports \"1K\", \"2K\""
        );
        let req = ImageRequest {
            aspect_ratio: Some("4:3".into()),
            ..request()
        };
        assert!(options().check(&req).is_err());
        assert!(options().check(&request()).is_ok());
    }

    #[test]
    fn a_model_without_sizes_says_to_leave_size_out() {
        let options = ImageOptions {
            sizes: Vec::new(),
            ..options()
        };
        let req = ImageRequest {
            size: Some("1K".into()),
            ..request()
        };
        let err = options.check(&req).unwrap_err().to_string();
        assert!(
            err.ends_with("m doesn't take a size; leave it out"),
            "{err}"
        );
    }

    #[test]
    fn too_many_input_images_is_refused() {
        let image = Part::Image(ImageData {
            mime: "image/png".into(),
            bytes: vec![1],
        });
        let mut req = request();
        req.parts.push(image.clone());
        assert!(options().check(&req).is_ok());
        req.parts.push(image);
        assert!(options().check(&req).is_err());
    }
}
