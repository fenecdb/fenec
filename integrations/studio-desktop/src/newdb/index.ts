import { Electroview } from "electrobun/view";
import type { NewDbRPC } from "../shared/rpc.ts";

const rpc = Electroview.defineRPC<NewDbRPC>({
	maxRequestTime: 120_000,
	handlers: { requests: {}, messages: {} },
});
const view = new Electroview({ rpc });
const ask = view.rpc!.request;

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const folder = $<HTMLInputElement>("folder");
const name = $<HTMLInputElement>("name");
const error = $("error");
const create = $<HTMLButtonElement>("create");

function showName() {
	const typed = name.value.trim();
	$("file").textContent = (typed || "orders") + (typed.toLowerCase().endsWith(".fenec") ? "" : ".fenec");
}
name.addEventListener("input", () => {
	error.textContent = "";
	showName();
});

$("choose").addEventListener("click", async () => {
	const { folder: picked } = await ask.chooseFolder({ from: folder.value });
	if (picked) {
		folder.value = picked;
		error.textContent = "";
	}
	name.focus();
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
		// The main process makes the file and opens its window, and closes this one.
		const made = await ask.create({ folder: folder.value, name: name.value });
		if (!made.ok && !made.shown) error.textContent = made.message ?? "Could not create the database.";
	} finally {
		create.disabled = false;
	}
});

folder.value = (await ask.defaults({})).folder;
name.focus();
showName();
