import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

export function remoteTestModel(dir: string, model: string, prompts: readonly string[], response: string) {
  let submissions = 0;
  const server = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    async fetch(request) {
      const body = await request.text();
      if (!prompts.some((prompt) => body.includes(prompt)))
        return new Response("Unexpected test request", { status: 400 });
      submissions++;
      const chunk = {
        id: "remote-completion",
        object: "chat.completion.chunk",
        created: 1,
        model,
        choices: [{ index: 0, delta: { role: "assistant", content: response }, finish_reason: "stop" }],
      };
      return new Response(`data: ${JSON.stringify(chunk)}\n\ndata: [DONE]\n\n`, {
        headers: { "content-type": "text/event-stream" },
      });
    },
  });
  mkdirSync(dir);
  writeFileSync(
    join(dir, "models.yml"),
    JSON.stringify({
      providers: {
        "cataly-local": {
          baseUrl: `${server.url.origin}/v1`,
          apiKey: "local-fixture-only",
          api: "openai-completions",
          models: [
            { id: model, name: model },
            { id: "alternate-fixture", name: "alternate" },
          ],
        },
      },
    }),
  );
  return { server, submissions: () => submissions };
}
