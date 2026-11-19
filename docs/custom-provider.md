# Custom (OpenAI Compatible)

Run a Bot on a local or remote service that implements streaming `POST /chat/completions`. Configure one inference endpoint per `botd` instance. Each custom Bot can use the default model or specify another model ID served by that endpoint.

## Configure botd

Set these environment variables before starting `botd`. The example below uses the default LM Studio port. Replace `<windows-lan-ip>` and `<model-id>` with your values.

```sh
CUSTOM_OPENAI_BASE_URL='http://<windows-lan-ip>:1234/v1'
CUSTOM_OPENAI_MODEL='<model-id>'
```

Include the API prefix expected by the server, usually `/v1`. For an authenticated endpoint, set `CUSTOM_OPENAI_API_KEY_FILE` to the path of an owner-only file containing the bearer token. Use a file outside the checkout or under the ignored `secrets/` directory and set mode `600`. Restart `botd` after rotating the key. Use HTTPS for remote services and trust the endpoint with conversation and tool data. SuperBot's CLI harnesses already share the server account, so run untrusted local workloads under a separate account or instance.

| Setting | Default | Purpose |
| --- | --- | --- |
| `CUSTOM_OPENAI_CONNECT_TIMEOUT_SECONDS` | `10` | Time to establish a connection |
| `CUSTOM_OPENAI_REQUEST_TIMEOUT_SECONDS` | `180` | Time for each inference request, including the full stream |
| `CUSTOM_OPENAI_TOOLS` | `true` | Set to `false` for text-only models or servers that reject function definitions |

Timeout values range from 1 to 3600 seconds. The existing `RUN_TIMEOUT_MINUTES` also applies to the entire Bot run, including tool calls.

## Try it with LM Studio

1. On the Windows PC, manually load a model in LM Studio. In Developer, start the API server and enable **Serve on Local Network**. Copy the base URL from LM Studio and confirm that the chosen port is reachable from the machine running `botd`. [Serve on Local Network](https://lmstudio.ai/docs/developer/core/server/serve-on-network)
2. Copy the exact model identifier shown by LM Studio or returned by `GET /v1/models`. For an endpoint without authentication, use `curl -fsS 'http://<windows-lan-ip>:1234/v1/models'` from the `botd` host. For token authentication, configure the same token in `CUSTOM_OPENAI_API_KEY_FILE`. [OpenAI compatibility](https://lmstudio.ai/docs/developer/openai-compat)
3. Build with `cargo build --workspace --locked --release`. Set the custom variables in the test instance's ignored `.env` and launch `botd` following the [backend setup](backend.md#run-it). Use a separate `PORT`, `DATA_DIR`, and `WORKSPACE_DIR` when testing alongside another instance.
4. Create a Bot and select **Custom (OpenAI Compatible)** under **Runs on**. To override the default model, enter the exact model ID in the model field above chat and press Enter. Clearing the field restores the server default.
5. Send a message, then ask a follow-up that refers to it. For a tool-capable model, give the Bot a job and ask it to save that job to memory. After restarting the test instance, ask it what its job is.

## Capabilities

- Each request includes the Bot's identity and current memory, shared user facts, and recent completed conversation turns. The custom provider retains up to 12 completed turns within a 256 KiB text budget. Small model context windows may need shorter messages.
- Tool-capable models can update their own memory, list Bots, ask another Bot, and list or add routines. Tool messages are used within the current turn. Plain chat remains available with `CUSTOM_OPENAI_TOOLS=false`, with existing memory supplied as context.
- The custom provider supports streamed text and OpenAI-style function calls. It has no shell or desktop tools, dedicated reasoning controls, or structured response mode. A model that cannot reliably emit function calls works best with tools disabled.
- Errors appear in the Bot's chat. Stopped or failed requests do not retry on another provider.

Changing the configured endpoint sends subsequent custom Bot requests, including their recent conversation and memory, to the new endpoint.
