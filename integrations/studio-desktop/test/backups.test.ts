import assert from "node:assert/strict";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, test } from "node:test";
import { imagesIn, listImages, makeKey, restoreArchive, takeBackup, verifyArchive } from "../src/bun/backups.ts";
import { randomToken, startServer, type FileServer } from "../src/bun/process.ts";
import { taskPlan, windowPlan } from "../src/bun/serving.ts";
import { missing, nodeRun, nodeSpawn, SERVER } from "./node-spawn.ts";

// Backups through the real binaries and the app's own code: `fenec backup`
// from a server started as the app starts one, into an archive folder;
// `fenec verify`; and `fenec restore` into a new file, which is never one that
// is there already -- a file a server serves least of all.

const skip = missing() || false;
const work = mkdtempSync(join(tmpdir(), "fenec-desktop-backups-"));
const running: FileServer[] = [];
after(async () => {
	await Promise.all(running.map((s) => s.stop()));
	rmSync(work, { recursive: true, force: true });
});

async function serve(name: string, rows: number): Promise<{ file: string; server: FileServer }> {
	const file = join(work, `${name}.fenec`);
	writeFileSync(file, "");
	const server = await startServer(nodeSpawn, SERVER, windowPlan(file, "250"), randomToken());
	running.push(server);
	const q = async (query: string) => {
		const r = await fetch(`${server.local}/query`, { method: "POST", headers: { authorization: `Bearer ${server.token}` }, body: JSON.stringify({ query }) });
		assert.equal(r.status, 200, await r.clone().text());
	};
	await q("create collection notes (title text)");
	for (let i = 0; i < rows; i++) await q(`put notes {title: "n${i}"}`);
	return { file, server };
}

/** How many notes a database file holds, asked of a server started on it. */
async function count(file: string): Promise<number> {
	const s = await startServer(nodeSpawn, SERVER, taskPlan(file), randomToken());
	try {
		const r = await fetch(`${s.local}/query`, { method: "POST", headers: { authorization: `Bearer ${s.token}` }, body: JSON.stringify({ query: "get notes count" }) });
		const body = (await r.json()) as unknown;
		return Number(JSON.stringify(body).match(/\d+/)?.[0] ?? NaN);
	} finally {
		await s.stop();
	}
}

const noScratch = (dir: string) => assert.deepEqual(readdirSync(dir).filter((n) => n.includes(".restoring-")), [], "no scratch file left behind");

test("a backup of a running server, verified, restored into a new file that holds what it held", { skip }, async () => {
	const { server } = await serve("orders", 3);
	const archive = { dir: join(work, "backups", "orders"), keyFile: null };
	const taken = await takeBackup(nodeRun, { local: server.local, replicationToken: server.replicationToken! }, archive);
	assert.ok(taken.ok, JSON.stringify(taken));
	assert.ok(existsSync(archive.dir), "the folder is made");
	const r = await fetch(`${server.local}/query`, { method: "POST", headers: { authorization: `Bearer ${server.token}` }, body: JSON.stringify({ query: 'put notes {title: "later"}' }) });
	assert.equal(r.status, 200);
	const second = await takeBackup(nodeRun, { local: server.local, replicationToken: server.replicationToken! }, archive);
	assert.ok(second.ok && taken.ok && second.change > taken.change, "a later image, at a later change");
	const images = listImages(archive.dir);
	assert.equal(images.length, 2);
	assert.equal(images[0]!.change, second.ok ? second.change : -1, "newest first");

	const v = await verifyArchive(nodeRun, archive);
	assert.ok(v.ok, v.text);
	assert.match(v.text, /2 images/);

	const latest = join(work, "restored.fenec");
	const back = await restoreArchive(nodeRun, archive, null, latest);
	assert.ok(back.ok, JSON.stringify(back));
	assert.equal(await count(latest), 4);
	const earlier = join(work, "earlier.fenec");
	assert.ok((await restoreArchive(nodeRun, archive, taken.ok ? taken.change : 0, earlier)).ok);
	assert.equal(await count(earlier), 3, "as it stood at the first image");
	noScratch(work);
});

test("a restore never writes over a file: not one that is there, not one a server serves", { skip }, async () => {
	const { file, server } = await serve("live", 2);
	const archive = { dir: join(work, "backups", "live"), keyFile: null };
	assert.ok((await takeBackup(nodeRun, { local: server.local, replicationToken: server.replicationToken! }, archive)).ok);
	const before = readFileSync(file);
	const over = await restoreArchive(nodeRun, archive, null, file);
	assert.equal(over.ok, false);
	assert.match(over.ok ? "" : over.message, /exists in that folder already/);
	assert.deepEqual(readFileSync(file), before, "the served file is as it was");
	const mine = join(work, "notes.txt");
	writeFileSync(mine, "mine");
	assert.equal((await restoreArchive(nodeRun, archive, null, mine)).ok, false);
	assert.equal(readFileSync(mine, "utf8"), "mine");
	noScratch(work);
});

test("sealed with a key file: the key is made once, the backup reads only with it", { skip }, async () => {
	const { server } = await serve("sealed", 1);
	mkdirSync(join(work, "keys"), { recursive: true });
	const keyFile = join(work, "keys", "sealed.key");
	const made = await makeKey(nodeRun, keyFile);
	assert.ok(made.ok, made.message);
	assert.equal(statSync(keyFile).mode & 0o777, 0o600, "readable by its owner alone");
	assert.equal((await makeKey(nodeRun, keyFile)).ok, false, "a key is never written over");
	const archive = { dir: join(work, "backups", "sealed"), keyFile };
	assert.ok((await takeBackup(nodeRun, { local: server.local, replicationToken: server.replicationToken! }, archive)).ok);
	const image = readFileSync(join(archive.dir, listImages(archive.dir)[0]!.file));
	assert.ok(!image.subarray(0, 7).equals(Buffer.from("FENECDB")), "the image is not a database in the clear");
	assert.equal((await verifyArchive(nodeRun, { ...archive, keyFile: null })).ok, false, "without the key it does not read");
	assert.ok((await verifyArchive(nodeRun, archive)).ok);
	const out = join(work, "unsealed.fenec");
	assert.equal((await restoreArchive(nodeRun, { ...archive, keyFile: null }, null, out)).ok, false);
	assert.ok(!existsSync(out), "a failed restore leaves nothing at the name");
	assert.ok((await restoreArchive(nodeRun, archive, null, out)).ok);
	assert.equal(await count(out), 1);
	noScratch(work);
});

test("a wrong replication token is a refusal in words", { skip }, async () => {
	const { server } = await serve("refused", 0);
	const r = await takeBackup(nodeRun, { local: server.local, replicationToken: "not-it" }, { dir: join(work, "backups", "refused"), keyFile: null });
	assert.equal(r.ok, false);
	assert.match(r.ok ? "" : r.message, /401|replication token/);
});

test("an archive folder's images, newest first; what is not one is passed over", () => {
	const names = ["image-00000000000000000003-1792968462639.fenec", "segment-1.fenec", "image-00000000000000000007-1792968500000.fenec", ".DS_Store", "image-7-1.fenec"];
	assert.deepEqual(imagesIn(names), [
		{ change: 7, at: 1792968500000, file: "image-00000000000000000007-1792968500000.fenec" },
		{ change: 3, at: 1792968462639, file: "image-00000000000000000003-1792968462639.fenec" },
	]);
	assert.deepEqual(listImages(join(work, "no-such-folder")), []);
});
