import { randomUUID } from "node:crypto";
import {
  closeSync,
  constants,
  fstatSync,
  fsyncSync,
  lstatSync,
  mkdirSync,
  openSync,
  readFileSync,
  renameSync,
  type Stats,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { join } from "node:path";
import type { RouteState } from "./discord-inbound-routing.ts";
import { MessengerError } from "./http.ts";

/**
 * One channel's committed cursor, plus the mention-routing state that advances with it. `loadRoute` returns the stored
 * routing state unparsed (the routing gate parses it); `save` without a routing state stores none.
 */
export type DiscordInboundStore = {
  load(): string | undefined;
  loadRoute(): unknown;
  save(cursor: string, route?: RouteState): void;
};

export const isDiscordSnowflake = (value: unknown): value is string =>
  typeof value === "string" && /^[1-9]\d{0,19}$/.test(value) && BigInt(value) <= 18_446_744_073_709_551_615n;

export const discordInboundEnv = (name: string, value: string | undefined): string => {
  const id = value?.trim();
  if (!isDiscordSnowflake(id)) throw new MessengerError(`${name} must be a positive decimal Discord ID`, 0);
  return id;
};

const privateFile = (stat: Stats) => {
  if (!stat.isFile() || stat.uid !== process.getuid?.() || (stat.mode & 0o077) !== 0 || stat.nlink !== 1) {
    throw new MessengerError("Discord inbound cursor must be a private regular file owned by this user", 0);
  }
};

export const fileDiscordInboundStore = (stateDir: string, channelId: string): DiscordInboundStore => {
  if (!isDiscordSnowflake(channelId)) throw new MessengerError("Discord inbound channel ID is invalid", 0);
  const dir = join(stateDir, "discord-inbound");
  const path = join(dir, `${channelId}.json`);
  const directory = () => {
    try {
      mkdirSync(dir, { recursive: true, mode: 0o700 });
      const stat = lstatSync(dir);
      if (!stat.isDirectory() || stat.uid !== process.getuid?.() || (stat.mode & 0o077) !== 0) throw new Error();
    } catch {
      throw new MessengerError("Discord inbound cursor directory must be private (0700) and owned by this user", 0);
    }
  };
  directory();
  const read = (): { cursor: string; route?: unknown } | undefined => {
    directory();
    let fd: number;
    try {
      fd = openSync(path, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
    } catch (error) {
      if (error instanceof Error && "code" in error && error.code === "ENOENT") return undefined;
      throw new MessengerError(
        "Discord inbound cursor is unreadable; use a private regular file owned by this user",
        0,
      );
    }
    try {
      privateFile(fstatSync(fd));
      const state = JSON.parse(readFileSync(fd, "utf8")) as {
        version?: number;
        channelId?: string;
        cursor?: string;
        route?: unknown;
      };
      if (
        !state ||
        state.version !== 1 ||
        state.channelId !== channelId ||
        (state.cursor !== "0" && !isDiscordSnowflake(state.cursor))
      ) {
        throw new MessengerError("Discord inbound cursor state is invalid; repair it before restarting intake", 0);
      }
      return { cursor: state.cursor, route: state.route };
    } catch (error) {
      if (error instanceof MessengerError) throw error;
      throw new MessengerError("Discord inbound cursor state is invalid; repair it before restarting intake", 0);
    } finally {
      closeSync(fd);
    }
  };
  return {
    load: () => read()?.cursor,
    loadRoute: () => read()?.route,
    save(cursor, route) {
      if (cursor !== "0" && !isDiscordSnowflake(cursor))
        throw new MessengerError("Discord inbound cursor is invalid", 0);
      directory();
      const existing = lstatSync(path, { throwIfNoEntry: false });
      if (existing) privateFile(existing);
      const temporary = join(dir, `.${channelId}.${process.pid}.${randomUUID()}.tmp`);
      let fd: number | undefined;
      let created = false;
      try {
        fd = openSync(
          temporary,
          constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | constants.O_NOFOLLOW,
          0o600,
        );
        created = true;
        writeFileSync(fd, `${JSON.stringify({ version: 1, channelId, cursor, ...(route ? { route } : {}) })}\n`);
        fsyncSync(fd);
        closeSync(fd);
        fd = undefined;
        renameSync(temporary, path);
        created = false;
        const parent = openSync(dir, constants.O_RDONLY | constants.O_DIRECTORY | constants.O_NOFOLLOW);
        try {
          fsyncSync(parent);
        } finally {
          closeSync(parent);
        }
      } catch {
        throw new MessengerError(
          "Discord inbound cursor could not be saved; check state directory ownership and space",
          0,
        );
      } finally {
        if (fd !== undefined) closeSync(fd);
        if (created) unlinkSync(temporary);
      }
    },
  };
};
