# 用户可见文案与后端消息约定

## 18.1 Structured UI message

后端用户可见消息使用 `structured_ui_message(code, params)`，实现位于
`src-tauri/src/main.rs`，engine 侧也提供同样的结构化消息约定：

- `params` 为空对象时，发出的文本只有 code，例如 `turn_setup_started`；
- `params` 非空时，发出 `code` 加一个 JSON 参数对象，例如
  `host_unavailable {"detail":"..."}`；
- code 是稳定的 snake_case 标识；provider 名、host 详情、路径、model ID 和
  error detail 等原始值仍是参数或 raw detail，不得把它们变成翻译 key。

前端 `web/src/i18n.ts` 的 `translateBackendError()` 只翻译已知 structured code
和已登记的旧字符串；未知或格式错误的后端文本原样返回。因此新增用户可见 code
必须同时加入该文件的中文、英文 dictionary，并补相应回归测试。

## 18.2 Localization boundary

前端支持中英文切换。正常中文 session 不得出现硬编码英文用户文案；后端只能发
structured code 和参数，前端负责把已知 code 映射到当前 locale。参数中的 provider
名、host 详情、路径、model ID 和错误细节保持原文，不作为 UI dictionary 的 key。

现有回归扫描会检查 `translate("literal")`，并对 `translate(variable)` 做有限
分析；动态拼接或其它间接 translation call 可能绕过该扫描，这是已知盲区。新增或
修改 UI 文案仍需在真实调用路径检查中英文显示。
