// Whether a module is the program being run, as `node file.mjs` runs it. The
// path Node was given can reach the file through a symlink (a temporary
// directory on macOS is under /var, which is /private/var), where the module's
// own URL is the real one, so the two are compared as real paths.
import { realpathSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

export const isMain = (url) => {
  try {
    return process.argv[1] !== undefined && realpathSync(process.argv[1]) === realpathSync(fileURLToPath(url));
  } catch {
    return false;
  }
};
