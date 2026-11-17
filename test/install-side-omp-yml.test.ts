import { expect, test } from "bun:test";
import { mkdirSync, readFileSync, statSync, symlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { script, setup } from "./install-setup.ts";

// A side home next to another Dori, so the installer takes the side path that generates omp.yml.
function withOtherDori(home: string) {
  mkdirSync(join(home, ".dori"));
  writeFileSync(join(home, ".dori/config.json"), '{"owner":"another Dori"}\n');
  mkdirSync(join(home, "skills"));
  symlinkSync("/elsewhere/dori-mode", join(home, "skills/dori-mode"));
  mkdirSync(join(home, "global-bin"));
  symlinkSync("/elsewhere/cli.ts", join(home, "global-bin/dori"));
}

test("a fresh side install writes exactly the block this script generates, with no owner keys added", () => {
  // Given another Dori's ~/.dori and no omp.yml in the side home yet.
  const { home, install } = setup();
  withOtherDori(home);
  // When scripts/install-source.sh runs in side mode.
  const r = install({ DORI_INSTALL_MODE: "side" });
  // Then the file is the script's own generated block verbatim: with nothing to preserve, nothing is added.
  expect(r.code).toBe(0);
  const generated = readFileSync(script, "utf8").match(/cat > "\$base\/omp\.yml" <<EOF\n([\s\S]*?)\nEOF\n/)?.[1];
  expect(generated).toBeDefined();
  expect(readFileSync(join(home, ".omp-dori/omp.yml"), "utf8")).toBe(
    `${generated?.replace("$quoted_skills", JSON.stringify(join(home, ".omp-dori/skills")))}\n`,
  );
});

test("a reinstall refreshes the managed skills block and keeps every key the owner added", () => {
  // Given a side install whose owner then added roles, a thinking level and an unrelated skills key.
  const { home, install } = setup();
  withOtherDori(home);
  expect(install({ DORI_INSTALL_MODE: "side" }).code).toBe(0);
  const path = join(home, ".omp-dori/omp.yml");
  const owner = {
    update: { auto: false },
    modelRoles: { default: "anthropic/claude-opus-5-5:medium", task: "deploy/deepseek-v4.1-flash-1017:max" },
    defaultThinkingLevel: "auto",
  };
  const before = Bun.YAML.parse(readFileSync(path, "utf8")) as { skills: object };
  writeFileSync(path, Bun.YAML.stringify({ ...before, ...owner, skills: { ...before.skills, extra: ["x"] } }, null, 2));
  const mode = statSync(path).mode & 0o777;
  // When the same side install runs again.
  const r = install({ DORI_INSTALL_MODE: "side" });
  // Then the owner's keys survive, the unrelated skills key survives, and only the managed skills keys are refreshed.
  expect(r.code).toBe(0);
  expect(Bun.YAML.parse(readFileSync(path, "utf8"))).toEqual({
    skills: { extra: ["x"], ignoredSkills: ["dori-mode"], customDirectories: [join(home, ".omp-dori/skills")] },
    ...owner,
  });
  expect(statSync(path).mode & 0o777).toBe(mode);
});
