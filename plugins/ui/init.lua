-- maki's default transcript block renderer. Third-party plugins layer over it
-- and delegate with prev().

local LEADING_BREAKS = "^\n+"
local BREAK_LINE = "([^\n]*)\n"
local DONE = "done"
local ERROR = "error"
local THINKING = "thinking"
local TOOL = "tool"
local SUCCESS_STYLE = "tool_success"
local ERROR_STYLE = "error"
local THINKING_STYLE = "thinking"
local DIM_STYLE = "tool_dim"
local THINKING_HEADER = "thinking> ..."
local THINKING_EXPAND_HINT = " (click to expand)"
local TOOL_INDICATOR = "● "
local TOOL_PREFIX = "> "
local TOOL_PREFIX_STYLE = "tool_prefix"
local TOOL_HEADER_STYLE = "tool"
local TOOL_ANNOTATION_STYLE = "tool_annotation"
local STATUS_IN_PROGRESS = "in_progress"
local HEADER_SNAPSHOT = "snapshot"
local RESPONSES = {
  assistant = { prefix = "maki> ", prefix_style = "assistant_prefix", text_style = "assistant" },
  user = { prefix = "you> ", prefix_style = "user", text_style = "assistant" },
  thinking = { prefix = "thinking> ", prefix_style = THINKING_STYLE, text_style = THINKING_STYLE },
}

-- Mirrors Rust `plain_lines` with an empty prefix: strip leading breaks, then
-- one line per remaining '\n', keeping a trailing empty line.
local function plain_lines(text, style)
  local out = {}
  for line in (text:gsub(LEADING_BREAKS, "") .. "\n"):gmatch(BREAK_LINE) do
    out[#out + 1] = { { line, style } }
  end
  return out
end

-- Mirrors Rust `logical_line_count`: no text is zero lines, otherwise one per
-- '\n' plus the last.
local function logical_lines(text)
  if text == "" then
    return 0
  end
  return select(2, text:gsub("\n", "")) + 1
end

-- The success bubble is bold, which no named style carries, so it needs the
-- resolved style table rather than a name.
local function done_lines(text)
  local style = maki.ui.theme_style(SUCCESS_STYLE)
  if not style then
    return nil
  end
  style.bold = true
  return plain_lines(text, style)
end

local function response_lines(block, ctx)
  local role = RESPONSES[block.kind]
  if not role then
    return nil
  end
  local text_style = maki.ui.theme_style(role.text_style)
  local prefix_style = maki.ui.theme_style(role.prefix_style)
  if not (text_style and prefix_style) then
    return nil
  end
  return maki.ui.transcript_markdown(block.text, ctx.width, {
    prefix = role.prefix,
    text_style = text_style,
    prefix_style = prefix_style,
  })
end

-- The collapsed thinking stand-in is two lines: a hidden header and a footer
-- with the line count and the expand hint.
local function thinking_lines(block, ctx)
  if not block.thinking_collapsed then
    return response_lines(block, ctx)
  end
  local thinking = maki.ui.theme_style(THINKING_STYLE)
  local dim = maki.ui.theme_style(DIM_STYLE)
  if not (thinking and dim) then
    return nil
  end
  local count = logical_lines(block.text)
  return {
    { { THINKING_HEADER, thinking } },
    { { "(" .. count .. " lines)", dim }, { THINKING_EXPAND_HINT, thinking } },
  }
end

-- The header line's leading indicator: a semantic spinner while the tool runs,
-- the native dot once it settles.
local function tool_indicator(status)
  if status == STATUS_IN_PROGRESS then
    return maki.ui.spinner()
  end
  return { TOOL_INDICATOR, status == ERROR and "tool_error" or SUCCESS_STYLE }
end

-- The tool header plus the host's body, with the indicator, prefix, header
-- text (or snapshot spans), annotation and right-info drawn natively. Returns
-- nil when the block carries no structured header, so the caller falls back.
local function tool_lines(block, ctx)
  local tool = block.tool
  local header = tool and tool.header
  if not header then
    return nil
  end

  local spans = { tool_indicator(tool.status) }
  spans[#spans + 1] = { tool.name .. TOOL_PREFIX, TOOL_PREFIX_STYLE }
  if header.kind == HEADER_SNAPSHOT then
    local line = header.lines and header.lines[1]
    if not line then
      return nil
    end
    for _, span in ipairs(line) do
      spans[#spans + 1] = span
    end
  else
    spans[#spans + 1] = { header.text, TOOL_HEADER_STYLE }
  end
  local annotation = header.annotation or block.annotation
  if annotation then
    spans[#spans + 1] = { " (" .. annotation .. ")", TOOL_ANNOTATION_STYLE }
  end
  -- Mirrors the transcript, which only draws right-info once a timestamp
  -- exists, so usage alone stays hidden.
  if tool.timestamp then
    maki.ui.right_info(spans, tool.usage, tool.timestamp, ctx.width)
  end
  return { spans, maki.ui.transcript_tool() }
end

maki.ui.set_block_renderer(function(prev, block, ctx)
  local lines
  if block.kind == DONE then
    lines = done_lines(block.text)
  elseif block.kind == ERROR then
    lines = plain_lines(block.text, ERROR_STYLE)
  elseif block.kind == THINKING then
    lines = thinking_lines(block, ctx)
  elseif block.kind == TOOL then
    lines = tool_lines(block, ctx)
  else
    lines = response_lines(block, ctx)
  end
  return lines or prev(block, ctx)
end)
