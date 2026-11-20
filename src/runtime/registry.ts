import { HEADLESS_ADDRESS, type RuntimeKind, type SessionRef, type SessionRuntime } from "./types.ts";

const nonEmpty = (value: string | undefined): string | undefined =>
  value === undefined || value === "" ? undefined : value;

/**
 * The runtimes one process talks to: `default` is the selected kind (where new sessions open), `for(ref)` is the
 * runtime that owns a stored session. Each kind is built on first use and then reused.
 */
export class Runtimes {
  private readonly built = new Map<RuntimeKind, SessionRuntime>();

  /** `selected` may be lazy: a stored session's runtime (`for(ref)`) never needs the choice to be made. */
  constructor(
    private readonly selected: RuntimeKind | (() => RuntimeKind),
    private readonly make: (kind: RuntimeKind) => SessionRuntime,
  ) {}

  get default(): SessionRuntime {
    return this.of(typeof this.selected === "function" ? this.selected() : this.selected);
  }

  for(ref: SessionRef): SessionRuntime {
    return this.of(ref.runtime);
  }

  /** The kinds built so far, in first-use order, to enumerate sessions across every runtime in use. */
  kinds(): readonly RuntimeKind[] {
    return [...this.built.keys()];
  }

  /**
   * The caller's own session, independent of the selected terminal runtime. Native lane identity takes precedence
   * over inherited terminal IDs; otherwise `HC_TERMINAL_ID` then `HERDR_PANE_ID`. Builds no runtime.
   */
  selfAny(env: Readonly<Record<string, string | undefined>>): SessionRef | undefined {
    const headless = env["CATALY_HEADLESS_ID"];
    if (headless !== undefined && HEADLESS_ADDRESS.test(headless)) return { runtime: "headless", id: headless };
    const terminal = nonEmpty(env["HC_TERMINAL_ID"]);
    if (terminal !== undefined) return { runtime: "hc", id: terminal };
    const pane = nonEmpty(env["HERDR_PANE_ID"]);
    if (pane === undefined) return undefined;
    const workspace = nonEmpty(env["HERDR_WORKSPACE_ID"]);
    return { runtime: "herdr", id: pane, ...(workspace === undefined ? {} : { group: workspace }) };
  }

  private of(kind: RuntimeKind): SessionRuntime {
    let runtime = this.built.get(kind);
    if (runtime === undefined) {
      runtime = this.make(kind);
      this.built.set(kind, runtime);
    }
    return runtime;
  }
}
