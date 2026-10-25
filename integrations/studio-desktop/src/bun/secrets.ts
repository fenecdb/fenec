// Connection tokens live in the OS credential store, never in state.json.
//
// Electrobun 2.0.4 has no keychain API of its own. The main process is Bun,
// and Bun.secrets (Bun >= 1.2.21) is one: macOS Keychain (Security.framework),
// libsecret on Linux, Credential Manager on Windows. Where it is missing the
// `security` CLI is the macOS fallback; its one drawback is that the secret
// is on the command line of `security add-generic-password -w` for a moment,
// which is why Bun.secrets is tried first.

const SERVICE = "dev.fenecdb.studio";

type Store = {
	get(name: string): Promise<string | null>;
	set(name: string, value: string): Promise<void>;
	delete(name: string): Promise<void>;
	kind: string;
};

const bunSecrets: Store | null =
	typeof (Bun as any).secrets?.get === "function"
		? {
				kind: "Bun.secrets",
				get: async (name) => (await (Bun as any).secrets.get({ service: SERVICE, name })) ?? null,
				set: (name, value) => (Bun as any).secrets.set({ service: SERVICE, name, value }),
				delete: async (name) => void (await (Bun as any).secrets.delete({ service: SERVICE, name })),
			}
		: null;

const securityCli: Store = {
	kind: "security CLI",
	async get(name) {
		const p = Bun.spawn(["security", "find-generic-password", "-s", SERVICE, "-a", name, "-w"], { stdout: "pipe", stderr: "ignore" });
		const out = (await new Response(p.stdout).text()).replace(/\n$/, "");
		return (await p.exited) === 0 ? out : null;
	},
	async set(name, value) {
		// -U updates an existing item.
		const p = Bun.spawn(["security", "add-generic-password", "-U", "-s", SERVICE, "-a", name, "-w", value], { stdout: "ignore", stderr: "ignore" });
		if ((await p.exited) !== 0) throw new Error("the keychain refused the token");
	},
	async delete(name) {
		await Bun.spawn(["security", "delete-generic-password", "-s", SERVICE, "-a", name], { stdout: "ignore", stderr: "ignore" }).exited;
	},
};

export const secrets: Store = bunSecrets ?? (process.platform === "darwin" ? securityCli : (null as never));
