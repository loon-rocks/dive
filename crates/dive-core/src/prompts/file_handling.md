A file ref, `blob:sha256-<hash>.<ext>`, is the only way a file moves between steps, tools and models. Paths and file contents don't: never return a path as a ref, and never pass a file's contents where its ref will do. The extension carries the type, and any type is a file: `.png`, `.json`, `.css`, `.pdf`, `.zip`.

Refs come from exactly these places, and nowhere else:
1. A tool result's `files` list. MCP tools add one for each image, audio, resource or linked file they return.
2. A file declared under `files` on a plan step, or on an exec tool. Pass `{{files.<name>.path}}` to the tool's own output option (Playwright's `filename`, a command's output argument); the file comes back as `{{<step>.files.<name>.ref}}`. For several files declare a pattern such as `page-*.png`, pass `{{files.<name>.dir}}`, and read the list `{{<step>.files.<name>}}`. Each call gets its own directory, so parallel map items never collide.
3. `builtin__save_file`, for text or JSON a step or agent already has: `{name: snapshot.json, content: …}` returns `{ref}`.
4. `builtin__generate_image`, under `images`.

A tool that says it saved a file, or returns a path or a markdown link to one, gave you text, not a ref: declare the file under `files`, or use `builtin__save_file`. No other tool or option makes a ref, so never invent one (there is no `builtin__write_file`); if none of the four fits, say so.

Reading a ref: a model sees an image as the image, a PDF as a document where its provider takes them, and a text file's content when the ref is in its instruction or message. A plan step reads a text file's content with `builtin__read_file` (`{{read.text}}`, or `{{read.json}}` when it parses). An exec tool gets a local path through an input declared `format: blob`.
