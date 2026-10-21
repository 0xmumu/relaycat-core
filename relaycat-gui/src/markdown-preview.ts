import createDOMPurify from "dompurify";
import MarkdownIt from "markdown-it";

const markdown = new MarkdownIt({
  breaks: false,
  html: false,
  linkify: false,
  typographer: false,
});

markdown.renderer.rules.image = (tokens, index) => {
  const alt = markdown.utils.escapeHtml(tokens[index].content || "image");
  return `<span class="ws-md-image-placeholder">[Image: ${alt}]</span>`;
};
markdown.renderer.rules.link_open = () => '<span class="ws-md-link">';
markdown.renderer.rules.link_close = () => "</span>";

function decorateTaskItems(html: string): string {
  return html.replace(
    /<li>\[([ xX])\]\s*/g,
    (_match, checked: string) =>
      `<li class="ws-md-task"><span class="ws-md-task-box" aria-hidden="true">${checked.toLowerCase() === "x" ? "✓" : ""}</span>`,
  );
}

export function renderSafeMarkdown(source: string): string {
  const rendered = decorateTaskItems(markdown.render(source));
  if (typeof window === "undefined") return rendered;
  const purifier = createDOMPurify(window);
  return purifier.sanitize(rendered, {
    FORBID_TAGS: ["a", "img", "script", "style", "iframe", "object", "embed"],
    FORBID_ATTR: ["href", "src", "srcset", "style"],
  });
}

let svgPurifier: ReturnType<typeof createDOMPurify> | null = null;

/**
 * Second sanitization pass for untrusted SVG markup before it is assigned to
 * `innerHTML`. The regex pre-sanitizer in developer-preview strips the obvious
 * active content, but regex sanitizers are fragile against mutation-based
 * bypasses, so the markup also goes through DOMPurify's SVG profile.
 */
export function purifySvgMarkup(markup: string): string {
  if (typeof window === "undefined") return markup;
  svgPurifier ??= createDOMPurify(window);
  return svgPurifier.sanitize(markup, {
    USE_PROFILES: { svg: true, svgFilters: true },
  });
}
