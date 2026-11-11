import { expect, test } from "bun:test";
import { writeFileSync } from "node:fs";
import { join } from "node:path";
import { withState } from "./fakes.ts";

test("Discord CLI reconnects without duplicate READY or heartbeat leaks and bounds its retry delay", async () => {
  const state = withState();
  try {
    const config = join(state.dir, "config.json");
    writeFileSync(config, JSON.stringify({ stateDir: state.dir, runtime: "herdr", hooks: {} }));
    const child = Bun.spawn(
      [
        process.execPath,
        "--preload",
        join(import.meta.dir, "record-gateway.ts"),
        join(import.meta.dir, "../src/cli.ts"),
        "presence",
        "discord",
      ],
      {
        env: {
          HOME: state.dir,
          PATH: "/usr/bin:/bin",
          DORI_CONFIG: config,
          DORI_STATE_DIR: state.dir,
          DORI_DISCORD_TOKEN: "synthetic-gateway-token",
        },
        stdout: "pipe",
        stderr: "pipe",
        timeout: 10_000,
      },
    );
    const [out, err, code] = await Promise.all([
      new Response(child.stdout).text(),
      new Response(child.stderr).text(),
      child.exited,
    ]);
    expect({ code, err }).toEqual({ code: 0, err: "" });
    expect(out.split("\n").filter((line) => line === "PRESENCE_READY discord")).toHaveLength(1);
    expect(out.split("\n").filter((line) => line.startsWith("PRESENCE_RECONNECT discord"))).toHaveLength(8);
    const receipt = out.split("\n").find((line) => line.startsWith("GATEWAY_QA "));
    expect(JSON.parse(receipt?.slice("GATEWAY_QA ".length) ?? "null")).toEqual({
      opened: 8,
      cleared: 8,
      identified: 8,
      delays: [10_000, 20_000, 40_000, 80_000, 160_000, 300_000, 300_000, 5_000],
      active: 0,
    });
  } finally {
    state.done();
  }
});
