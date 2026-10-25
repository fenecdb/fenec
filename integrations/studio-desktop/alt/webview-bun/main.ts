// webview-bun + bun build --compile: one process, one window, one file.
//   ./studio-wvb <file.fenec>
import { Webview, SizeHint } from "webview-bun";

const file = process.argv[2];
if (!file) throw new Error("usage: studio-wvb <file.fenec>");
const bin = process.env.FENEC_SERVER_BIN ?? "../../bin/fenec-server"; // see ../../scripts/get-server.sh

// What this shows, measured (macOS arm64, Bun 1.4.4, webview-bun 2.4.0):
//  - `bun build --compile --minify` gives a 89.8 MB binary (the Bun runtime), a
//    libwebview.dylib of 186 KB beside it; studio visible 0.87 s after start.
//  - Webview.run() blocks the JavaScript thread in the native run loop: with a
//    setInterval and a SIGTERM handler installed, 0 ticks in 5 s, and a SIGTERM
//    is not even acted on (the process needs SIGKILL, which orphans the server,
//    lock held). Nothing can supervise the server from JavaScript while the
//    window is open; a separate supervisor process would have to.
//  - No menus, no file dialogs, no multiple windows in one process: one
//    process per window, a file chooser from osascript / zenity / PowerShell.
const token = Buffer.from(crypto.getRandomValues(new Uint8Array(32))).toString("base64url");
const t0 = Date.now();

const proc = Bun.spawn([bin, "--file", file, "--studio", "--http", "127.0.0.1:0"], {
	env: { ...process.env, FENEC_HTTP_TOKEN: token },
	stdin: "ignore",
	stdout: "ignore",
	stderr: "pipe",
});
let origin = "";
let tail = "";
const reader = (proc.stderr as ReadableStream<Uint8Array>).getReader();
const dec = new TextDecoder();
while (!origin) {
	const { value, done } = await reader.read();
	if (done) break;
	tail += dec.decode(value);
	origin = /listening on: (http:\/\/\S+)/.exec(tail)?.[1] ?? "";
}
if (!origin) {
	console.error(tail);
	process.exit(1);
}
// keep draining stderr
(async () => { for (;;) { const r = await reader.read(); if (r.done) break; } })();
console.error(`server up after ${Date.now() - t0} ms at ${origin}`);

const w = new Webview(false, { width: 1280, height: 820, hint: SizeHint.NONE });
w.title = `${file} — fenec studio (webview-bun)`;
// Runs before the page's own scripts on every navigation: the token goes where the studio looks.
const value = JSON.stringify({ server: origin, token, tenant: null });
w.init(`(function(){try{if(location.origin!==${JSON.stringify(origin)})return;sessionStorage.setItem("fenec-studio",${JSON.stringify(value)});}catch(e){}})();`);
if (process.env.WVB_REPORT) {
	w.bind("report", (s: string) => { console.error(`[page ${Date.now() - t0} ms] ${s}`); });
	w.init(`(function(){var n=0,t=setInterval(function(){n++;var x=document.body?document.body.innerText:"";if(/Rows/.test(x)&&/Schema/.test(x)&&/orders \\d/.test(x.replace(/\\n/g," "))){clearInterval(t);window.report("studio visible");}if(n>600)clearInterval(t)},50)})();`);
}
w.navigate(`${origin}/_studio/`);
w.run(); // blocks until the window is closed

proc.kill("SIGTERM");
await proc.exited;
console.error(`closed; server exit ${proc.exitCode}`);
