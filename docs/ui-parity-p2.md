# P2 UI parity — session surfaces

This pass returns the session page's three primary surfaces to the OpenWorker
reference implementation. The visual source is preserved in the component
structure and class names; OPCOS-specific behavior is limited to invoke/event
adapters and durable session data.

| OPCOS file | Reference file and lines | Treatment |
| --- | --- | --- |
| `web/src/App.tsx:4759-4820` | `/home/ubuntu/repos/openworker/surfaces/gui/src/App.tsx:1365-1442` | Adapted port: OpenWorker topbar geometry, title/subtitle layout, and icon controls; OPCOS facts, secret backend badge, and session-panel invoke retained. |
| `web/src/components/Transcript.tsx:1-465` | `/home/ubuntu/repos/openworker/surfaces/gui/src/components/Transcript.tsx:1-465` | Adapted port: transcript grouping, thinking disclosure, bubbles, tool rows, raw details, and class names retained; OPCOS approval cards, item types, connector handling, retry, and invoke resolution adapted. |
| `web/src/components/Composer.tsx:1-1159` | `/home/ubuntu/repos/openworker/surfaces/gui/src/components/Composer.tsx:1-813` | Adapted port: composer shell, textarea, attachment row, mode/model controls, and class names retained; OPCOS invoke-backed send/interrupt, harness selector, assets, secrets, and host attachments adapted. |

No new visual vocabulary is introduced for these three surfaces. OPCOS-only
controls remain in the corresponding OpenWorker-style rows rather than adding
new layout primitives.
