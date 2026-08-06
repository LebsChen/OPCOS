import { writeFile } from "node:fs/promises";

const cdpUrl = process.env.CDP_URL || "http://localhost:29229";
const pageUrl = process.env.PAGE_URL || "http://127.0.0.1:1420/";
const expected = process.env.EXPECT_TEXT || "";
const clickText = process.env.CLICK_TEXT || "";
const output = process.env.OUTPUT || "/tmp/opcos-page.png";

const target = await fetch(
  `${cdpUrl}/json/new?${encodeURIComponent(pageUrl)}`,
  { method: "PUT" },
).then((response) => response.json());
const socket = new WebSocket(target.webSocketDebuggerUrl);
let nextId = 0;
const pending = new Map();
socket.addEventListener("message", ({ data }) => {
  const message = JSON.parse(data);
  const resolver = pending.get(message.id);
  if (resolver) {
    pending.delete(message.id);
    resolver(message);
  }
});
await new Promise((resolve, reject) => {
  socket.addEventListener("open", resolve, { once: true });
  socket.addEventListener("error", () => reject(new Error("CDP websocket error")), {
    once: true,
  });
});
function command(method, params = {}) {
  const id = ++nextId;
  socket.send(JSON.stringify({ id, method, params }));
  return new Promise((resolve) => pending.set(id, resolve));
}
await command("Runtime.enable");
await command("Page.enable");
await command("Page.reload", { ignoreCache: true });
await new Promise((resolve) => setTimeout(resolve, 1000));
for (const targetText of clickText.split(",").map((value) => value.trim()).filter(Boolean)) {
  await command("Runtime.evaluate", {
    expression: `(() => {
      const target = ${JSON.stringify(targetText)};
      const node = target === "account"
        ? document.querySelector('[data-testid="account-row"]')
        : [...document.querySelectorAll("button,[role=tab],a")].find(
            (item) => item.textContent?.trim().includes(target),
          );
      node?.click();
      return Boolean(node);
    })()`,
    returnByValue: true,
  });
  await new Promise((resolve) => setTimeout(resolve, 300));
}
const state = await command("Runtime.evaluate", {
  expression: `JSON.stringify({
    text: document.body?.innerText || "",
    root: document.getElementById("root")?.innerHTML.length || 0,
  })`,
  returnByValue: true,
});
const page = JSON.parse(state.result?.result?.value || '{"text":"","root":0}');
const matched = expected
  .split("|")
  .map((value) => value.trim())
  .filter(Boolean)
  .every((value) => page.text.includes(value));
console.log(`target=${target.url}`);
console.log(`expected_text=${expected}`);
console.log(`matched=${matched}`);
console.log(`root_inner_html_length=${page.root}`);
if (!matched || page.root <= 0) {
  socket.close();
  process.exitCode = 1;
} else {
  const screenshot = await command("Page.captureScreenshot", {
    format: "png",
    captureBeyondViewport: true,
  });
  await writeFile(output, Buffer.from(screenshot.result.data, "base64"));
  console.log(`screenshot=${output}`);
  socket.close();
}
