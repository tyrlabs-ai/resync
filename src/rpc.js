import { createConnection } from "node:net";
import { statePaths } from "./state.js";

export function rpc(request, socketPath = statePaths().socket) {
  return new Promise((resolve, reject) => {
    const socket = createConnection(socketPath);
    let buffered = "";
    socket.on("connect", () => socket.write(`${JSON.stringify(request)}\n`));
    socket.on("data", chunk => {
      buffered += chunk;
      const newline = buffered.indexOf("\n");
      if (newline < 0) return;
      const response = JSON.parse(buffered.slice(0, newline));
      socket.end();
      if (response.ok) resolve(response.result);
      else reject(new Error(response.error));
    });
    socket.on("error", error => reject(new Error(`daemon unavailable: ${error.message}; start it with \`resync daemon\``)));
  });
}
