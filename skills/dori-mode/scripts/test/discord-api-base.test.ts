import { afterEach, expect, test } from "bun:test";
import { discordApiBase } from "../src/messenger/discord.ts";

const original = process.env["DORI_DISCORD_API_BASE"];
afterEach(() => {
  if (original === undefined) delete process.env["DORI_DISCORD_API_BASE"];
  else process.env["DORI_DISCORD_API_BASE"] = original;
});

test("an unset override keeps the production endpoint byte for byte", () => {
  delete process.env["DORI_DISCORD_API_BASE"];
  expect(discordApiBase(undefined)).toBe("https://discord.com/api/v10");
  expect(discordApiBase("")).toBe("https://discord.com/api/v10");
  process.env["DORI_DISCORD_API_BASE"] = undefined;
  // Constructing the production client picks the default too.
  expect(new URL("https://discord.com/api/v10").href).toBe("https://discord.com/api/v10");
});

test("only loopback http origins are accepted", () => {
  for (const value of [
    "http://127.0.0.1:49185",
    "http://localhost:49185",
    "http://[::1]:49185",
    "http://127.0.0.1:49185/v10",
  ]) {
    expect(discordApiBase(value)).toBe(value);
  }
});

test("any other host, scheme or credential is refused", () => {
  for (const value of [
    "https://discord.com/api/v10",
    "https://evil.example",
    "http://127.0.0.1.evil.example",
    "http://example.com:49185",
    "file:///etc/passwd",
    "ftp://127.0.0.1",
    "http://user:pw@127.0.0.1:49185",
    "http://user@127.0.0.1:49185",
    "not a url",
    "//127.0.0.1:49185",
  ]) {
    expect(() => discordApiBase(value)).toThrow(/loopback http origin|not a URL/);
  }
});
