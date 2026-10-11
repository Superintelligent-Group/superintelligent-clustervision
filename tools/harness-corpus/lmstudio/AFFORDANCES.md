# LM Studio (store as of app 0.4.21): affordances and where they persist

From `crates/cv-core/tests/corpus/lmstudio/0.4.21+2/conversations.shape.txt` and cv's adapter
(`harness/lmstudio.rs`). Nothing is exercised by a generated corpus (see README.md).

| affordance | record | persists? | cv IR today |
|---|---|---|---|
| regenerations / branches | `messages[].versions[]` + `currentlySelected` | yes | the selected version only |
| reasoning | `steps[].style.type = thinking` content blocks | yes | Block::Thinking |
| tool calls | no first-class record; gpt-oss channel markers inline in text; `plugins`, `pluginConfigs`, `disabledPluginTools` | text only | kept as text |
| attachments | `content[].type = file` (`fileIdentifier`, `fileType = image`), `preprocessed`, `clientInputFiles`, `looseFiles` | references; bytes under `.internal/files` | Block::Image / Block::File |
| generation stats | `steps[].genInfo.stats` (tokens, tps, `stopReason = eosFound|userStopped`) | yes | Usage, model |
| model + load config | `lastUsedModel`, `genInfo.loadModelConfig`, `predictionConfig` | yes | model; configs not mapped |
| errors / cancels | `steps[].type = status`, `statusState.status = canceled|error` | yes | not mapped (status steps) |
| system prompt / preset | `systemPrompt`, `preset`, `perChatPredictionConfig` | yes | Session.system_prompt |
| edits | `versions[].edited`, `steps[].edited` | yes | not mapped |
| notes / pinning | `notes`, `pinned` | yes | not mapped |
| draft input | `clientInput`, `clientInputDocument` | yes | not mapped |
