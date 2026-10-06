import ts from 'typescript';
import { resolve } from 'node:path';

// Crypto publishes TS source with upstream WebStream/DOM overload mismatches.
// Check our project strictly without making upstream source errors our build gate.
const root = resolve(import.meta.dir, '..');
process.chdir(root);
const config = ts.readConfigFile(resolve(root, 'tsconfig.json'), ts.sys.readFile);
const parsed = ts.parseJsonConfigFileContent(config.config, ts.sys, root);
const program = ts.createProgram(parsed.fileNames, parsed.options);
const diagnostics = [...parsed.errors, ...ts.getPreEmitDiagnostics(program)];
const own = diagnostics.filter(d => !d.file?.fileName.includes('/node_modules/'));
if (own.length) {
  process.stderr.write(ts.formatDiagnosticsWithColorAndContext(own, {
    getCanonicalFileName: f => f, getCurrentDirectory: () => root, getNewLine: () => '\n',
  }));
  process.exitCode = 1;
} else {
  process.stdout.write(`Project typecheck passed (${diagnostics.length} upstream source diagnostics excluded).\n`);
}
