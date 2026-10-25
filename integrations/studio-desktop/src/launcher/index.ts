import { Electroview } from "electrobun/view";
import type { Connection, LauncherRPC, Overview, Result } from "../shared/rpc.ts";
import { originOf, sendsTokenInTheClear } from "../bun/remote.ts";

const rpc = Electroview.defineRPC<LauncherRPC>({
	maxRequestTime: 60_000,
	handlers: {
		requests: {},
		// A window opened or closed, a file was used: what is listed has changed.
		messages: { changed: () => void refresh() },
	},
});
const view = new Electroview({ rpc });
const ask = view.rpc!.request;

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const status = $<HTMLParagraphElement>("status");
const form = $<HTMLFormElement>("add");

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

let editing: string | null = null;

function render(o: Overview) {
	const recents = $("recents");
	recents.replaceChildren(
		...o.recents.map((r) => {
			const open = o.open.includes(`file:${r.path}`);
			return el(
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
				...(o.open.includes(`remote:${c.id}`) ? [el("span", { class: "badge" }, "open")] : []),
				el("span", { class: "badge" }, c.hasToken ? "token in keychain" : "no token"),
				el("button", { class: "quiet", title: "Change this connection or its token", onclick: () => edit(c) }, "Edit"),
				el("button", { class: "quiet", title: "Remove this connection and its token", onclick: async () => render(await ask.removeConnection({ id: c.id })) }, "×"),
			),
		),
	);
	$("keychain").textContent = `Tokens are stored in ${o.keychain}.`;
}

/** The form, ready to change a saved connection: its token stays unless a new one is typed. */
function edit(c: Connection) {
	editing = c.id;
	const f = (n: string) => form.elements.namedItem(n) as HTMLInputElement;
	f("name").value = c.name;
	f("url").value = c.url;
	f("tenant").value = c.tenant ?? "";
	f("token").value = "";
	f("token").placeholder = c.hasToken ? "unchanged: type one to replace it" : "kept in the system keychain";
	$("save").textContent = "Save changes";
	$("cancel-edit").hidden = false;
	warnClear();
	f("token").focus();
}

function stopEditing() {
	editing = null;
	form.reset();
	(form.elements.namedItem("token") as HTMLInputElement).placeholder = "kept in the system keychain";
	$("save").textContent = "Save connection";
	$("cancel-edit").hidden = true;
	warnClear();
}

/** Plain http to another machine sends the token in the clear: say so where it is typed. */
function warnClear() {
	let clear = false;
	try {
		clear = sendsTokenInTheClear(originOf((form.elements.namedItem("url") as HTMLInputElement).value));
	} catch {
		// Not an address yet.
	}
	$("clear").hidden = !clear;
}

async function refresh() {
	const o = await ask.overview({});
	render(o);
	// A notice's "Edit connection" asked for this one.
	const wanted = o.edit && o.connections.find((c) => c.id === o.edit);
	if (wanted) edit(wanted);
}

async function run(p: Promise<Result>) {
	say("");
	const r = await p;
	// Said already by a window of the app's own: not twice.
	if (r.message && !r.shown) say(r.message, !r.ok);
	await refresh();
}

$("open").addEventListener("click", () => run(ask.chooseFile({})));
$("new").addEventListener("click", () => run(ask.newDatabase({})));
$("cancel-edit").addEventListener("click", stopEditing);
(form.elements.namedItem("url") as HTMLInputElement).addEventListener("input", warnClear);

form.addEventListener("submit", async (e) => {
	e.preventDefault();
	const v = (n: string) => (form.elements.namedItem(n) as HTMLInputElement).value;
	const r = await ask.saveConnection({ id: editing ?? undefined, name: v("name"), url: v("url"), token: v("token"), tenant: v("tenant") });
	if (!r.ok) return say(r.message, true);
	const changedToken = v("token") !== "";
	const wasEditing = editing !== null;
	stopEditing();
	say(changedToken ? "Saved. The token is in the keychain." : wasEditing ? "Saved." : "Saved, with no token.");
	render(r.overview!);
});

window.addEventListener("focus", () => void refresh());
await refresh();
