import assert from "node:assert/strict";
import { chmodSync, mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, test } from "node:test";
// @ts-expect-error: plain JavaScript, with no declarations
import { treeDigest } from "../scripts/tree-digest.mjs";

const dir = mkdtempSync(join(tmpdir(), "fenec-tree-"));
after(() => rmSync(dir, { recursive: true, force: true }));

function tree(name: string, files: Record<string, string>) {
	const root = join(dir, name);
	for (const [path, text] of Object.entries(files)) {
		mkdirSync(join(root, path, ".."), { recursive: true });
		writeFileSync(join(root, path), text);
	}
	return root;
}
const digest = (root: string): { files: number; tree: string } => treeDigest(root);

test("the same files are the same digest, wherever the directory is and in whatever order they were made", () => {
	const a = digest(tree("a", { "x/one": "1", "two": "2", ".hidden": "h" }));
	const b = digest(tree("b", { ".hidden": "h", "two": "2", "x/one": "1" }));
	assert.deepEqual(a, b);
	assert.equal(a.files, 3);
});

test("a changed byte, a renamed file, a file added or taken away, a hidden one, each is another digest", () => {
	const base = digest(tree("base", { "bin/tool": "code", "lib": "data" }));
	assert.notEqual(digest(tree("changed", { "bin/tool": "codf", "lib": "data" })).tree, base.tree);
	assert.notEqual(digest(tree("renamed", { "bin/tool2": "code", "lib": "data" })).tree, base.tree);
	assert.notEqual(digest(tree("added", { "bin/tool": "code", "lib": "data", "extra": "" })).tree, base.tree);
	assert.notEqual(digest(tree("removed", { "bin/tool": "code" })).tree, base.tree);
	assert.notEqual(digest(tree("hidden", { "bin/tool": "code", "lib": "data", ".dylib": "x" })).tree, base.tree);
});

test("whether a file is executable is part of it", () => {
	const plain = digest(tree("plain", { tool: "code" }));
	const root = tree("exec", { tool: "code" });
	chmodSync(join(root, "tool"), 0o755);
	assert.notEqual(digest(root).tree, plain.tree);
});

test("a symlink counts as itself, not as what it points to", () => {
	const root = tree("link", { target: "t" });
	symlinkSync("/etc/hosts", join(root, "l"));
	const withLink = digest(root);
	assert.equal(withLink.files, 2);
	const other = tree("link2", { target: "t" });
	symlinkSync("/etc/passwd", join(other, "l"));
	assert.notEqual(digest(other).tree, withLink.tree);
});
