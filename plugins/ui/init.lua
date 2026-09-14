-- maki's default transcript block renderer. Third-party plugins layer over it
-- and delegate with prev().

local LEADING_BREAKS = "^\n+"
local BREAK_LINE = "([^\n]*)\n"
local DONE = "done"
local ERROR = "error"
local THINKING = "thinking"
local SUCCESS_STYLE = "tool_success"
local ERROR_STYLE = "error"
local THINKING_STYLE = "thinking"
local DIM_STYLE = "tool_dim"
local THINKING_HEADER = "thinking> ..."
local THINKING_EXPAND_HINT = " (click to expand)"
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

maki.ui.set_block_renderer(function(prev, block, ctx)
  local lines
  if block.kind == DONE then
    lines = done_lines(block.text)
  elseif block.kind == ERROR then
    lines = plain_lines(block.text, ERROR_STYLE)
  elseif block.kind == THINKING then
    lines = thinking_lines(block, ctx)
  else
    lines = response_lines(block, ctx)
  end
  return lines or prev(block, ctx)
end)
