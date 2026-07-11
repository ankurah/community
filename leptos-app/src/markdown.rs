//! Markdown subset renderer for chat messages (#9).
//!
//! Parses with `pulldown-cmark` and maps events to **typed Leptos view nodes**
//! — never `innerHTML`. Raw HTML events (`Html` / `InlineHtml`) are dropped
//! entirely, so no author-controlled markup ever reaches the DOM (XSS posture;
//! plain text nodes are always DOM-escaped by Leptos).
//!
//! Supported subset: **bold**, *italic*, `inline code`, fenced/indented code
//! blocks (language tag captured on `data-lang` for later tooling), links, and
//! a few graceful degradations so message content is never lost:
//! - headings render as a bold line (chat has no heading hierarchy)
//! - block quotes render with a muted left rule
//! - list items render with a literal bullet / number prefix
//! - images are never fetched; their alt text renders as plain text
//!
//! Links render as real anchors with `target="_blank"` and
//! `rel="noopener noreferrer"`, but only for http(s) destinations — any other
//! scheme (`javascript:`, `data:`, …) renders as plain text. URL unfurling is
//! deliberately out of scope here (#20).

use leptos::prelude::*;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag};

/// Fast path: skip parsing entirely when the text contains none of the
/// characters that can open a construct we render. `.` is deliberately not
/// gated (every sentence has one), so a *lone* ordered list ("1. foo") renders
/// literally — it only becomes a list when combined with other markdown.
fn has_markdown(text: &str) -> bool {
    text.bytes().any(|b| matches!(b, b'*' | b'_' | b'`' | b'[' | b'<' | b'>' | b'#' | b'-' | b'+'))
}

/// Render message text: plain text node when no markdown characters are
/// present, otherwise the parsed subset. The result always lives inside the
/// row's `.messageText` div, so the virtual-scroll DOM contract is untouched.
pub fn render_message(text: &str) -> AnyView {
    if !has_markdown(text) {
        return text.to_string().into_any();
    }

    let mut stack = vec![Frame::new(FrameKind::Root)];

    for event in Parser::new_ext(text, Options::empty()) {
        match event {
            Event::Start(tag) => {
                let kind = match tag {
                    Tag::Paragraph => FrameKind::Paragraph,
                    Tag::Heading { .. } => FrameKind::Heading,
                    Tag::BlockQuote(_) => FrameKind::Quote,
                    Tag::CodeBlock(kind) => FrameKind::Code {
                        lang: match kind {
                            CodeBlockKind::Fenced(info) => {
                                info.split_whitespace().next().map(str::to_string).filter(|l| !l.is_empty())
                            }
                            CodeBlockKind::Indented => None,
                        },
                        text: String::new(),
                    },
                    Tag::List(start) => FrameKind::List { numbering: start },
                    Tag::Item => FrameKind::Item,
                    Tag::Emphasis => FrameKind::Emphasis,
                    Tag::Strong => FrameKind::Strong,
                    Tag::Link { dest_url, .. } => FrameKind::Link { href: safe_href(&dest_url) },
                    // Image URLs are never fetched; the frame renders alt text only.
                    Tag::Image { .. } => FrameKind::Image,
                    // Anything else (tables, footnotes, html blocks, …) passes its
                    // children through unstyled so content is never dropped. The
                    // raw-HTML *events* inside an HtmlBlock are still discarded.
                    _ => FrameKind::Passthrough,
                };

                let mut frame = Frame::new(kind);
                if matches!(frame.kind, FrameKind::Item) {
                    frame.children.push(item_marker(&mut stack).into_any());
                }
                stack.push(frame);
            }
            Event::End(_) => {
                // Starts/ends are balanced (parser guarantee); the guards are belt.
                if stack.len() > 1 {
                    if let Some(frame) = stack.pop() {
                        let view = frame.into_view();
                        if let Some(parent) = stack.last_mut() {
                            parent.children.push(view);
                        }
                    }
                }
            }
            Event::Text(t) => push_text(&mut stack, &t),
            Event::Code(t) => {
                if let Some(top) = stack.last_mut() {
                    top.children.push(view! { <code class="mdCode">{t.to_string()}</code> }.into_any());
                }
            }
            // `.messageText` is `white-space: pre-wrap`, so a newline text node
            // is a line break.
            Event::SoftBreak | Event::HardBreak => push_text(&mut stack, "\n"),
            // XSS posture: raw HTML is dropped, never rendered.
            Event::Html(_) | Event::InlineHtml(_) => {}
            // Rules ("---") are noise in chat; requires other markdown present
            // to even parse as one (see has_markdown).
            Event::Rule => {}
            // Math / footnotes / task lists need Options we don't enable.
            _ => {}
        }
    }

    // Defensive unwind (should be a no-op given balanced events).
    while stack.len() > 1 {
        let frame = stack.pop().expect("len checked");
        let view = frame.into_view();
        if let Some(parent) = stack.last_mut() {
            parent.children.push(view);
        }
    }
    stack.pop().map(|root| root.children.into_any()).unwrap_or_else(|| ().into_any())
}

/// One open markdown construct; children accumulate until its `End` event.
struct Frame {
    kind: FrameKind,
    children: Vec<AnyView>,
}

enum FrameKind {
    Root,
    Paragraph,
    /// Headings render as a bold line — chat messages have no heading levels.
    Heading,
    Quote,
    Strong,
    Emphasis,
    /// `href` is `None` for non-http(s) schemes: children render as plain text.
    Link {
        href: Option<String>,
    },
    /// Alt text renders as plain text; the image URL is discarded (never fetched).
    Image,
    List {
        /// `Some(n)` for ordered lists: the number the *next* item takes.
        numbering: Option<u64>,
    },
    Item,
    /// Code block: raw text accumulates here instead of becoming child views.
    Code {
        lang: Option<String>,
        text: String,
    },
    /// Unstyled container: children flow through to the parent.
    Passthrough,
}

impl Frame {
    fn new(kind: FrameKind) -> Self { Self { kind, children: Vec::new() } }

    /// Close the frame into a view. Container-ish kinds return their children
    /// bare (a fragment); styled kinds wrap them in a typed element.
    fn into_view(self) -> AnyView {
        let Frame { kind, children } = self;
        match kind {
            FrameKind::Root | FrameKind::Passthrough | FrameKind::Image | FrameKind::List { .. } => children.into_any(),
            FrameKind::Paragraph => view! { <p class="mdP">{children}</p> }.into_any(),
            FrameKind::Heading => view! { <p class="mdP mdHeading">{children}</p> }.into_any(),
            FrameKind::Quote => view! { <div class="mdQuote">{children}</div> }.into_any(),
            FrameKind::Strong => view! { <strong>{children}</strong> }.into_any(),
            FrameKind::Emphasis => view! { <em>{children}</em> }.into_any(),
            FrameKind::Item => view! { <div class="mdLi">{children}</div> }.into_any(),
            FrameKind::Link { href: Some(href) } => {
                view! {
                    <a class="mdLink" href=href target="_blank" rel="noopener noreferrer">
                        {children}
                    </a>
                }
                .into_any()
            }
            FrameKind::Link { href: None } => children.into_any(),
            FrameKind::Code { lang, text } => {
                // The parser leaves a trailing newline on code block bodies.
                let code = text.strip_suffix('\n').unwrap_or(&text).to_string();
                view! {
                    <pre class="mdCodeBlock" data-lang=lang>
                        <code>{code}</code>
                    </pre>
                }
                .into_any()
            }
        }
    }
}

/// Append text to the top frame — into the raw buffer for code blocks,
/// as a DOM text node everywhere else.
fn push_text(stack: &mut [Frame], s: &str) {
    match stack.last_mut() {
        Some(Frame { kind: FrameKind::Code { text, .. }, .. }) => text.push_str(s),
        Some(frame) => frame.children.push(s.to_string().into_any()),
        None => {}
    }
}

/// Literal "• " / "3. " marker for a list item, indented two spaces per
/// nesting level. Advances the parent's ordered-list counter.
fn item_marker(stack: &mut [Frame]) -> String {
    let depth = stack.iter().filter(|f| matches!(f.kind, FrameKind::List { .. })).count();
    let indent = "  ".repeat(depth.saturating_sub(1));
    match stack.last_mut().map(|f| &mut f.kind) {
        Some(FrameKind::List { numbering: Some(n) }) => {
            let marker = format!("{indent}{n}. ");
            *n += 1;
            marker
        }
        _ => format!("{indent}\u{2022} "),
    }
}

/// Only http(s) URLs become anchors; every other scheme (javascript:, data:,
/// relative, …) renders as plain text.
fn safe_href(url: &str) -> Option<String> {
    let lower = url.trim().to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://")).then(|| url.trim().to_string())
}
