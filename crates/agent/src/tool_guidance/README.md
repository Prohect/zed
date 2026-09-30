# Built-in tool guidance defaults

Guidance is the overridable half of a tool's model-facing documentation: the
tool's Rust code owns the non-overridable API contract (schema structure plus
the descriptions its doc comments generate), and each `*.hbs` file here adds to
it. Guidance is always appended, never replaces the contract.

A tool's guidance lives in a directory named after the tool:

```
edit_file/
  &self.hbs          extends the `edit_file` description
  path.hbs           extends the `path` parameter description
  $edits/
    items.hbs        extends the `edits` items description
    $items/
      old_text.hbs   extends `old_text` inside each edit
      new_text.hbs
```

- `&self.hbs` (tool root only) extends the tool's own description.
- `<name>.hbs` extends the description of the schema node named `<name>`, i.e.
  a property or the `items` keyword inside an array node.
- `$<name>/` descends into the schema node named `<name>` so its children can
  be addressed. The `$` distinguishes a schema descent from an organizational
  directory whose files are partials.

Tool and parameter names are conventionally `[A-Za-z0-9_-]`; names containing
`&` or `$` are unsupported and their files are ignored.

Every file is embedded into the binary and shadowed, skills-style, by a
same-named file in the user-global `tool_guidance` config directory
(`~/.config/zed/tool_guidance/`). The flat `tool_guidance/<tool>.hbs` form is
accepted as a legacy alias for `tool_guidance/<tool>/&self.hbs`.

Files are Handlebars templates rendered with the rules-template context
(`available_tools`, `model_name`, `date`, `is_windows`, `is_linux`, `is_macos`,
`sandboxing`). Text is emitted verbatim; `{{!-- ... --}}` comments are stripped
and never reach the model.

Files under organizational (non-`$`) directories are importable as partials
named by their relative path without the extension, `/`-separated on every
platform (`shared/tips.hbs` → `{{> shared/tips}}`).

The `extract_builtin_tool_docs` utility test in `../tool_guidance.rs` dumps the
current code-authored descriptions into this tree as the starting point for
moving prose into files.
