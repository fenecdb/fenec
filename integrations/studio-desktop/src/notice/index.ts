// The notice window's page. What it shows is data the main process put in
// `window.__notice` (windows.ts), drawn with textContent: what a server said
// is text here, never markup. The page is sandboxed, so it has no RPC; a
// button tells the main process which one it was through the event-only
// bridge, and the main process closes the window.

import type { Notice } from "../shared/rpc.ts";

const notice = (window as unknown as { __notice?: Notice }).__notice ?? { title: "fenec studio", message: "" };
const host = (m: unknown) => (window as unknown as { __electrobunSendToHost?: (m: unknown) => void }).__electrobunSendToHost?.(m);
const send = (id: string) => host({ type: "notice", id });

const text = (id: string, value: string | undefined) => {
	const el = document.getElementById(id)!;
	el.textContent = value ?? "";
	el.hidden = !value;
};
text("title", notice.title);
text("message", notice.message);
text("hint", notice.hint);
text("detail", notice.detail);
document.title = notice.title;

const buttons = document.getElementById("buttons")!;
const actions = notice.actions ?? [];
const button = (id: string, label: string, primary: boolean) => {
	const b = document.createElement("button");
	b.textContent = label;
	b.id = `notice-${id}`;
	if (primary) b.className = "primary";
	b.addEventListener("click", () => send(id));
	buttons.append(b);
	return b;
};
button("ok", actions.length ? "Close" : "OK", actions.length === 0);
const first = actions.map((a, i) => button(a.id, a.label, i === 0))[0];
(first ?? (buttons.firstElementChild as HTMLButtonElement)).focus();

window.addEventListener("keydown", (e: KeyboardEvent) => {
	if (e.key === "Escape") send("ok");
});

// Drawn: the checks wait for this before they press a button.
host({ type: "notice-ready" });
