import { expect, test } from "bun:test";
import { existsSync, mkdirSync, readFileSync, readlinkSync, readdirSync, symlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { git, script, setup } from "./install-setup.ts";

const OTHER_CONFIG = '{"owner":"another Dori"}\n';

// Another Dori's installation: its home, its dori-mode skill link and its global dori bin.
function withOtherDori(home: string) {
  mkdirSync(join(home, ".dori"));
  writeFileSync(join(home, ".dori/config.json"), OTHER_CONFIG);
  mkdirSync(join(home, "skills"));
  symlinkSync("/elsewhere/dori-mode", join(home, "skills/dori-mode"));
  mkdirSync(join(home, "global-bin"));
  symlinkSync("/elsewhere/cli.ts", join(home, "global-bin/dori"));
}

const expectOtherDoriUntouched = (home: string) => {
  expect(readFileSync(join(home, ".dori/config.json"), "utf8")).toBe(OTHER_CONFIG);
  expect(readdirSync(join(home, ".dori"))).toEqual(["config.json"]);
  expect(readlinkSync(join(home, "skills/dori-mode"))).toBe("/elsewhere/dori-mode");
  expect(readlinkSync(join(home, "global-bin/dori"))).toBe("/elsewhere/cli.ts");
};

test("another Dori in ~/.dori stops an install with no terminal and no mode, and nothing changes", () => {
  // Given another Dori's ~/.dori.
  const { root, home, install } = setup();
  withOtherDori(home);
  // When install.sh runs without a terminal or DORI_INSTALL_MODE.
  const r = install();
  // Then it stops, names both modes, and leaves every file of the other Dori and every OMP location as it was.
  expect(r.code).toBe(1);
  expect(r.err).toContain("DORI_INSTALL_MODE=side");
  expect(r.err).toContain("DORI_INSTALL_MODE=primary");
  expectOtherDoriUntouched(home);
  expect(existsSync(join(home, "extensions/omp-dori"))).toBe(false);
  expect(existsSync(join(home, ".omp-dori"))).toBe(false);
  expect(existsSync(join(root, "bun.log"))).toBe(false);
});

test("a side install lives in ~/.omp-dori and leaves ~/.dori, the dori-mode link and the dori command alone", () => {
  // Given another Dori's ~/.dori.
  const { root, home, src, ulw, install } = setup();
  withOtherDori(home);
  // When install.sh runs with DORI_INSTALL_MODE=side.
  const r = install({ DORI_INSTALL_MODE: "side" });
  // Then the other Dori is unchanged, no global extension or dori bin was linked, and only omp-dori was added.
  expect(r.code).toBe(0);
  expectOtherDoriUntouched(home);
  const side = join(home, ".omp-dori");
  expect(existsSync(join(home, "extensions/omp-dori"))).toBe(false);
  expect(readFileSync(join(root, "bun.log"), "utf8")).not.toContain("link");
  expect(readlinkSync(join(home, "global-bin/omp-dori"))).toBe(join(side, "bin/omp-dori"));
  expect(readFileSync(join(side, ".omp-dori"), "utf8")).toBe("side\n");
  // And its configuration keeps state and lanes inside the side home.
  const config = JSON.parse(readFileSync(join(side, "config.json"), "utf8"));
  expect(config.stateDir).toBe(join(side, "state"));
  expect(config.agentCommand).toEqual([join(side, "bin/omp"), "--dori-role", "lane", "--model", "{model}", "{prompt}"]);
  expect(config.ompUlwPath).toBe(join(ulw, "index.ts"));
  // And its skill is a renamed copy whose description differs from dori-mode's, so omp lists both apart.
  const skill = readFileSync(join(side, "skills/omp-dori-mode/SKILL.md"), "utf8");
  expect(skill).toContain("\nname: omp-dori-mode\n");
  expect(skill).not.toContain("\ndescription: Turns a coding agent");
  expect(readlinkSync(join(side, "skills/omp-dori-mode/references"))).toBe(join(src, "skills/dori-mode/references"));
});

test("omp-dori runs the real omp with only the omp-dori extension, its overlay, its config and its hc session", () => {
  // Given a side install next to another Dori.
  const { home, src, env, install } = setup();
  withOtherDori(home);
  expect(install({ DORI_INSTALL_MODE: "side" }).code).toBe(0);
  const side = join(home, ".omp-dori");
  // When the installed command starts omp.
  const r = Bun.spawnSync([join(home, "global-bin/omp-dori"), "omp", "--dori-role", "dori"], { env: env() });
  // Then the stub omp (not the wrapper again) gets no discovered extensions, the omp-dori extension and the overlay.
  const out = r.stdout.toString();
  expect(r.exitCode).toBe(0);
  expect(out).toContain(
    `OMP_ARGS --no-extensions -e ${join(src, "extension")} --config ${join(side, "omp.yml")} --dori-role dori`,
  );
  expect(out).toContain(`OMP_ENV ${join(side, "config.json")} omp-dori`);
  // And the overlay hides the other Dori's skill and adds the side skill directory.
  const overlay = readFileSync(join(side, "omp.yml"), "utf8");
  expect(overlay).toContain("ignoredSkills:\n    - dori-mode\n");
  expect(overlay).toContain(`customDirectories:\n    - ${JSON.stringify(join(side, "skills"))}\n`);
});

test("a later run of a side install stays a side install without asking", () => {
  // Given a side install next to another Dori.
  const { home, install } = setup();
  withOtherDori(home);
  expect(install({ DORI_INSTALL_MODE: "side" }).code).toBe(0);
  // When install.sh runs again with no mode and no terminal; then it succeeds and the other Dori stays unchanged.
  const r = install();
  expect(r.code).toBe(0);
  expect(r.out).toContain("next to the Dori");
  expectOtherDoriUntouched(home);
});

test("primary over another Dori keeps its config and link targets, then takes ~/.dori", () => {
  // Given another Dori's ~/.dori.
  const { home, src, install } = setup();
  withOtherDori(home);
  // When install.sh runs with DORI_INSTALL_MODE=primary.
  expect(install({ DORI_INSTALL_MODE: "primary" }).code).toBe(0);
  // Then a backup holds the previous config and link targets, and the dori-mode link now points at omp-dori.
  const backups = readdirSync(join(home, ".dori/backups"));
  expect(backups).toHaveLength(1);
  const backup = join(home, ".dori/backups", backups[0] ?? "");
  expect(readFileSync(join(backup, "config.json"), "utf8")).toBe(OTHER_CONFIG);
  expect(readFileSync(join(backup, "links.txt"), "utf8")).toContain("-> /elsewhere/dori-mode");
  expect(readlinkSync(join(home, "skills/dori-mode"))).toBe(join(src, "skills/dori-mode"));
  expect(readFileSync(join(home, ".dori/.omp-dori"), "utf8")).toBe("primary\n");
});

test("primary refuses a ~/.dori/src that is another repository and changes nothing", () => {
  // Given another Dori whose ~/.dori/src checks out a different repository.
  const { root, home, install } = setup();
  withOtherDori(home);
  const other = join(home, ".dori/src");
  mkdirSync(other);
  git(other, "init", "-q", "-b", "main");
  git(other, "remote", "add", "origin", "https://github.com/example/dori-superagent.git");
  // When install.sh would use it as the omp-dori checkout.
  const r = install({ DORI_INSTALL_MODE: "primary", DORI_SRC: "" });
  // Then it stops before any backup, link or package install, and points at the side mode.
  expect(r.code).toBe(1);
  expect(r.err).toContain("dori-superagent");
  expect(r.err).toContain("DORI_INSTALL_MODE=side");
  expect(readdirSync(join(home, ".dori")).sort()).toEqual(["config.json", "src"]);
  expect(readlinkSync(join(home, "skills/dori-mode"))).toBe("/elsewhere/dori-mode");
  expect(existsSync(join(root, "bun.log"))).toBe(false);
});

test("in a terminal the owner is asked, and the default answer installs next to the other Dori", async () => {
  // Given another Dori's ~/.dori and install.sh in a terminal with no mode.
  const { home, env } = setup();
  withOtherDori(home);
  let screen = "";
  let answered = false;
  const proc = Bun.spawn([script], {
    env: env(),
    timeout: 60_000,
    terminal: {
      cols: 120,
      rows: 40,
      data(term, chunk) {
        screen += new TextDecoder().decode(chunk);
        // When the question appears, the owner presses Enter.
        if (!answered && screen.includes("Install as 1 or 2")) {
          answered = true;
          term.write("\n");
        }
      },
    },
  });
  // Then the install finishes as a side install and the other Dori is unchanged.
  expect(await proc.exited).toBe(0);
  expect(screen).toContain("belongs to another Dori installation");
  expect(readFileSync(join(home, ".omp-dori/.omp-dori"), "utf8")).toBe("side\n");
  expectOtherDoriUntouched(home);
});
