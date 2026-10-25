// Connection tokens live in the OS credential store, never in state.json.
//
// Electrobun 2.0.4 has no keychain API of its own. The main process is Bun,
// and Bun.secrets (Bun >= 1.2.21; the app bundles 1.4) is one: macOS Keychain
// (Security.framework), libsecret on Linux, Credential Manager on Windows.
// There is no fallback to the `security` command-line tool on purpose: its
// `add-generic-password -w <secret>` puts the secret in argv, where `ps`
// shows it to every user, which is the one place this app never puts a token.

// The app's identifier (the test variant has its own), so the checks never read
// or write a real install's items.
const SERVICE = __KEYCHAIN_SERVICE__;

type Secrets = {
	get(o: { service: string; name: string }): Promise<string | null>;
	set(o: { service: string; name: string; value: string }): Promise<void>;
	delete(o: { service: string; name: string }): Promise<boolean>;
};

const store = (): Secrets => {
	const s = (Bun as unknown as { secrets?: Secrets }).secrets;
	if (typeof s?.get !== "function") throw new Error("this runtime has no credential store (Bun.secrets)");
	return s;
};

export const secrets = {
	kind: process.platform === "darwin" ? "the macOS Keychain" : "the system credential store",
	async get(name: string): Promise<string | null> {
		return (await store().get({ service: SERVICE, name })) ?? null;
	},
	async set(name: string, value: string): Promise<void> {
		await store().set({ service: SERVICE, name, value });
	},
	async delete(name: string): Promise<void> {
		await store().delete({ service: SERVICE, name });
	},
};
