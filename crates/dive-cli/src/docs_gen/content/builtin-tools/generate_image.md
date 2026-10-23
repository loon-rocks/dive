---
summary: Generates images with the image model, or edits the images it's given, and returns each as a file reference.
output:
  description: The images the model drew, plus any text it wrote alongside them.
  type: object
  properties:
    images:
      type: array
      description: One entry per image, in the order they were generated.
      items:
        type: object
        properties:
          ref:
            type: string
            description: The image's file reference, such as `blob:sha256-….png`. Put it in a reply as a markdown image's URL, or pass it to another tool.
          mime:
            type: string
            description: The image's type, such as `image/png`.
          bytes:
            type: integer
            description: The file's size.
          width:
            type: integer
            description: Width in pixels.
          height:
            type: integer
            description: Height in pixels.
          path:
            type: string
            description: Where the image was saved in the workspace, when the call set `path`.
    text:
      type: string
      description: Text the model returned with the images, when it wrote any.
---

Needs an [image model](/getting-started/models#image-models) on the `image` role, and appears in the tool catalog only once one is configured. Pass refs from earlier results in `images` to edit or combine them, with a prompt that says what to change. A value the model doesn't support, such as an `aspect_ratio` or `size` it can't produce, is a tool error that lists the values it does support, so an agent can correct the call and retry.

With `path`, each image is also written into the workspace; when `count` is over 1, later images get `-2`, `-3` and so on before the extension. A path must be relative and stay inside the workspace.
