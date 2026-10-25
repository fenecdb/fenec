// @hutch cli=0.28.1 cottontail=0.7.2
// The build tool's pins: Hutch 0.28.1 and the Electrobun it builds with, the
// release `electrobun` in package.json is paired with. All three are fetched
// into .hutch-home/ by scripts/electrobun.mjs; nothing is installed globally.
export default {
	electrobun: {
		version: "2.0.4",
	},
};
