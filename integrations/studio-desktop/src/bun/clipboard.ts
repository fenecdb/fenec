// A token copied to the clipboard does not stay there: a minute later, if the
// clipboard still holds it, it is cleared. If something else was copied since,
// that is left alone. A quit does not wait the minute: it clears it then, on
// the same terms (`sweepSecrets`, index.ts). What is kept meanwhile is a digest
// of the token, not the token. Pure but for the clipboard and the timer it is
// handed (pages.ts hands it Electrobun's; test/clipboard.test.ts a stand-in).

import { createHash } from "node:crypto";

export type Clipboard = {
	write(text: string): void;
	read(): string | null;
	clear(): void;
};

/** How long a copied token stays on the clipboard. */
export const SECRET_ON_CLIPBOARD_MS = 60_000;

const digest = (text: string) => createHash("sha256").update(text).digest("hex");

/** The clears still waiting for their minute. */
const waiting = new Set<() => void>();

/** Puts `secret` on the clipboard, and clears it after `afterMs` if it is still there. */
export function copySecret(secret: string, clipboard: Clipboard, afterMs = SECRET_ON_CLIPBOARD_MS, schedule: (f: () => void, ms: number) => unknown = setTimeout): void {
	clipboard.write(secret);
	const mark = digest(secret);
	const sweep = () => {
		waiting.delete(sweep);
		const now = clipboard.read();
		if (now !== null && digest(now) === mark) clipboard.clear();
	};
	waiting.add(sweep);
	schedule(sweep, afterMs);
}

/** What a quit does: each token still waiting for its minute is cleared now, if the clipboard still holds it. */
export function sweepSecrets(): void {
	for (const sweep of [...waiting]) sweep();
}
