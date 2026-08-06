const cdpUrl = process.env.CDP_URL || "http://localhost:29229";
const pageUrl = process.env.PAGE_URL || "http://127.0.0.1:1420/";

const target = await fetch(
  `${cdpUrl}/json/new?${encodeURIComponent(pageUrl)}`,
  { method: "PUT" },
).then(async (response) => {
  if (!response.ok) {
    throw new Error(`CDP target creation failed: ${response.status}`);
  }
  return response.json();
});

const socket = new WebSocket(target.webSocketDebuggerUrl);
let nextId = 0;
const pending = new Map();
const exceptions = [];
const errors404 = [];

socket.addEventListener("message", ({ data }) => {
  const message = JSON.parse(data);
  if (message.method === "Runtime.exceptionThrown") {
    const details = message.params.exceptionDetails;
    exceptions.push(
      details?.exception?.description ||
        details?.description ||
        details?.text ||
        "runtime exception",
    );
  }
  if (
    message.method === "Log.entryAdded" &&
    (message.params.entry.level === "error" ||
      message.params.entry.text.includes("404"))
  ) {
    errors404.push(
      `${message.params.entry.url || ""} ${message.params.entry.text}`,
    );
  }
  const resolver = pending.get(message.id);
  if (resolver) {
    pending.delete(message.id);
    resolver(message);
  }
});

await new Promise((resolve, reject) => {
  socket.addEventListener("open", resolve, { once: true });
  socket.addEventListener(
    "error",
    () => reject(new Error("CDP websocket error")),
    {
      once: true,
    },
  );
});

function command(method, params = {}) {
  const id = ++nextId;
  socket.send(JSON.stringify({ id, method, params }));
  return new Promise((resolve) => pending.set(id, resolve));
}

await command("Runtime.enable");
await command("Log.enable");
await command("Page.enable");
await command("Page.reload", { ignoreCache: true });
await new Promise((resolve) => setTimeout(resolve, 1200));
const evaluated = await command("Runtime.evaluate", {
  expression: "document.getElementById('root')?.innerHTML.length || 0",
  returnByValue: true,
});
const rootLength = evaluated.result?.result?.value || 0;

console.log(`target=${target.url}`);
console.log(`exceptions=${exceptions.length}`);
console.log(`errors_404=${errors404.length}`);
console.log(`root_inner_html_length=${rootLength}`);
if (exceptions.length)
  console.log(`exception_details=${exceptions.join(" | ")}`);
if (errors404.length) console.log(`error_details=${errors404.join(" | ")}`);

socket.close();
if (exceptions.length || errors404.length || rootLength <= 0) {
  process.exitCode = 1;
}
