import { describe, expect, it } from "vitest";
import { expandSlashCommandValue } from "./slashCommands";

describe("slash command expansion", () => {
  const commands = [
    { name: "/compact", body: "Compact now.", execution: "action" },
    { name: "/review", body: "Review the change.", execution: "prompt" },
  ];

  it("leaves action commands intact for backend dispatch", () => {
    expect(expandSlashCommandValue("/compact", commands)).toBe("/compact");
    expect(expandSlashCommandValue("/compact now", commands)).toBe(
      "/compact now",
    );
  });

  it("expands prompt commands", () => {
    expect(expandSlashCommandValue("/review focus on tests", commands)).toBe(
      "Review the change.\n\nfocus on tests",
    );
  });
});
