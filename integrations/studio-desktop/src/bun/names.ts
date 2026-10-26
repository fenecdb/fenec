// The name typed in the new-database window, made a file name or refused in
// words. Pure, so `node --test` runs it (test/names.test.ts).

export const EXTENSION = ".fenec";

export type Named = { ok: true; fileName: string } | { ok: false; message: string };

// APFS and HFS+ both cap a name at 255 bytes of UTF-8.
const MAX_BYTES = 255;

/**
 * A first collection, from the new-database window: a name and its fields as
 * FenecQL writes them (`title text, done bool @hash`), made into the one
 * statement that creates it, or refused in words. Held to a narrow grammar
 * rather than handed over as typed: the statement goes to the server with the
 * database's own token, and nothing typed here may close the field list and
 * say something else. A field the server knows and this does not is added in
 * the studio afterwards; a keyword used as a name is refused by the server, in
 * its words.
 */
const IDENT = /^[A-Za-z_][A-Za-z0-9_]*$/;
const TYPE = /^(?:bool|int|float|text|bytes|timestamp|json|geo|vector<[1-9]\d{0,5}>|\[(?:bool|int|float|text|bytes|timestamp|json)\])$/;
const INDEX = /^@(?:hash|sorted|text|unique)$/;

export type Statement = { ok: true; statement: string } | { ok: false; message: string };

export function collectionStatement(name: string, fields: string): Statement {
	const n = name.trim();
	if (!IDENT.test(n)) return { ok: false, message: "A collection's name is letters, digits and _, and starts with a letter or _." };
	const parts = fields.split(",").map((f) => f.trim()).filter(Boolean);
	if (parts.length === 0) return { ok: false, message: "A collection has at least one field: title text, for one." };
	const seen = new Set<string>();
	const out: string[] = [];
	for (const part of parts) {
		const [field, type, index, ...rest] = part.split(/\s+/);
		if (!field || !IDENT.test(field)) return { ok: false, message: `"${part}": a field's name is letters, digits and _, and starts with a letter or _.` };
		if (!type || !TYPE.test(type.toLowerCase())) return { ok: false, message: `"${part}": give a type after the name: bool, int, float, text, bytes, timestamp, json, geo, vector<768> or [text].` };
		if ((index && !INDEX.test(index)) || rest.length) return { ok: false, message: `"${part}": after the type only an index: @hash, @sorted, @text or @unique.` };
		if (seen.has(field.toLowerCase())) return { ok: false, message: `${field} is there twice.` };
		if (field.toLowerCase() === "id") return { ok: false, message: "Every collection has its id already: leave it out." };
		seen.add(field.toLowerCase());
		out.push([field, type.toLowerCase(), ...(index ? [index] : [])].join(" "));
	}
	return { ok: true, statement: `create collection ${n} (${out.join(", ")})` };
}

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
