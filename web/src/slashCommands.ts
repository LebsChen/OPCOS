export function expandSlashCommandValue(
  value: string,
  commands: Array<{
    name: string;
    body: string;
    execution?: string;
  }>,
): string {
  const match = value.trimStart().match(/^(\/\S+)(?:\s+([\s\S]*))?$/);
  if (!match) return value;
  const command = commands.find((item) => item.name === match[1]);
  if (!command || command.execution === "action") return value;
  return match[2]?.trim()
    ? `${command.body}\n\n${match[2].trim()}`
    : command.body;
}
