# 0004 Native OpenAI-compatible harness

Custom (OpenAI Compatible) runs directly in `botd` over streaming Chat Completions. This lets a configured Bot use an arbitrary model identifier without requiring another agent CLI. The adapter emits the existing run events, so the same per-Bot queue, chat, schedules, stop handling, and memory files apply.

Keep endpoint configuration at server startup and pass only the model override through the existing Bot settings. Load optional bearer authentication from a private file, with redirects and automatic retries disabled. This keeps credentials out of the Bot API, SQLite settings, tool arguments, and provider errors.

Store a bounded set of completed custom turns alongside each Bot's memory. Expose a small set of named functions and reuse `botctl` for routines and peer messages. Text-only endpoints can omit tools entirely.
