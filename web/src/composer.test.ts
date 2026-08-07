import { describe, expect, it } from "vitest";
import { expandSlashCommandValue } from "./slashCommands";
import { submissionRoute } from "./gui";

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

  it("never drops a submission when the running surface has no steer path", () => {
    expect(submissionRoute(false, false)).toBe("send");
    expect(submissionRoute(true, true)).toBe("steer");
    expect(submissionRoute(true, false)).toBe("blocked");
  });
});
