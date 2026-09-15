---
title: Lua renderer block contract
weight: 80
---

`maki.ui.set_block_renderer` receives structured transcript blocks. This contract is versioned with Maki. Additive fields may appear in a compatible release. Renderer plugins must ignore fields they do not use and delegate blocks they do not own.

## Tool projections

A tool block may include `block.tool.diff` or `block.tool.grep`. They contain source data for a Lua renderer. They are separate from the temporary native tool-body marker.

### Plain tool bodies

`block.tool.body` is present for plain, Markdown, directory, todo, and legacy batch output. Unsupported and interactive collapsed bodies use the native tool marker until action dispatch is available.

```lua
block.tool.body = {
  kind = "plain", -- "plain", "markdown", "read_dir", "batch", or "todo_list"
  text = "raw source output", -- text bodies
  legacy = true, -- batch only
  display = {
    expanded = false,
    limit = { configured = 20, visible_lines = 20, total_lines = 42 },
    truncation = {
      head_hidden = 0, tail_hidden = 22,
      visible_start = 1, visible_end = 20,
    },
  },
}
```

Code blocks use `block.tool.code`. `input` normalizes `ToolInput::Code` and legacy `ToolInput::Script` to `{ language, code, display }`. Static read output uses `{ kind = "read_code", path, start_line, lines, total_lines, instructions, display }`. Each present section has its own display state and truncation metadata. A missing code output is `output = nil`.

Todo bodies use `items = { { content, status, priority, marker }, ... }` rather than `text`. `status` and host-derived `marker` are one of `pending`, `in_progress`, `completed`, or `cancelled`. Use `maki.ui.todo_marker(marker)` to obtain the host-owned glyph and `todo.<status>` semantic style. `priority` is `high`, `medium`, or `low`; use the corresponding `todo.priority.<priority>` group for its layout. Empty todo bodies provide `empty = { label = "No todos.", group = "todo.empty" }`. `visible_start` and `visible_end` are 1-based inclusive logical source-line indices for text bodies and item indices for todo. `truncation` is absent when no entries are hidden. When present, `head_hidden + visible_lines + tail_hidden = total_lines`. A 0/0 visible window is only valid for an empty source or an explicit empty selection.

### Diff

```lua
block.tool.diff = {
  path = "src/lib.rs",
  summary = "Updated src/lib.rs",
  mode = "edit", -- "edit", "new", or "delete"
  hunks = {
    {
      old_start = 4, old_count = 2,
      new_start = 4, new_count = 3,
      lines = {
        {
          kind = "remove", text = "old text", old_line = 4,
          emphasis = { { start_byte = 0, end_byte = 3, kind = "changed" } },
          no_newline_at_eof = false,
        },
      },
    },
  },
}
```

Hunk starts and line numbers are 1-based. An empty old or new hunk side has start `0` and count `0`. `text` has no trailing newline. `no_newline_at_eof` represents the source EOF state. `emphasis` ranges are half-open UTF-8 byte columns into `text`. They belong to the old side for `remove` and the new side for `add`. Context lines have both line numbers and no emphasis.

### Grep

```lua
block.tool.grep = {
  entries = {
    {
      path = "src/lib.rs",
      display = "src/lib.rs",
      groups = {
        {
          lines = {
            {
              line = 12,
              text = "let needle = true;",
              is_match = true,
              ranges = { { start_byte = 4, end_byte = 10 } },
            },
          },
        },
      },
    },
  },
}
```

`line` is 1-based. `ranges` are validated half-open UTF-8 byte columns into `text`. `is_match` preserves grep's source match-event meaning. It can be true when a multiline match has no range in this displayed line, so it is not derived from `ranges`.
