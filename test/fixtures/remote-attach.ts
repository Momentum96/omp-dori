/** Open a separate observer PTY: never type test input into the OMP terminal itself. */
export async function observeRemotePty(argv: string[], env: Record<string, string | undefined>, marker: string) {
  const utf8 = new TextDecoder();
  let output = "";
  let observed = false;
  await using terminal = new Bun.Terminal({
    cols: 150,
    rows: 42,
    data(tty, bytes) {
      output = (output + utf8.decode(bytes, { stream: true })).slice(-64 * 1024);
      if (observed || !output.includes(marker)) return;
      observed = true;
      // hc handles ctrl+\ at the observer, before forwarding even in --observe.
      tty.write("\x1c");
    },
  });
  const view = Bun.spawn(argv, { env, terminal, timeout: 12_000 });
  try {
    if ((await view.exited) !== 0 || !observed)
      throw new Error(`hc observer did not display the OMP marker and detach:\n${output.slice(-2000)}`);
  } finally {
    view.kill();
    await view.exited;
  }
}
