// Constants electrobun.config.ts has the bundler put in (`build.bun.define`).

/** True only in the test variant: the hooks the scripted checks drive are bundled in it, and in no other build. */
declare const __TESTABLE__: boolean;
/** The Keychain service saved servers' tokens go under: the app's identifier. */
declare const __KEYCHAIN_SERVICE__: string;
