# P2 UI parity — session surfaces

This pass returns the session page's three primary surfaces to the OpenWorker
reference implementation. The visual source is preserved in the component
structure and class names; OPCOS-specific behavior is limited to invoke/event
adapters and durable session data.

| OPCOS file | Reference file and lines | Treatment |
| --- | --- | --- |
| `web/src/App.tsx:4759-4820` | `/home/ubuntu/repos/openworker/surfaces/gui/src/App.tsx:1365-1442` | Adapted port: OpenWorker topbar geometry, title/subtitle layout, and icon controls; OPCOS facts, secret backend badge, and session-panel invoke retained. |
| `web/src/components/Transcript.tsx:1-465` | `/home/ubuntu/repos/openworker/surfaces/gui/src/components/Transcript.tsx:1-465` | Existing M9 OpenWorker port; no code change in this pass. Its OPCOS approval/item/invoke adaptations already existed and remain unchanged. |
| `web/src/components/Composer.tsx:1-1159` | `/home/ubuntu/repos/openworker/surfaces/gui/src/components/Composer.tsx:1-813` | Existing M9 OpenWorker port; no code change in this pass. Its OPCOS invoke, harness, assets, secrets, and attachment adaptations already existed and remain unchanged. |

No new visual vocabulary is introduced for these three surfaces. OPCOS-only
controls remain in the corresponding OpenWorker-style rows rather than adding
new layout primitives.

## Follow-up parity pass — Settings and Activity

| OPCOS file | Reference file and lines | Treatment |
| --- | --- | --- |
| `web/src/components/SettingsView.tsx:1-79` | `/home/ubuntu/repos/openworker/surfaces/gui/src/components/SettingsView.tsx:85-123` | Adapted shell: OpenWorker sub-navigation, centered `max-w-3xl` content region, spacing, and active-row classes retained. |
| `web/src/App.tsx:892-2770` (Settings body) | `/home/ubuntu/repos/openworker/surfaces/gui/src/components/SettingsView.tsx:125-376`; `/home/ubuntu/repos/Cloud-Dev/src/components/SettingsView.tsx:578-771` | Adapted OPCOS body: the references provide the card/row vocabulary, while Rules, Knowledge, Playbook, Skill, MCP, Secrets, and Blueprint are OPCOS configuration-object surfaces with no one-to-one reference component. |
| `web/src/App.tsx:2773-3450` (Activity) | `/home/ubuntu/repos/openworker/surfaces/gui/src/components/AuditView.tsx:20-64`; `/home/ubuntu/repos/Cloud-Dev/src/components/RemotePanes.tsx:496-526` | Adapted shell/timeline: centered page shell and audit-card treatment follow OpenWorker; worklog timeline vocabulary follows Cloud-Dev. Coordination board commands and fields are OPCOS-only and are listed separately rather than claimed as copied. |

Unmatched source items are limited to the OPCOS-specific configuration and
coordination data models above; no new visual system was introduced for them.
