import assert from "node:assert/strict";
import { test } from "node:test";
import { childEnvironment, serverEnvironment } from "../src/bun/env.ts";

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

test("the replication token a backup asks with is the app's, and none is none", () => {
	const env = { PATH: "/bin", FENEC_REPLICATION_TOKEN: "an old one", FENEC_KEY_FILE: "/tmp/leak.key" };
	assert.deepEqual(serverEnvironment(env, "t", "r"), { PATH: "/bin", FENEC_HTTP_TOKEN: "t", FENEC_REPLICATION_TOKEN: "r" });
	// Under --sync off the server refuses one, so it is given none, not the shell's.
	assert.deepEqual(serverEnvironment(env, "t", null), { PATH: "/bin", FENEC_HTTP_TOKEN: "t" });
	// The shell is given the allowlist and what is named, and never a key file of the app's environment.
	assert.deepEqual(childEnvironment(env, { FENEC_REPLICATION_TOKEN: "r" }), { PATH: "/bin", FENEC_REPLICATION_TOKEN: "r" });
	assert.deepEqual(childEnvironment(env, {}), { PATH: "/bin" });
	assert.deepEqual(childEnvironment(env, { FENEC_REPLICATION_TOKEN: "" }), { PATH: "/bin" }, "an empty secret is none, as the shell reads it");
});
