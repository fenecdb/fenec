// The main window: databases on this Mac, the servers the app runs for them,
// saved remote connections, and backups. Everything it shows comes from the
// main process (`overview`), and everything it does is asked of it
// (shared/rpc.ts): the page holds no token but a served database's, and that
// only while its serve sheet shows it.

import { Electroview } from "electrobun/view";
import type { Connection, DatabaseRow, Figures, Image, LauncherRPC, Overview, Result, ServeSettings, Snippets, Sync } from "../shared/rpc.ts";
import { originOf, sendsTokenInTheClear } from "../bun/remote.ts";
import { deadShare, formatBytes, formatCount, formatSeconds } from "../bun/metrics.ts";
import { DEFAULT_SETTINGS, parsePort, SYNC_NOTES } from "../bun/serving.ts";

const rpc = Electroview.defineRPC<LauncherRPC>({
	maxRequestTime: 35 * 60_000,
	handlers: {
		requests: {},
		// A window opened or closed, a server started: what is listed has changed.
		messages: { changed: () => void refresh() },
	},
});
const view = new Electroview({ rpc });
const ask = view.rpc!.request;

const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;
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

/** Shows `doing` while `p` takes its time, then what it answered (unless the main process said it already). */
async function run(p: Promise<Result>, doing = ""): Promise<Result> {
	say(doing);
	const r = await p;
	say(r.message && !r.shown ? r.message : "", !r.ok && !r.shown);
	await refresh();
	return r;
}

// ------------------------------------------------------------------ time and words

const pad = (n: number) => String(n).padStart(2, "0");
function when(ms: number | null): string {
	if (!ms) return "—";
	const d = new Date(ms);
	const today = new Date();
	const days = Math.round((new Date(today.toDateString()).getTime() - new Date(d.toDateString()).getTime()) / 86_400_000);
	if (days === 0) return `Today, ${pad(d.getHours())}:${pad(d.getMinutes())}`;
	if (days === 1) return "Yesterday";
	return d.toLocaleDateString("en-US", { month: "short", day: "numeric", ...(d.getFullYear() !== today.getFullYear() ? { year: "numeric" } : {}) });
}
function since(ms: number): string {
	const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
	if (s < 60) return `${s} s`;
	const m = Math.floor(s / 60);
	if (m < 60) return `${m} min`;
	const h = Math.floor(m / 60);
	if (h < 48) return `${h} h ${m % 60} min`;
	return `${Math.floor(h / 24)} days`;
}
const tilde = (path: string, home: string) => (home && path.startsWith(home) ? `~${path.slice(home.length)}` : path);

// ------------------------------------------------------------------ sections

const SECTIONS = { databases: "databases", servers: "servers", connections: "connections-view", backups: "backups" } as const;
type Section = keyof typeof SECTIONS;
let section: Section = "databases";

function show(s: Section) {
	section = s;
	for (const [name, id] of Object.entries(SECTIONS)) $(id).hidden = name !== s;
	for (const b of document.querySelectorAll<HTMLButtonElement>(".nav")) {
		if (b.dataset["section"] === s) b.setAttribute("aria-current", "page");
		else b.removeAttribute("aria-current");
	}
	say("");
	void tick();
}
for (const b of document.querySelectorAll<HTMLButtonElement>(".nav")) b.addEventListener("click", () => show(b.dataset["section"] as Section));

// ------------------------------------------------------------------ state

let o: Overview | null = null;
let selected: string | null = null;
let selectedServer: string | null = null;
let home = "";
const live = new Map<string, Figures>();

const row = (path: string | null) => o?.databases.find((d) => d.path === path) ?? null;
const isServed = (d: DatabaseRow) => d.served !== null && d.served.state !== "stopped";

async function refresh() {
	o = await ask.overview({});
	home = o.home;
	// The first database is the one the details are about until another is picked.
	if (!selected && o.databases[0]) selected = o.databases[0].path;
	render();
	// A notice's "Edit connection" asked for this one; New database's "Serve it" for that one.
	const wanted = o.edit && o.connections.find((c) => c.id === o!.edit);
	if (wanted) {
		show("connections");
		edit(wanted);
	}
	if (o.serve) void openSheet(o.serve);
}

function render() {
	if (!o) return;
	$("bundled").textContent = o.bundled;
	const running = o.databases.filter((d) => d.served?.state === "running").length;
	$("running-count").textContent = running ? `${running} running` : "";
	renderDatabases();
	renderServers();
	renderConnections();
	renderBackups();
	$("keychain").textContent = `Tokens are stored in ${o.keychain}.`;
}

// ------------------------------------------------------------------ databases

const find = $<HTMLInputElement>("find");
find.addEventListener("input", () => renderDatabases());

function serverCell(d: DatabaseRow): HTMLElement {
	if (d.served?.state === "running" && d.served.url) return el("span", { class: "pill on mono" }, el("span", { class: "dot on" }), `:${new URL(d.served.url).port}`);
	if (d.served && d.served.state !== "stopped") return el("span", { class: "muted" }, d.served.state === "starting" ? "Starting…" : "Stopping…");
	if (d.running === "window") return el("span", { class: "muted" }, "Open in a window");
	if (d.running === "task") return el("span", { class: "muted" }, "Busy");
	return el("span", { class: "muted" }, "Not served");
}

function renderDatabases() {
	if (!o) return;
	const q = find.value.trim().toLowerCase();
	const list = o.databases.filter((d) => !q || d.name.toLowerCase().includes(q) || d.folder.toLowerCase().includes(q));
	$("db-count").textContent = `${o.databases.length} ${o.databases.length === 1 ? "database" : "databases"}${q ? `, ${list.length} shown` : ""}`;
	$("db-empty").hidden = o.databases.length > 0;
	if (selected && !row(selected)) selected = null;
	$("db-rows").replaceChildren(
		...list.map((d) => {
			const tr = el(
				"tr",
				{ class: [d.path === selected ? "selected" : "", d.exists ? "" : "missing"].join(" ").trim(), title: d.path, onclick: () => select(d.path) },
				el("td", { class: "strong name" }, d.name),
				el("td", { class: "mono muted" }, tilde(d.folder, home)),
				el("td", { class: "num" }, d.size === null ? "—" : formatBytes(d.size)),
				el("td", { class: "num" }, d.figures ? String(d.figures.collections) : "—"),
				el("td", { class: "muted" }, d.exists ? when(d.openedAt) : "File missing"),
				el("td", {}, serverCell(d)),
				el(
					"td",
					{ class: "row-actions" },
					el("button", { type: "button", class: "small", disabled: !d.exists, onclick: (e: Event) => (e.stopPropagation(), void run(ask.openPath({ path: d.path }), `Opening ${d.name}…`)) }, "Open"),
					d.served
						? el("button", { type: "button", class: "small link", onclick: (e: Event) => (e.stopPropagation(), showServer(d.path)) }, "Server")
						: el("button", { type: "button", class: "small link", disabled: !d.exists, onclick: (e: Event) => (e.stopPropagation(), void openSheet(d.path)) }, "Serve…"),
				),
			);
			tr.dataset["path"] = d.path;
			return tr;
		}),
	);
	renderDetail();
}

function select(path: string) {
	selected = path;
	renderDatabases();
	void loadFigures(path);
}

async function loadFigures(path: string) {
	const f = await ask.figures({ path });
	if (f) live.set(path, f);
	if (selected === path) renderDetail();
	if (selectedServer === path) renderServers();
}

function renderDetail() {
	const d = row(selected);
	$("db-detail").hidden = !d;
	if (!d) return;
	$("db-name").textContent = d.name;
	$("db-path").textContent = tilde(d.path, home);
	const f = live.get(d.path) ?? d.figures;
	$("db-size").textContent = d.size === null ? "—" : formatBytes(d.size);
	$("db-rows-count").textContent = f ? formatCount(f.rows) : "—";
	$("db-collections").textContent = f ? String(f.collections) : "—";
	$("db-dead").textContent = f ? deadShare(f) : "—";
	$("db-seen").textContent = f && !f.live ? `As its server said ${when(f.at).toLowerCase()}: nothing runs on it now.` : f ? "" : "Its figures come from its server: open or serve it to see them.";
	for (const id of ["db-open", "db-backup", "db-compact", "db-reveal"]) $<HTMLButtonElement>(id).disabled = !d.exists;
	$<HTMLButtonElement>("db-forget").disabled = d.window || isServed(d) || d.running !== "none";

	const s = d.served;
	const on = s?.state === "running";
	$("db-dot").className = `dot${on ? " on" : ""}`;
	$("db-serving").textContent = on ? "Serving" : s && s.state !== "stopped" ? (s.state === "starting" ? "Starting…" : "Stopping…") : "Not served";
	$("db-for").textContent = on && s?.startedAt ? `for ${since(s.startedAt)}` : "";
	$("db-url").textContent = on && s?.url ? s.url : s ? (s.settings.port ? `Port ${s.settings.port} when started` : "Any free port when started") : "";
	$("db-facts").textContent = s ? s.facts : "Serve it so your apps can connect: a port, a token kept in your Keychain, and the server runs until you stop it or quit the app.";
	$("db-show-server").hidden = !s;
	$("db-serve").hidden = Boolean(s && s.state !== "stopped");
	$("db-serve").textContent = s ? "Start…" : "Serve…";
	$<HTMLButtonElement>("db-serve").disabled = !d.exists;
	$("db-stop").hidden = !on;
}

$("db-open").addEventListener("click", () => selected && void run(ask.openPath({ path: selected }), "Opening…"));
$("db-backup").addEventListener("click", () => selected && void backup(selected));
$("db-compact").addEventListener("click", async () => {
	if (!selected) return;
	const path = selected;
	await run(ask.compact({ path }), "Compacting…");
	void loadFigures(path);
});
$("db-reveal").addEventListener("click", () => selected && void ask.reveal({ path: selected }));
$("db-forget").addEventListener("click", async () => {
	if (!selected) return;
	o = await ask.forget({ path: selected });
	selected = null;
	render();
});
$("db-show-server").addEventListener("click", () => selected && showServer(selected));
$("db-serve").addEventListener("click", () => selected && void openSheet(selected));
$("db-stop").addEventListener("click", () => selected && void run(ask.stopServer({ path: selected }), "Stopping…"));
$("open").addEventListener("click", () => void run(ask.chooseFile({})));
$("new").addEventListener("click", () => void run(ask.newDatabase({})));

// ------------------------------------------------------------------ servers

let tab: keyof Snippets = "curl";
let fullLog = false;

function showServer(path: string) {
	selectedServer = path;
	show("servers");
	renderServers();
}

function renderServers() {
	if (!o) return;
	const list = o.databases.filter((d) => d.served);
	$("srv-empty").hidden = list.length > 0;
	if (!selectedServer || !list.some((d) => d.path === selectedServer)) selectedServer = list[0]?.path ?? null;
	$("srv-list").replaceChildren(
		...list.map((d) => {
			const s = d.served!;
			const on = s.state === "running";
			return el(
				"li",
				{},
				el(
					"button",
					{ type: "button", class: `pick-item${d.path === selectedServer ? " selected" : ""}`, onclick: () => ((selectedServer = d.path), renderServers(), void tick()) },
					el("span", { class: "strong" }, el("span", { class: `dot${on ? " on" : ""}`, ariaHidden: "true" }), d.name),
					el("span", { class: "mono muted" }, on && s.url ? s.url : s.settings.port ? `Port ${s.settings.port} when started` : "Any free port when started"),
					el("span", { class: "muted" }, stateWords(d)),
				),
			);
		}),
	);
	const d = row(selectedServer);
	$("srv-detail").hidden = !d;
	if (!d || !d.served) return;
	const s = d.served;
	const on = s.state === "running";
	$("srv-name").textContent = d.name;
	$("srv-chip").textContent = on ? "Running" : s.state === "stopped" ? "Stopped" : s.state === "starting" ? "Starting" : "Stopping";
	$("srv-chip").className = `chip${on ? " on" : ""}`;
	$("srv-open").hidden = !on;
	$("srv-restart").hidden = !on;
	$("srv-stop").hidden = !on;
	$("srv-start").hidden = s.state !== "stopped";
	$("srv-remove").hidden = s.state !== "stopped";
	const f = live.get(d.path);
	const fresh = on && f?.live;
	$("srv-rate").textContent = fresh && f.perSecond != null ? (f.perSecond < 10 ? f.perSecond.toFixed(1) : String(Math.round(f.perSecond))) : "—";
	$("srv-median").textContent = fresh && f.median != null ? formatSeconds(f.median) : "—";
	$("srv-conns").textContent = fresh && f.connections != null ? String(f.connections) : "—";
	$("srv-file").textContent = d.size === null ? "—" : formatBytes(d.size);
	$("srv-facts").textContent = on ? s.facts : `${s.lastRan ? `Stopped ${when(s.lastRan.at).toLowerCase()}: ${s.lastRan.why}. ` : ""}Starts with the settings it last ran with.`;
	$("srv-where").textContent = [...s.networkUrls.map((u) => `On your network: ${u}`), ...(s.metricsUrl ? [`Metrics: ${s.metricsUrl}`] : [])].join("\n");
	$("srv-connect").hidden = !s.snippets;
	for (const b of document.querySelectorAll<HTMLButtonElement>(".tab")) b.setAttribute("aria-pressed", String(b.dataset["tab"] === tab));
	$("snippet").textContent = s.snippets ? s.snippets[tab] : "";
	$("log-full").textContent = fullLog ? "Show the last lines" : "Show full log";
}

function stateWords(d: DatabaseRow): string {
	const s = d.served!;
	if (s.state === "running") return s.startedAt ? `Running for ${since(s.startedAt)}` : "Running";
	if (s.state === "starting") return "Starting…";
	if (s.state === "stopping") return "Stopping…";
	return s.lastRan ? `Stopped · last ran ${when(s.lastRan.at).toLowerCase()}` : "Stopped";
}

async function loadLog(path: string) {
	const { lines } = await ask.serverLog({ path });
	if (selectedServer !== path) return;
	const shown = fullLog ? lines : lines.slice(-12);
	const pre = $("srv-log");
	const atEnd = pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 4;
	pre.textContent = shown.join("\n") || (row(path)?.served?.state === "running" ? "" : "Nothing yet: its output shows here while it runs.");
	if (atEnd) pre.scrollTop = pre.scrollHeight;
}

for (const b of document.querySelectorAll<HTMLButtonElement>(".tab"))
	b.addEventListener("click", () => {
		tab = b.dataset["tab"] as keyof Snippets;
		renderServers();
	});
$("snippet-copy").addEventListener("click", () => void run(ask.copyText({ text: $("snippet").textContent ?? "" })));
$("log-full").addEventListener("click", () => {
	fullLog = !fullLog;
	renderServers();
	if (selectedServer) void loadLog(selectedServer);
});
$("srv-open").addEventListener("click", () => selectedServer && void run(ask.openPath({ path: selectedServer }), "Opening…"));
$("srv-settings").addEventListener("click", () => selectedServer && void openSheet(selectedServer));
$("srv-restart").addEventListener("click", () => selectedServer && void run(ask.restartServer({ path: selectedServer }), "Restarting…"));
$("srv-stop").addEventListener("click", () => selectedServer && void run(ask.stopServer({ path: selectedServer }), "Stopping…"));
$("srv-start").addEventListener("click", () => {
	const d = row(selectedServer);
	if (d?.served) void run(ask.serve({ path: d.path, settings: d.served.settings, sync: d.sync }), `Starting ${d.name}…`);
});
$("srv-remove").addEventListener("click", async () => {
	if (!selectedServer) return;
	o = await ask.removeServer({ path: selectedServer });
	selectedServer = null;
	render();
	say("Removed from Servers, and its token from your Keychain.");
});
$("serve-pick").addEventListener("click", () => void openSheet(null));

// ------------------------------------------------------------------ the serve sheet

const sheet = $<HTMLDialogElement>("sheet");
const sheetForm = $<HTMLFormElement>("sheet-form");
let sheetPath: string | null = null;
let tokenShown = false;
const field = <T extends HTMLElement>(name: string) => sheetForm.elements.namedItem(name) as unknown as T;
const radio = (name: string) => (sheetForm.querySelector(`input[name=${name}]:checked`) as HTMLInputElement | null)?.value;

function sheetSettings(): ServeSettings | null {
	const port = parsePort($<HTMLInputElement>("port").value);
	if (!port.ok) return null;
	const box = (n: string) => field<HTMLInputElement>(n).checked;
	return { port: port.port, reach: radio("reach") === "lan" ? "lan" : "mac", studio: box("studio"), compact: box("compact"), mcp: box("mcp"), metrics: box("metrics") };
}
const sheetSync = (): Sync => (radio("sync") as Sync | undefined) ?? "250";

/** The serve sheet on `path`, or on a database to pick when null. Filled with what it was last served with. */
async function openSheet(path: string | null) {
	if (!o) return;
	const choices = o.databases.filter((d) => d.exists && !isServed(d));
	$("sheet-db-box").hidden = path !== null;
	if (path === null) {
		if (!choices.length) {
			show("databases");
			say("Every database here is served already, or there is none: make one with New database, or open one.", true);
			return;
		}
		$<HTMLSelectElement>("sheet-db").replaceChildren(...choices.map((d) => el("option", { value: d.path }, `${d.name} — ${tilde(d.folder, home)}`)));
		path = choices[0]!.path;
	}
	fillSheet(path);
	if (!sheet.open) sheet.showModal();
	$<HTMLInputElement>("port").focus();
}

function fillSheet(path: string) {
	sheetPath = path;
	const d = row(path);
	const s = d?.served?.settings ?? DEFAULT_SETTINGS;
	const running = d?.served?.state === "running";
	$("sheet-title").textContent = `Serve ${d?.name ?? ""}`;
	$<HTMLInputElement>("port").value = s.port ? String(s.port) : "";
	for (const r of sheetForm.querySelectorAll<HTMLInputElement>("input[name=reach]")) r.checked = r.value === s.reach;
	for (const r of sheetForm.querySelectorAll<HTMLInputElement>("input[name=sync]")) r.checked = r.value === (d?.sync ?? "250");
	for (const n of ["studio", "compact", "mcp", "metrics"] as const) field<HTMLInputElement>(n).checked = s[n];
	$("sheet-start").textContent = running ? "Restart server" : "Start server";
	$("sheet-error").textContent = "";
	tokenShown = false;
	$("tok-shown").textContent = "••••••••••••••••••••••••••";
	$("tok-show").textContent = "Show";
	void sheetChanged();
}

let portAsked = 0;
async function sheetChanged() {
	if (!sheetPath) return;
	const lan = radio("reach") === "lan";
	$("lan-warn").hidden = !lan;
	$("sync-note").textContent = SYNC_NOTES[sheetSync()];
	const typed = $<HTMLInputElement>("port").value;
	const port = parsePort(typed);
	const settings = sheetSettings();
	if (!port.ok) {
		$("port-note").textContent = port.message;
		$("port-note").classList.add("warn");
	} else if (port.port === null) {
		$("port-note").textContent = "Empty picks any free port: your apps then need the address again after each start.";
		$("port-note").classList.remove("warn");
	} else {
		const n = ++portAsked;
		const free = await ask.portFree({ port: port.port, reach: lan ? "lan" : "mac", path: sheetPath });
		if (n === portAsked) {
			$("port-note").textContent = free.text;
			$("port-note").classList.toggle("warn", !free.free);
		}
	}
	if (settings) {
		const plan = await ask.plan({ path: sheetPath, settings, sync: sheetSync() });
		$("command").textContent = plan.command;
	}
}
sheetForm.addEventListener("input", () => void sheetChanged());
sheetForm.addEventListener("change", (e) => {
	if ((e.target as HTMLElement).id === "sheet-db") fillSheet(($<HTMLSelectElement>("sheet-db")).value);
	else void sheetChanged();
});
$("sheet-cancel").addEventListener("click", () => sheet.close());
$("tok-show").addEventListener("click", async () => {
	if (!sheetPath) return;
	tokenShown = !tokenShown;
	$("tok-shown").textContent = tokenShown ? (await ask.serveToken({ path: sheetPath })).token : "••••••••••••••••••••••••••";
	$("tok-show").textContent = tokenShown ? "Hide" : "Show";
});
$("tok-copy").addEventListener("click", async () => sheetPath && ($("sheet-error").textContent = (await ask.copyToken({ path: sheetPath })).message ?? ""));
$("tok-new").addEventListener("click", async () => {
	if (!sheetPath) return;
	const r = await ask.newToken({ path: sheetPath });
	$("sheet-error").textContent = r.message ?? "";
	if (tokenShown) $("tok-shown").textContent = (await ask.serveToken({ path: sheetPath })).token;
});
sheetForm.addEventListener("submit", async (e) => {
	e.preventDefault();
	if (!sheetPath) return;
	const settings = sheetSettings();
	if (!settings) return void ($("sheet-error").textContent = "A port is a number from 1 to 65535, or empty for any free port.");
	const path = sheetPath;
	const running = row(path)?.served?.state === "running";
	$<HTMLButtonElement>("sheet-start").disabled = true;
	try {
		const r = running ? await ask.restartServer({ path, settings, sync: sheetSync() }) : await ask.serve({ path, settings, sync: sheetSync() });
		if (!r.ok && !r.shown) return void ($("sheet-error").textContent = r.message ?? "It did not start.");
		sheet.close();
		if (r.ok) {
			say(r.message);
			await refresh();
			showServer(path);
		}
	} finally {
		$<HTMLButtonElement>("sheet-start").disabled = false;
	}
});

// ------------------------------------------------------------------ remote connections

const form = $<HTMLFormElement>("add");
const input = (n: string) => form.elements.namedItem(n) as HTMLInputElement;
let editing: string | null = null;

function renderConnections() {
	if (!o) return;
	$("connections").replaceChildren(
		...o.connections.map((c) => {
			const check = o!.checks[c.id];
			const open = o!.open.includes(`remote:${c.id}`);
			const li = el(
				"li",
				{},
				el(
					"button",
					{ type: "button", class: `pick-item${c.id === editing ? " selected" : ""}`, onclick: () => edit(c) },
					el("span", { class: "strong" }, el("span", { class: `dot${check?.ok ? " on" : check ? " bad" : ""}`, ariaHidden: "true" }), c.name),
					el("span", { class: "mono muted" }, c.url + (c.tenant ? ` · ${c.tenant}` : "")),
					el("span", { class: "muted" }, [open ? "open" : "", c.hasToken ? "token in keychain" : "no token", check ? (check.ok ? "reachable" : "not reachable") : ""].filter(Boolean).join(" · ")),
				),
			);
			// What the earlier list had, kept for whoever drives it: the name opens it.
			li.dataset["id"] = c.id;
			return li;
		}),
	);
	const c = editing ? o.connections.find((x) => x.id === editing) : null;
	if (editing && !c) stopEditing();
	if (c) {
		const check = o.checks[c.id];
		$("last-check-box").hidden = !check;
		$("last-check").textContent = check ? `${check.text} (${when(check.at).toLowerCase()})` : "";
		$("conn-chip").hidden = !check;
		$("conn-chip").textContent = check?.ok ? "Reachable" : "Not reachable";
		$("conn-chip").className = `chip${check?.ok ? " on" : " bad"}`;
		$("token-note").textContent = c.hasToken ? "saved in your Keychain" : "none saved";
		$("forget-token").hidden = !c.hasToken;
	}
}

/** The form, ready to change a saved connection: its token stays unless a new one is typed or the address changes. */
function edit(c: Connection) {
	editing = c.id;
	input("name").value = c.name;
	input("url").value = c.url;
	input("tenant").value = c.tenant ?? "";
	input("token").value = "";
	input("token").placeholder = c.hasToken ? "unchanged: type one to replace it" : "kept in your Keychain";
	$("conn-title").textContent = c.name;
	$("save").textContent = "Save changes";
	for (const id of ["cancel-edit", "conn-remove", "conn-check", "conn-open"]) $(id).hidden = false;
	warnClear();
	renderConnections();
	input("token").focus();
}

function stopEditing() {
	editing = null;
	form.reset();
	input("token").placeholder = "kept in your Keychain";
	$("conn-title").textContent = "A new connection";
	$("token-note").textContent = "kept in your Keychain";
	$("save").textContent = "Save connection";
	for (const id of ["cancel-edit", "conn-remove", "conn-check", "conn-open", "forget-token", "last-check-box", "conn-chip"]) $(id).hidden = true;
	warnClear();
	renderConnections();
}

/** Plain http to another machine sends the token in the clear: say so where it is typed. */
function warnClear() {
	let clear = false;
	try {
		clear = sendsTokenInTheClear(originOf(input("url").value));
	} catch {
		// Not an address yet.
	}
	$("clear").hidden = !clear;
}

$("conn-add").addEventListener("click", () => {
	stopEditing();
	input("name").focus();
});
$("cancel-edit").addEventListener("click", stopEditing);
$("forget-token").addEventListener("click", async () => {
	if (editing === null) return;
	o = await ask.forgetToken({ id: editing });
	render();
	input("token").placeholder = "kept in your Keychain";
	$("forget-token").hidden = true;
	say("The saved token was removed from the keychain. The connection stays.");
});
$("conn-remove").addEventListener("click", async () => {
	if (editing === null) return;
	o = await ask.removeConnection({ id: editing });
	stopEditing();
	render();
	say("Removed, and its token from the keychain.");
});
$("conn-check").addEventListener("click", async () => {
	if (editing === null) return;
	say("Checking…");
	const r = await ask.checkConnection({ id: editing });
	say(r.ok ? "Reachable." : "Not reachable.", !r.ok);
	await refresh();
});
$("conn-open").addEventListener("click", () => {
	const c = o?.connections.find((x) => x.id === editing);
	if (c) void run(ask.openConnection({ id: c.id }), `Connecting to ${c.name}…`);
});
input("url").addEventListener("input", warnClear);

form.addEventListener("submit", async (e) => {
	e.preventDefault();
	const v = (n: string) => input(n).value;
	const r = await ask.saveConnection({ id: editing ?? undefined, name: v("name"), url: v("url"), token: v("token"), tenant: v("tenant") });
	if (!r.ok) return say(r.message, true);
	const changedToken = v("token") !== "";
	const wasEditing = editing !== null;
	stopEditing();
	o = r.overview!;
	render();
	say(
		r.forgotToken
			? "Saved. The address changed, so the token saved for the old one was forgotten: type the new server's token."
			: changedToken
				? "Saved. The token is in the keychain."
				: wasEditing
					? "Saved."
					: "Saved, with no token.",
	);
});

// ------------------------------------------------------------------ backups

let backupFor: string | null = null;

function renderBackups() {
	if (!o) return;
	const list = o.databases.filter((d) => d.exists || d.archive);
	$("bk-empty").hidden = list.length > 0;
	$("bk-rows").replaceChildren(
		...list.map((d) => {
			const a = d.archive;
			return el(
				"tr",
				{},
				el("td", { class: "strong" }, d.name),
				el("td", { class: "mono muted" }, a ? tilde(a.dir, home) : "Not set up"),
				el("td", {}, a ? (a.keyFile ? "With a key file" : "No") : "—"),
				el("td", { class: "muted" }, a ? (a.lastAt ? `${when(a.lastAt)}, change ${a.lastChange}` : "None yet") : "—"),
				el(
					"td",
					{ class: "row-actions" },
					...(a
						? [
								el("button", { type: "button", class: "small", disabled: !d.exists, onclick: () => void backup(d.path) }, "Back up now"),
								el("button", { type: "button", class: "small", onclick: () => void run(ask.verify({ path: d.path }), "Verifying…") }, "Verify"),
								el("button", { type: "button", class: "small", onclick: () => void openRestore(d.path) }, "Restore…"),
								el("button", { type: "button", class: "small quiet", onclick: () => openSetup(d.path) }, "Change…"),
							]
						: [el("button", { type: "button", class: "small", disabled: !d.exists, onclick: () => openSetup(d.path) }, "Set up")]),
				),
			);
		}),
	);
}

async function backup(path: string) {
	const d = row(path);
	if (!d) return;
	if (!d.archive) {
		show("backups");
		openSetup(path);
		return say(`Choose where the backups of ${d.name} go, then Back up now.`);
	}
	await run(ask.backup({ path }), `Backing up ${d.name}…`);
}

function openSetup(path: string) {
	const d = row(path);
	if (!d) return;
	backupFor = path;
	$("bk-restore").hidden = true;
	$("bk-setup").hidden = false;
	$("bk-setup-title").textContent = `Backups of ${d.name}`;
	$<HTMLInputElement>("bk-dir").value = d.archive?.dir ?? "";
	$<HTMLInputElement>("bk-key").value = d.archive?.keyFile ?? "";
	for (const r of document.querySelectorAll<HTMLInputElement>("input[name=seal]")) r.checked = r.value === (d.archive?.keyFile ? "key" : "none");
	sealChanged();
	$("bk-setup-error").textContent = "";
	$("bk-setup").scrollIntoView({ block: "nearest" });
}
function sealChanged() {
	const key = (document.querySelector("input[name=seal]:checked") as HTMLInputElement | null)?.value === "key";
	$("bk-key-box").hidden = !key;
	$("bk-key-note").hidden = !key;
}
for (const r of document.querySelectorAll<HTMLInputElement>("input[name=seal]")) r.addEventListener("change", sealChanged);
$("bk-setup-cancel").addEventListener("click", () => ($("bk-setup").hidden = true));
$("bk-dir-choose").addEventListener("click", async () => {
	const d = row(backupFor);
	const { folder } = await ask.chooseFolder({ from: $<HTMLInputElement>("bk-dir").value });
	// A folder for this database's backups inside the one chosen, unless it was chosen as that.
	if (folder) $<HTMLInputElement>("bk-dir").value = d && !folder.endsWith(`/${stem(d.name)}`) ? `${folder}/${stem(d.name)}` : folder;
});
$("bk-key-choose").addEventListener("click", async () => {
	const { path } = await ask.chooseKey({ from: "" });
	if (path) $<HTMLInputElement>("bk-key").value = path;
});
$("bk-key-make").addEventListener("click", async () => {
	const d = row(backupFor);
	const { folder } = await ask.chooseFolder({ from: "" });
	if (!folder || !d) return;
	const r = await ask.makeKey({ folder, name: `${stem(d.name)}.key` });
	$("bk-setup-error").textContent = r.message ?? "";
	if (r.ok && r.path) $<HTMLInputElement>("bk-key").value = r.path;
});
$<HTMLFormElement>("bk-setup").addEventListener("submit", async (e) => {
	e.preventDefault();
	if (!backupFor) return;
	const sealed = (document.querySelector("input[name=seal]:checked") as HTMLInputElement | null)?.value === "key";
	const keyFile = sealed ? $<HTMLInputElement>("bk-key").value.trim() : "";
	if (sealed && !keyFile) return void ($("bk-setup-error").textContent = "Choose the key file, or make a new one.");
	const r = await ask.setArchive({ path: backupFor, dir: $<HTMLInputElement>("bk-dir").value.trim(), keyFile: keyFile || null });
	if (!r.ok) return void ($("bk-setup-error").textContent = r.message ?? "");
	$("bk-setup").hidden = true;
	say(r.message);
	await refresh();
});

const stem = (name: string) => name.replace(/\.fenec$/i, "");

async function openRestore(path: string) {
	const d = row(path);
	if (!d) return;
	backupFor = path;
	$("bk-setup").hidden = true;
	$("bk-restore").hidden = false;
	$("bk-restore-title").textContent = `Restore ${d.name}`;
	const images: Image[] = await ask.images({ path });
	$<HTMLSelectElement>("bk-image").replaceChildren(
		...(images.length
			? [
					el("option", { value: "" }, `The newest backup (${when(images[0]!.at)}, change ${images[0]!.change})`),
					...images.slice(1).map((i) => el("option", { value: String(i.change) }, `${new Date(i.at).toLocaleString("en-US")}, change ${i.change}`)),
				]
			: [el("option", { value: "" }, "The newest backup")]),
	);
	$<HTMLInputElement>("bk-to-folder").value = d.folder;
	$<HTMLInputElement>("bk-to-name").value = `${stem(d.name)}-restored`;
	$("bk-restore-error").textContent = images.length ? "" : "No backup is in that folder yet.";
	$("bk-restore").scrollIntoView({ block: "nearest" });
}
$("bk-restore-cancel").addEventListener("click", () => ($("bk-restore").hidden = true));
$("bk-to-choose").addEventListener("click", async () => {
	const { folder } = await ask.chooseFolder({ from: $<HTMLInputElement>("bk-to-folder").value });
	if (folder) $<HTMLInputElement>("bk-to-folder").value = folder;
});
$<HTMLFormElement>("bk-restore").addEventListener("submit", async (e) => {
	e.preventDefault();
	const go = $<HTMLButtonElement>("bk-restore-go");
	// One restore at a time from here: a second click would start a second into the same name.
	if (!backupFor || go.disabled) return;
	const v = $<HTMLSelectElement>("bk-image").value;
	say("Restoring…");
	go.disabled = true;
	let r: Awaited<ReturnType<typeof ask.restore>>;
	try {
		r = await ask.restore({ path: backupFor, change: v === "" ? null : Number(v), folder: $<HTMLInputElement>("bk-to-folder").value, name: $<HTMLInputElement>("bk-to-name").value });
	} finally {
		go.disabled = false;
	}
	if (!r.ok) {
		say("");
		if (!r.shown) $("bk-restore-error").textContent = r.message ?? "";
		return;
	}
	$("bk-restore").hidden = true;
	say(r.message);
	await refresh();
});

// ------------------------------------------------------------------ what moves by itself

/** Figures and output of what is in view, every 2 s: the main process reads them from each server's own /_metrics. */
async function tick() {
	if (!o) return;
	if (section === "databases" && selected && row(selected)?.running !== "none") await loadFigures(selected);
	if (section === "servers" && selectedServer) {
		if (row(selectedServer)?.served?.state === "running") await loadFigures(selectedServer);
		await loadLog(selectedServer);
	}
}
setInterval(() => void tick(), 2000);

window.addEventListener("focus", () => void refresh());
await refresh();
