import { Fragment, type ReactNode } from "react";

/**
 * Tiny dependency-free markdown renderer for Claude summaries.
 * Supports: # headings, -/* bullets, 1. numbered lists, **bold**, *italic*,
 * `inline code`, horizontal rules, and paragraphs. Everything is rendered as
 * React text nodes, so no HTML from the model is ever injected.
 */
export function Markdown({ source }: { source: string }) {
  return <div className="markdown">{renderBlocks(source)}</div>;
}

type Block =
  | { type: "heading"; level: number; text: string }
  | { type: "ul"; items: string[] }
  | { type: "ol"; items: string[] }
  | { type: "hr" }
  | { type: "p"; text: string };

function parseBlocks(src: string): Block[] {
  const lines = src.replace(/\r\n?/g, "\n").split("\n");
  const blocks: Block[] = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    const trimmed = line.trim();

    if (trimmed === "") {
      i++;
      continue;
    }

    const heading = /^(#{1,6})\s+(.*)$/.exec(trimmed);
    if (heading) {
      blocks.push({
        type: "heading",
        level: heading[1].length,
        text: heading[2].replace(/\s+#+$/, ""),
      });
      i++;
      continue;
    }

    if (/^(-{3,}|\*{3,}|_{3,})$/.test(trimmed)) {
      blocks.push({ type: "hr" });
      i++;
      continue;
    }

    if (/^[-*+]\s+/.test(trimmed)) {
      const items: string[] = [];
      while (i < lines.length && /^\s*[-*+]\s+/.test(lines[i])) {
        items.push(lines[i].replace(/^\s*[-*+]\s+/, ""));
        i++;
        // Continuation lines (indented, not a new bullet) join the previous item.
        while (
          i < lines.length &&
          /^\s{2,}\S/.test(lines[i]) &&
          !/^\s*[-*+]\s+/.test(lines[i]) &&
          !/^\s*\d+[.)]\s+/.test(lines[i])
        ) {
          items[items.length - 1] += " " + lines[i].trim();
          i++;
        }
      }
      blocks.push({ type: "ul", items });
      continue;
    }

    if (/^\d+[.)]\s+/.test(trimmed)) {
      const items: string[] = [];
      while (i < lines.length && /^\s*\d+[.)]\s+/.test(lines[i])) {
        items.push(lines[i].replace(/^\s*\d+[.)]\s+/, ""));
        i++;
        while (
          i < lines.length &&
          /^\s{2,}\S/.test(lines[i]) &&
          !/^\s*[-*+]\s+/.test(lines[i]) &&
          !/^\s*\d+[.)]\s+/.test(lines[i])
        ) {
          items[items.length - 1] += " " + lines[i].trim();
          i++;
        }
      }
      blocks.push({ type: "ol", items });
      continue;
    }

    // Paragraph: consume until blank line or another block starter.
    const para: string[] = [trimmed];
    i++;
    while (i < lines.length) {
      const t = lines[i].trim();
      if (
        t === "" ||
        /^#{1,6}\s+/.test(t) ||
        /^[-*+]\s+/.test(t) ||
        /^\d+[.)]\s+/.test(t) ||
        /^(-{3,}|\*{3,}|_{3,})$/.test(t)
      ) {
        break;
      }
      para.push(t);
      i++;
    }
    blocks.push({ type: "p", text: para.join(" ") });
  }
  return blocks;
}

function renderBlocks(src: string): ReactNode[] {
  return parseBlocks(src).map((block, idx) => {
    switch (block.type) {
      case "heading": {
        const inline = renderInline(block.text);
        switch (block.level) {
          case 1:
            return <h1 key={idx}>{inline}</h1>;
          case 2:
            return <h2 key={idx}>{inline}</h2>;
          case 3:
            return <h3 key={idx}>{inline}</h3>;
          default:
            return <h4 key={idx}>{inline}</h4>;
        }
      }
      case "ul":
        return (
          <ul key={idx}>
            {block.items.map((item, j) => (
              <li key={j}>{renderInline(item)}</li>
            ))}
          </ul>
        );
      case "ol":
        return (
          <ol key={idx}>
            {block.items.map((item, j) => (
              <li key={j}>{renderInline(item)}</li>
            ))}
          </ol>
        );
      case "hr":
        return <hr key={idx} />;
      case "p":
        return <p key={idx}>{renderInline(block.text)}</p>;
    }
  });
}

// Inline tokens: `code`, **bold**, __bold__, *italic*, _italic_
const INLINE_RE = /(`[^`]+`|\*\*[^*]+\*\*|__[^_]+__|\*[^*\s][^*]*\*|_[^_\s][^_]*_)/g;

export function renderInline(text: string): ReactNode {
  const parts = text.split(INLINE_RE);
  if (parts.length === 1) return text;
  return parts.map((part, i) => {
    if (!part) return null;
    if (part.startsWith("`") && part.endsWith("`") && part.length > 2) {
      return <code key={i}>{part.slice(1, -1)}</code>;
    }
    if (
      (part.startsWith("**") && part.endsWith("**") && part.length > 4) ||
      (part.startsWith("__") && part.endsWith("__") && part.length > 4)
    ) {
      return <strong key={i}>{renderInline(part.slice(2, -2))}</strong>;
    }
    if (
      (part.startsWith("*") && part.endsWith("*") && part.length > 2) ||
      (part.startsWith("_") && part.endsWith("_") && part.length > 2)
    ) {
      return <em key={i}>{renderInline(part.slice(1, -1))}</em>;
    }
    return <Fragment key={i}>{part}</Fragment>;
  });
}
