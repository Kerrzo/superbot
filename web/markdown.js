// Keep HTML from messages inert before sanitizing the generated Markdown markup.
const chatMarkdown = new marked.Marked({
  gfm: true,
  breaks: true,
  renderer: {
    html: ({ text }) => esc(text),
    table(token) {
      const table = marked.Renderer.prototype.table.call(this, token);
      return `<div class="markdown-table" tabindex="0" role="region" aria-label="Markdown table">${table}</div>`;
    },
    code(token) {
      const fence = /^ {0,3}(`{3,}|~{3,})[^\n]*\n/.exec(token.raw);
      const language = token.lang?.trim().split(/\s+/, 1)[0].toLowerCase() || "";
      let code = token.text;
      if (fence) {
        // Marked's code token can omit the final newline. Use the fence body
        // to preserve trailing line breaks while retaining parsed indentation.
        const body = token.raw.slice(fence[0].length);
        const close = new RegExp(`^ {0,3}${fence[1][0]}{${fence[1].length},}[ \t]*$`, "m").exec(body);
        const source = close ? body.slice(0, close.index) : body;
        code = code.replace(/\n+$/, "") + (source.match(/\n+$/)?.[0] || "");
      }
      const highlighted = language && code.length <= 64000 && hljs.getLanguage(language)
        ? hljs.highlight(code, { language, ignoreIllegals: true }).value
        : esc(code);
      return `<div class="markdown-code">
        <div class="code-toolbar"><span>${esc(language)}</span>
          <button type="button" class="copy-code" aria-label="Copy code">Copy</button>
        </div>
        <pre tabindex="0" aria-label="Code"><code>${highlighted}</code></pre>
      </div>`;
    },
  },
});

DOMPurify.addHook("afterSanitizeAttributes", (node) => {
  if (node.tagName === "A" && node.hasAttribute("href")) {
    if (safeMarkdownUrl(node.getAttribute("href"), ["http:", "https:", "mailto:"])) {
      node.setAttribute("target", "_blank");
      node.setAttribute("rel", "noopener noreferrer");
    } else {
      node.removeAttribute("href");
    }
  }
  if (node.tagName === "IMG") {
    if (!safeMarkdownUrl(node.getAttribute("src"), ["http:", "https:"])) node.removeAttribute("src");
    node.setAttribute("loading", "lazy");
    node.setAttribute("referrerpolicy", "no-referrer");
  }
});

function safeMarkdownUrl(value, protocols) {
  if (!value) return false;
  try { return protocols.includes(new URL(value, document.baseURI).protocol); }
  catch { return false; }
}

function md(source) {
  try {
    return DOMPurify.sanitize(chatMarkdown.parse(source), {
      ALLOWED_TAGS: [
        "a", "blockquote", "br", "button", "code", "del", "div", "em", "h1", "h2", "h3",
        "h4", "h5", "h6", "hr", "img", "input", "li", "ol", "p", "pre", "s", "span", "strong",
        "table", "tbody", "td", "th", "thead", "tr", "ul",
      ],
      ALLOWED_ATTR: ["align", "alt", "checked", "class", "disabled", "href", "role", "src", "start", "tabindex", "title", "type"],
      ALLOW_DATA_ATTR: false,
    });
  } catch {
    return esc(source).replace(/\n/g, "<br>");
  }
}

async function writeClipboard(text) {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    // The selection-based fallback also works on many plain HTTP installations.
  }
  const focus = document.activeElement;
  const selection = window.getSelection();
  const ranges = Array.from({ length: selection.rangeCount }, (_, i) => selection.getRangeAt(i).cloneRange());
  const inputRange = focus?.selectionStart == null
    ? null : [focus.selectionStart, focus.selectionEnd, focus.selectionDirection];
  const field = document.createElement("textarea");
  field.className = "clipboard-field";
  field.value = text;
  field.readOnly = true;
  document.body.append(field);
  field.select();
  try {
    return document.execCommand("copy");
  } catch {
    return false;
  } finally {
    field.remove();
    focus?.focus({ preventScroll: true });
    if (inputRange) focus.setSelectionRange(...inputRange);
    else if (ranges.length) {
      selection.removeAllRanges();
      ranges.forEach((range) => selection.addRange(range));
    }
  }
}

const copyFeedback = new WeakMap();

async function copyText(button, text) {
  const success = await writeClipboard(text);
  const code = button.classList.contains("copy-code");
  const label = success ? "Copied" : "Copy failed";
  const status = document.getElementById("copy-status");
  status.textContent = success ? (code ? "Code copied" : "Response copied") : "Copy failed. Select the text and copy it manually.";
  status.classList.toggle("copy-error", !success);
  if (code) button.textContent = label;
  else button.dataset.copyState = success ? "success" : "failure";
  button.setAttribute("aria-label", label);
  button.title = label;
  clearTimeout(copyFeedback.get(button));
  copyFeedback.set(button, setTimeout(() => {
    if (code) button.textContent = "Copy";
    delete button.dataset.copyState;
    const original = code ? "Copy code" : "Copy response";
    button.setAttribute("aria-label", original);
    button.title = original;
  }, 1500));
  clearTimeout(copyFeedback.get(status));
  copyFeedback.set(status, setTimeout(() => {
    status.classList.remove("copy-error");
    status.textContent = "";
  }, 2400));
}
