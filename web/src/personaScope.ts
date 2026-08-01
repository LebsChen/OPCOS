// A persona is "project-scoped" only when it's code-family: an explicit directory the user
// picks, sessions grouped by project in the sidebar. Everything else (knowledge, chat) runs on
// a transparent per-conversation scratch dir, with real folders added as roots when needed —
// no folder gate, ever. (The old workspace enum — git/project/deliverable/none — collapsed
// into family; owner decision 2026-07-03, UX-DECISIONS §16.)
export function isProjectScoped(p?: {
  workspace?: string;
  family?: string;
}): boolean {
  return p?.family === "code";
}

// Persona naming: the product is "OpenWorker"; the personas are a "OPCOS" family — OPCOS
// (general), Code OPCOS, Ops OPCOS. In lists/chrome we use the SHORT label (OPCOS / Code /
// Ops); the persona detail page uses the FULL family name. Backend names are left untouched (the
// API + tests keep "OpenWorker" / "Ops OPCOS"); this is purely the display layer.

// Short label for the sidebar + top bar: "OPCOS" / "Code" / "Ops" / "Chat".
export function shortPersonaName(name?: string, id?: string): string {
  if (id === "cowork") return "OPCOS";
  const n = (name || id || "").trim();
  return n.replace(/\s*mode$/i, "").trim() || n;
}

// Full family name for the persona detail page: "OPCOS" / "Code OPCOS" / "Ops OPCOS".
// Chat isn't a mode — left as-is.
export function fullPersonaName(name?: string, id?: string): string {
  if (id === "cowork") return "OPCOS";
  const n = (name || id || "").trim();
  if (id === "chat" || !n) return n;
  return /mode$/i.test(n) ? n : `${n} OPCOS`;
}
