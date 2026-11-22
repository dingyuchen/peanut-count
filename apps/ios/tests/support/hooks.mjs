// Loads the app's storage code under Node: swaps native Expo modules for test doubles
// and resolves the extensionless imports that Metro allows.
import fs from 'node:fs';
import { registerHooks } from 'node:module';
import { fileURLToPath, pathToFileURL } from 'node:url';

const doubles = {
  'expo-sqlite': 'expo-sqlite.mjs',
  'expo-secure-store': 'expo-secure-store.mjs',
  'expo-crypto': 'expo-crypto.mjs',
  '../drizzle/migrations': 'migrations.mjs',
};

registerHooks({
  resolve(specifier, context, next) {
    if (doubles[specifier]) return { url: new URL(doubles[specifier], import.meta.url).href, shortCircuit: true };
    if (specifier.startsWith('.') && context.parentURL?.endsWith('.ts') && !/\.[cm]?[jt]sx?$/.test(specifier)) {
      const file = fileURLToPath(new URL(`${specifier}.ts`, context.parentURL));
      if (fs.existsSync(file)) return { url: pathToFileURL(file).href, shortCircuit: true };
    }
    return next(specifier, context);
  },
});
