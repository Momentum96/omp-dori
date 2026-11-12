import type { Distribution } from "./release-build.ts";
import { RELEASE_REPOSITORY, releaseGate } from "./release-gate.ts";
import { ReleaseJournal } from "./release-journal.ts";
import { githubApi, uploadAsset } from "./release-process.ts";
import { assets, findRelease, marker, notes, type Release, validateRelease } from "./release-remote.ts";

const PREFIX = `repos/${RELEASE_REPOSITORY}`;

/** The draft body also retains upload/publish intents across fresh workflow runners. */
export async function publishRelease(directory: string, distribution: Distribution): Promise<void> {
  const { manifest, assets: expected } = distribution;
  await releaseGate(manifest);
  const names = Object.keys(expected).sort();
  const body = notes(manifest);
  const journal = new ReleaseJournal(directory, manifest);
  let release = await findRelease(manifest);
  let prepared: string | undefined;
  const readback = async (): Promise<Release | undefined> => {
    const current = await findRelease(manifest);
    if (current) validateRelease(current, manifest, body, names);
    release = current;
    return current;
  };
  if (release) validateRelease(release, manifest, body, names);
  if (journal.pending) {
    const pending = journal.pending;
    if (!release) throw new Error(`Unresolved ${pending}; retain the journal and inspect GitHub before any replay`);
    if (pending === "create") journal.confirmed();
    else if (pending.startsWith("prepare:")) {
      const operation = pending.slice("prepare:".length);
      if (release["draft"] !== true || release["body"] !== marker(body, operation)) {
        throw new Error("Draft intent write is ambiguous; retained without replay");
      }
      prepared = operation;
      journal.confirmed();
    } else if (pending.startsWith("upload:")) {
      if (!(await assets(release, expected)).has(pending.slice("upload:".length))) {
        throw new Error("Asset upload is ambiguous; retained without replay");
      }
      journal.confirmed();
    } else if (pending === "publish" && release["draft"] === false) journal.confirmed();
    else throw new Error(`Unresolved publication intent: ${pending}; inspect GitHub before any replay`);
  }
  if (release?.["draft"] === false) {
    if ((await assets(release, expected)).size !== names.length)
      throw new Error("Published release assets are incomplete");
    return;
  }
  if (!release) {
    if (Number(process.env["GITHUB_RUN_ATTEMPT"] ?? "1") > 1) {
      throw new Error(
        "Fresh workflow retry has no durable draft; use the retained local journal after inspecting GitHub",
      );
    }
    await journal.effect(
      "create",
      () =>
        githubApi(`${PREFIX}/releases`, "POST", {
          tag_name: manifest.tag,
          target_commitish: manifest.commit,
          name: `OMP-Dori ${manifest.tag}`,
          body,
          draft: true,
          prerelease: manifest.prerelease,
          make_latest: "false",
        }),
      async () => (await readback())?.["draft"] === true,
    );
  }
  const prepare = async (operation: string): Promise<void> => {
    if (!release || release["draft"] !== true) throw new Error("Release draft changed before publication intent");
    if (release["body"] === marker(body, operation)) {
      if (prepared !== operation) throw new Error(`Remote ${operation} intent is ambiguous; retained without replay`);
      return;
    }
    await journal.effect(
      `prepare:${operation}`,
      () =>
        githubApi(`${PREFIX}/releases/${release!["id"]}`, "PATCH", {
          body: marker(body, operation),
        }),
      async () => {
        const current = await readback();
        return current?.["draft"] === true && current["body"] === marker(body, operation);
      },
    );
    prepared = operation;
  };
  for (const name of names) {
    if (!release) throw new Error("Release draft is missing");
    const current = await assets(release, expected);
    if (current.has(name)) continue;
    await prepare(`upload:${name}`);
    await journal.effect(
      `upload:${name}`,
      () => uploadAsset(RELEASE_REPOSITORY, Number(release!["id"]), name, expected[name]!),
      async () => {
        const current = await readback();
        return current?.["draft"] === true && (await assets(current, expected)).has(name);
      },
    );
    prepared = undefined;
  }
  if (!release || (await assets(release, expected)).size !== names.length)
    throw new Error("Draft assets are incomplete");
  await releaseGate(manifest);
  await prepare("publish");
  if (!(await readback()) || release!["draft"] !== true || (await assets(release!, expected)).size !== names.length) {
    throw new Error("Verified draft changed before publication");
  }
  await journal.effect(
    "publish",
    () =>
      githubApi(`${PREFIX}/releases/${release!["id"]}`, "PATCH", {
        draft: false,
        prerelease: manifest.prerelease,
        body,
        make_latest: manifest.prerelease ? "false" : "true",
      }),
    async () => {
      const published = await readback();
      return published?.["draft"] === false && (await assets(published, expected)).size === names.length;
    },
  );
}
