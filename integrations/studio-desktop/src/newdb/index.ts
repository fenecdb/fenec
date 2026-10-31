import { Electroview } from "electrobun/view";
import type { NewDbRPC, Sync } from "../shared/rpc.ts";

const rpc = Electroview.defineRPC<NewDbRPC>({
	maxRequestTime: 35 * 60_000,
	handlers: { requests: {}, messages: {} },
});
const view = new Electroview({ rpc });
const ask = view.rpc!.request;

const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;
const folder = $<HTMLInputElement>("folder");
const name = $<HTMLInputElement>("name");
const error = $("error");
const create = $<HTMLButtonElement>("create");
const checked = (n: string) => (document.querySelector(`input[name=${n}]:checked`) as HTMLInputElement | null)?.value;

const fileName = () => {
	const typed = name.value.trim();
	return (typed || "orders") + (typed.toLowerCase().endsWith(".fenec") ? "" : ".fenec");
};
function showName() {
	$("file").textContent = fileName();
	create.textContent = `Create ${fileName()}`;
}
name.addEventListener("input", () => {
	error.textContent = "";
	showName();
});

let notes: Record<Sync, string> = { always: "", "250": "", off: "" };
function showChoices() {
	const fromArchive = checked("start") === "archive";
	$("empty-box").hidden = fromArchive;
	$("archive-box").hidden = !fromArchive;
	$("sync-note").textContent = notes[(checked("sync") as Sync) ?? "250"];
}
$<HTMLFormElement>("form").addEventListener("change", showChoices);

$("choose").addEventListener("click", async () => {
	const { folder: picked } = await ask.chooseFolder({ from: folder.value });
	if (picked) {
		folder.value = picked;
		error.textContent = "";
	}
	name.focus();
});
$("archive-choose").addEventListener("click", async () => {
	const { folder: picked } = await ask.chooseFolder({ from: $<HTMLInputElement>("archive").value || folder.value });
	if (picked) $<HTMLInputElement>("archive").value = picked;
});
$("key-choose").addEventListener("click", async () => {
	const { path } = await ask.chooseKey({ from: "" });
	if (path) $<HTMLInputElement>("key").value = path;
});

// A window a script did not open cannot close itself, so the main process does.
const cancel = () => void ask.close({});
$("cancel").addEventListener("click", cancel);
window.addEventListener("keydown", (e: KeyboardEvent) => {
	if (e.key === "Escape") cancel();
});

$<HTMLFormElement>("form").addEventListener("submit", async (e) => {
	e.preventDefault();
	error.textContent = "";
	create.disabled = true;
	try {
		const start = checked("start") === "archive" ? "archive" : "empty";
		const key = $<HTMLInputElement>("key").value.trim();
		// The main process makes the file, opens its window or the serve sheet, and closes this one.
		const made = await ask.create({
			folder: folder.value,
			name: name.value,
			sync: (checked("sync") as Sync) ?? "250",
			start,
			...(start === "archive" ? { archive: { dir: $<HTMLInputElement>("archive").value.trim(), keyFile: key || null } } : {}),
			...(start === "empty" && $<HTMLInputElement>("coll").value.trim() ? { collection: { name: $<HTMLInputElement>("coll").value, fields: $<HTMLInputElement>("fields").value } } : {}),
			serve: $<HTMLInputElement>("serve").checked,
		});
		if (!made.ok && !made.shown) error.textContent = made.message ?? "Could not create the database.";
	} finally {
		create.disabled = false;
	}
});

const d = await ask.defaults({});
folder.value = d.folder;
notes = d.syncNotes;
showChoices();
name.focus();
showName();
