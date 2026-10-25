import { Electroview } from "electrobun/view";
import type { LauncherRPC, Overview } from "../shared/rpc";

const rpc = Electroview.defineRPC<LauncherRPC>({
	maxRequestTime: 60_000,
	handlers: { requests: {}, messages: {} },
});
const view = new Electroview({ rpc });
const ask = view.rpc!.request;

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const status = $<HTMLParagraphElement>("status");

function say(text: string | undefined, bad = false) {
	status.textContent = text ?? "";
	status.classList.toggle("bad", bad);
}

function el<K extends keyof HTMLElementTagNameMap>(tag: K, props: Partial<HTMLElementTagNameMap[K]> & { class?: string } = {}, ...kids: (Node | string)[]) {
	const e = document.createElement(tag);
	const { class: cls, ...rest } = props as any;
	if (cls) e.className = cls;
	Object.assign(e, rest);
	e.append(...kids);
	return e;
}

function render(o: Overview) {
	const recents = $("recents");
	recents.replaceChildren(
		...o.recents.map((r) => {
			const open = o.open.includes(`file:${r.path}`);
			const li = el(
				"li",
				{ class: r.exists ? "" : "missing" },
				el(
					"div",
					{ class: "main", title: r.path, onclick: () => r.exists && run(ask.openPath({ path: r.path })) },
					el("div", { class: "name" }, r.name),
					el("div", { class: "path" }, r.path),
				),
				...(open ? [el("span", { class: "badge" }, "open")] : []),
				el("button", { class: "quiet", title: "Forget this file", onclick: async () => render(await ask.forgetRecent({ path: r.path })) }, "×"),
			);
			return li;
		}),
	);

	const conns = $("connections");
	conns.replaceChildren(
		...o.connections.map((c) =>
			el(
				"li",
				{},
				el(
					"div",
					{ class: "main", onclick: () => run(ask.openConnection({ id: c.id })) },
					el("div", { class: "name" }, c.name),
					el("div", { class: "path" }, c.url + (c.tenant ? ` · ${c.tenant}` : "")),
				),
				el("span", { class: "badge" }, c.hasToken ? "token in keychain" : "no token"),
				el("button", { class: "quiet", title: "Remove this connection and its token", onclick: async () => render(await ask.removeConnection({ id: c.id })) }, "×"),
			),
		),
	);
	$("keychain").textContent = `Tokens are stored with ${o.keychain}.`;
}

async function run(p: Promise<{ ok: boolean; message?: string }>) {
	say("");
	const r = await p;
	if (r.message) say(r.message, !r.ok);
	render(await ask.overview({}));
}

$("open").addEventListener("click", () => run(ask.chooseFile({})));

$<HTMLFormElement>("add").addEventListener("submit", async (e) => {
	e.preventDefault();
	const f = e.currentTarget as HTMLFormElement;
	const v = (n: string) => (f.elements.namedItem(n) as HTMLInputElement).value;
	const r = await ask.saveConnection({ name: v("name"), url: v("url"), token: v("token"), tenant: v("tenant") });
	if (!r.ok) return say(r.message, true);
	f.reset();
	say("Saved. The token is in the keychain.");
	render(r.overview!);
});

window.addEventListener("focus", async () => render(await ask.overview({})));
render(await ask.overview({}));
