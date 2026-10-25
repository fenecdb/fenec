import assert from "node:assert/strict";
import { test } from "node:test";
import { serverEnvironment } from "../src/bun/env.ts";

test("the server gets the few variables a process needs and the token, and nothing else", () => {
	const given = serverEnvironment(
		{
			HOME: "/Users/me",
			PATH: "/usr/bin",
			TMPDIR: "/var/folders/x/",
			LANG: "en_US.UTF-8",
			LC_ALL: "en_US.UTF-8",
			LC_CTYPE: "UTF-8",
			USER: "me",
			LOGNAME: "me",
			TZ: "Europe/Istanbul",
			// What a shell that launched the app may have exported, and what the server reads.
			OTEL_EXPORTER_OTLP_ENDPOINT: "http://collector:4318",
			OTEL_SERVICE_NAME: "x",
			FENEC_JWT_SECRET: "leak",
			FENEC_HTTP_TOKEN: "an old one",
			AWS_SECRET_ACCESS_KEY: "leak",
			ELECTROBUN_APPLEAPIKEYPATH: "/tmp/key.p8",
			GITHUB_TOKEN: "leak",
			DYLD_INSERT_LIBRARIES: "/tmp/evil.dylib",
		},
		"the token",
	);
	assert.deepEqual(Object.keys(given).sort(), ["FENEC_HTTP_TOKEN", "HOME", "LANG", "LC_ALL", "LC_CTYPE", "LOGNAME", "PATH", "TMPDIR", "TZ", "USER"]);
	assert.equal(given["FENEC_HTTP_TOKEN"], "the token", "the app's token, not one that was in the environment");
});

test("a variable that is not set is not passed as the text 'undefined'", () => {
	assert.deepEqual(serverEnvironment({ HOME: undefined, PATH: "/bin" }, "t"), { PATH: "/bin", FENEC_HTTP_TOKEN: "t" });
});
