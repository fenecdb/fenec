// The name typed in the new-database window, made a file name or refused in
// words. Pure, so `node --test` runs it (test/names.test.ts).

export const EXTENSION = ".fenec";

export type Named = { ok: true; fileName: string } | { ok: false; message: string };

// APFS and HFS+ both cap a name at 255 bytes of UTF-8.
const MAX_BYTES = 255;

export function databaseFileName(typed: string): Named {
	const name = typed.trim();
	if (name === "") return { ok: false, message: "Type a name for the database." };
	// A path separator would name a folder, and ":" is one in Finder's eyes.
	if (/[/\\:\u0000-\u001f]/.test(name)) return { ok: false, message: 'A name cannot hold "/", "\\" or ":". Pick the folder with Choose…' };
	if (name.startsWith(".")) return { ok: false, message: "A name that starts with a dot is hidden in Finder. Start it with a letter or a digit." };
	const fileName = name.toLowerCase().endsWith(EXTENSION) ? name : name + EXTENSION;
	if (new TextEncoder().encode(fileName).length > MAX_BYTES) return { ok: false, message: "That name is too long for a file." };
	return { ok: true, fileName };
}
