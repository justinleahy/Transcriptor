import type { ReactNode } from "react";

/**
 * Render an FTS snippet safely. The backend only emits <b>..</b> marks; every
 * other tag is stripped and all text is rendered as text nodes (never HTML).
 */
export function renderSnippet(snippet: string): ReactNode[] {
  const withoutOtherTags = snippet.replace(/<(?!\/?b\s*>)[^>]*>/gi, "");
  const parts = withoutOtherTags.split(/(<b\s*>|<\/b\s*>)/i);
  const nodes: ReactNode[] = [];
  let bold = false;
  parts.forEach((part, i) => {
    if (/^<b\s*>$/i.test(part)) {
      bold = true;
      return;
    }
    if (/^<\/b\s*>$/i.test(part)) {
      bold = false;
      return;
    }
    if (!part) return;
    const text = decodeEntities(part);
    nodes.push(bold ? <b key={i}>{text}</b> : <span key={i}>{text}</span>);
  });
  return nodes;
}

function decodeEntities(s: string): string {
  return s
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&#39;/g, "'")
    .replace(/&amp;/g, "&");
}
