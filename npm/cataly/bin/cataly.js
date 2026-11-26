#!/usr/bin/env node
// The `cataly` command of an npm or Bun install. It runs the Cataly binary from the package for
// this platform (an optional dependency, so only that one is installed) with the same arguments
// and standard streams, passes signals on, and ends the way the binary ended.
"use strict";

const { spawn } = require("node:child_process");
const os = require("node:os");
const path = require("node:path");

const TARGETS = {
	"linux x64": "linux-x64",
	"linux arm64": "linux-arm64",
	"darwin arm64": "darwin-arm64",
	"darwin x64": "darwin-x64",
};
const SIGNALS = ["SIGINT", "SIGTERM", "SIGHUP", "SIGQUIT", "SIGUSR2"];

function fail(message) {
	process.stderr.write(`cataly: ${message}\n`);
	process.exit(1);
}

const host = `${process.platform} ${process.arch}`;
const target = TARGETS[host];
if (!target) {
	fail(`this host is ${host}; Cataly runs on Linux and macOS, on x64 and arm64`);
}

const name = `cataly-${target}`;
let binary;
try {
	binary = path.join(path.dirname(require.resolve(`${name}/package.json`)), "cataly");
} catch {
	fail(
		`the package ${name} is not installed next to this one; install again with ` +
			"`npm install --global cataly` or `bun add --global cataly`, with optional dependencies enabled",
	);
}

const child = spawn(binary, process.argv.slice(2), { stdio: "inherit" });
const forward = (signal) => {
	child.kill(signal);
};
for (const signal of SIGNALS) {
	process.on(signal, forward);
}
child.on("error", (error) => {
	fail(`${binary} did not run: ${error.message}`);
});
child.on("exit", (code, signal) => {
	for (const s of SIGNALS) {
		process.removeListener(s, forward);
	}
	if (signal) {
		// Ends this process by the same signal; one Node ignores (SIGPIPE) ends with the shell's code.
		process.kill(process.pid, signal);
		process.exit(128 + (os.constants.signals[signal] ?? 0));
	}
	process.exit(code ?? 1);
});
