import { WebSocketServer } from "ws";

const port = Number(process.env.NANOCODEX_MOCK_OPENAI_PORT ?? 8790);
const server = new WebSocketServer({ host: "127.0.0.1", port });
let nextResponse = 1;

server.on("connection", (socket) => {
  socket.on("message", (data) => {
    const request = JSON.parse(data.toString("utf8"));
    const encoded = JSON.stringify(request.input ?? []);
    const hasUserPrompt = encoded.includes("Reply with exactly EDGE_OK")
      || encoded.includes("What exact token did I ask you to return previously?");
    const response = {
      id: `mock-response-${nextResponse++}`,
      status: "completed",
      ...(hasUserPrompt ? {
        output: [{
          type: "message",
          role: "assistant",
          content: [{ type: "output_text", text: "EDGE_OK" }],
        }],
      } : {}),
      usage: null,
    };
    socket.send(JSON.stringify({ type: "response.completed", response }));
  });
});

await new Promise((resolve, reject) => {
  server.once("listening", resolve);
  server.once("error", reject);
});
console.log(`mock Responses WebSocket listening on ws://127.0.0.1:${port}`);

for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => {
    for (const socket of server.clients) socket.terminate();
    server.close(() => process.exit(0));
  });
}
