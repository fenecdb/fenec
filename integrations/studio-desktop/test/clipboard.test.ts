import assert from "node:assert/strict";
import { test } from "node:test";
import { copySecret, SECRET_ON_CLIPBOARD_MS, sweepSecrets, type Clipboard } from "../src/bun/clipboard.ts";

// A token copied to the clipboard is cleared a minute later if the clipboard
// still holds it, and left alone if something else was copied since.

function board(): Clipboard & { text: string | null; cleared: number } {
	return {
		text: null,
		cleared: 0,
		write(t) {
			this.text = t;
		},
		read() {
			return this.text;
		},
		clear() {
			this.text = null;
			this.cleared++;
		},
	};
}

function timers() {
	const due: { f: () => void; ms: number }[] = [];
	return { due, schedule: (f: () => void, ms: number) => due.push({ f, ms }), fire: () => due.splice(0).forEach((t) => t.f()) };
}

test("a minute, then the token is gone from the clipboard", () => {
	const b = board();
	const t = timers();
	copySecret("the-token", b, SECRET_ON_CLIPBOARD_MS, t.schedule);
	assert.equal(b.text, "the-token");
	assert.deepEqual(
		t.due.map((d) => d.ms),
		[60_000],
	);
	t.fire();
	assert.equal(b.text, null);
	assert.equal(b.cleared, 1);
});

test("what was copied since is not cleared", () => {
	const b = board();
	const t = timers();
	copySecret("the-token", b, SECRET_ON_CLIPBOARD_MS, t.schedule);
	b.write("a snippet the user copied after");
	t.fire();
	assert.equal(b.text, "a snippet the user copied after");
	assert.equal(b.cleared, 0);
});

test("copied twice: each copy's minute is its own, and the later token is kept until its own", () => {
	const b = board();
	const t = timers();
	copySecret("old-token", b, SECRET_ON_CLIPBOARD_MS, t.schedule);
	const first = t.due.splice(0);
	copySecret("new-token", b, SECRET_ON_CLIPBOARD_MS, t.schedule);
	first.forEach((d) => d.f());
	assert.equal(b.text, "new-token", "the first copy's timer leaves the second token");
	t.fire();
	assert.equal(b.text, null);
});

test("an empty clipboard, or one the app cannot read, is left as it is", () => {
	const b = board();
	const t = timers();
	copySecret("the-token", b, SECRET_ON_CLIPBOARD_MS, t.schedule);
	b.clear();
	t.fire();
	assert.equal(b.cleared, 1, "cleared once, by the user, not again");
});

test("a quit clears a token still waiting for its minute, on the same terms, and once", () => {
	const b = board();
	const t = timers();
	copySecret("the-token", b, SECRET_ON_CLIPBOARD_MS, t.schedule);
	sweepSecrets();
	assert.equal(b.text, null);
	b.write("copied after the quit began");
	t.fire();
	assert.equal(b.text, "copied after the quit began", "the timer that fires after finds nothing of its own");
	const other = board();
	copySecret("another", other, SECRET_ON_CLIPBOARD_MS, t.schedule);
	other.write("the user's own");
	sweepSecrets();
	assert.equal(other.text, "the user's own");
});
