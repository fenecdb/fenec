// What the examples on serverless.html take from the Worker around them:
// a `.wasm` import is the module compiled, as wrangler declares it.

declare module '*.wasm' {
  const module: WebAssembly.Module;
  export default module;
}
