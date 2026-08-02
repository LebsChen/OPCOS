import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { StrictMode, useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import "./style.css";

type OpcosEvent = { kind: string; message: string };

function App() {
  const [message, setMessage] = useState("Starting OPCOS…");

  useEffect(() => {
    let active = true;
    const subscription = listen<OpcosEvent>("opcos://event", (event) => {
      if (active) setMessage(event.payload.message);
    });
    void subscription.then((unlisten) => {
      if (!active) unlisten();
    });
    void invoke<string>("ping").then((value) => {
      if (active) setMessage(`Backend: ${value}`);
    });
    return () => {
      active = false;
      void subscription.then((unlisten) => unlisten());
    };
  }, []);

  return (
    <main>
      <h1>OPCOS</h1>
      <p>Local Devin client</p>
      <output>{message}</output>
    </main>
  );
}

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
